//! Blob byte storage.
//!
//! `BlobRepo` tracks metadata; `BlobStore` owns the actual bytes. P2 ships a
//! local-filesystem backend keyed by `{prefix}/{blob_id}`; an S3/MinIO backend
//! can swap in later by implementing the same trait.

use std::path::{Path, PathBuf};
use std::pin::Pin;

use aero_common::BlobId;
use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;

#[derive(Debug, thiserror::Error)]
pub enum BlobStoreError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("not found")]
    NotFound,
    /// Misconfiguration detected at startup (e.g. `AERO_BLOB_BACKEND=s3` with an
    /// incomplete S3 config). Surfaced fail-loud rather than silently degrading.
    #[error("blob store config: {0}")]
    Config(String),
}

/// Inclusive byte range requested from a blob backend.
///
/// HTTP range parsing and satisfiability checks live at the server edge. This
/// type carries the already-resolved range down to storage so remote backends
/// can avoid fetching bytes the client did not request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobRange {
    start: u64,
    end: u64,
}

impl BlobRange {
    /// Construct an inclusive range, returning `None` when `end < start`.
    #[must_use]
    pub const fn new(start: u64, end: u64) -> Option<Self> {
        if end < start {
            None
        } else {
            Some(Self { start, end })
        }
    }

    #[must_use]
    pub const fn start(self) -> u64 {
        self.start
    }

    #[must_use]
    pub const fn end(self) -> u64 {
        self.end
    }

    #[must_use]
    pub const fn byte_len(self) -> u64 {
        self.end.saturating_sub(self.start).saturating_add(1)
    }

    #[must_use]
    pub fn http_value(self) -> String {
        format!("bytes={}-{}", self.start, self.end)
    }
}

/// Type-erased byte stream returned by [`BlobStore::get_stream`].
pub type BlobStream = Pin<Box<dyn Stream<Item = Result<Bytes, BlobStoreError>> + Send + 'static>>;

#[async_trait]
pub trait BlobStore: Send + Sync + 'static {
    /// Returns the storage key for the blob.
    async fn put(&self, id: BlobId, bytes: Bytes) -> Result<String, BlobStoreError>;
    /// Returns the entire blob contents.
    async fn get(&self, id: BlobId) -> Result<Bytes, BlobStoreError>;
    /// Streams the complete blob or an inclusive byte range.
    ///
    /// The default implementation preserves compatibility for small fake/test
    /// stores that only implement [`Self::get`]. Production backends override
    /// it so download responses do not buffer the whole object in memory.
    async fn get_stream(
        &self,
        id: BlobId,
        range: Option<BlobRange>,
    ) -> Result<BlobStream, BlobStoreError> {
        let bytes = self.get(id).await?;
        let bytes = if let Some(range) = range {
            let start = usize::try_from(range.start()).map_err(invalid_range)?;
            let end = usize::try_from(range.end())
                .map_err(invalid_range)?
                .checked_add(1)
                .ok_or_else(|| invalid_range("range end overflow"))?;
            if start >= bytes.len() || end > bytes.len() {
                return Err(invalid_range("range exceeds blob length"));
            }
            bytes.slice(start..end)
        } else {
            bytes
        };
        Ok(Box::pin(futures::stream::once(async move { Ok(bytes) })))
    }
    /// Deletes the storage object. `NotFound` is treated as success (idempotent).
    async fn delete(&self, id: BlobId) -> Result<(), BlobStoreError>;
    /// Returns the storage key without reading anything.
    fn key_for(&self, id: BlobId) -> String;

    /// Cheap reachability probe for readiness gating (ROADMAP 方向三). Defaults to
    /// `Ok` — a local store backed by a directory created at startup is always
    /// reachable. Remote backends (S3) override this with a lightweight check so
    /// an outage pulls the pod from the load-balancer rotation instead of leaving
    /// it "ready" while every attachment request 5xxs.
    async fn health_check(&self) -> Result<(), BlobStoreError> {
        Ok(())
    }
}

/// Local-filesystem `BlobStore`.
#[derive(Debug, Clone)]
pub struct LocalFsBlobStore {
    root: PathBuf,
}

impl LocalFsBlobStore {
    pub fn new<P: AsRef<Path>>(root: P) -> std::io::Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn path_for(&self, id: BlobId) -> PathBuf {
        self.root.join(id.to_string())
    }
}

#[async_trait]
impl BlobStore for LocalFsBlobStore {
    async fn put(&self, id: BlobId, bytes: Bytes) -> Result<String, BlobStoreError> {
        let path = self.path_for(id);
        let mut file = tokio::fs::File::create(&path).await?;
        file.write_all(&bytes).await?;
        file.flush().await?;
        Ok(self.key_for(id))
    }

    async fn get(&self, id: BlobId) -> Result<Bytes, BlobStoreError> {
        let path = self.path_for(id);
        match tokio::fs::File::open(&path).await {
            Ok(mut f) => {
                let mut buf = Vec::new();
                f.read_to_end(&mut buf).await?;
                Ok(Bytes::from(buf))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(BlobStoreError::NotFound),
            Err(e) => Err(BlobStoreError::Io(e)),
        }
    }

    async fn get_stream(
        &self,
        id: BlobId,
        range: Option<BlobRange>,
    ) -> Result<BlobStream, BlobStoreError> {
        let path = self.path_for(id);
        let mut file = match tokio::fs::File::open(&path).await {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(BlobStoreError::NotFound);
            }
            Err(e) => return Err(BlobStoreError::Io(e)),
        };
        let limit = if let Some(range) = range {
            if range.end() >= file.metadata().await?.len() {
                return Err(invalid_range("range exceeds blob length"));
            }
            file.seek(std::io::SeekFrom::Start(range.start())).await?;
            range.byte_len()
        } else {
            u64::MAX
        };
        let stream = ReaderStream::with_capacity(file.take(limit), 64 * 1024)
            .map(|chunk| chunk.map_err(BlobStoreError::Io));
        Ok(Box::pin(stream))
    }

    async fn delete(&self, id: BlobId) -> Result<(), BlobStoreError> {
        let path = self.path_for(id);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(BlobStoreError::Io(e)),
        }
    }

    fn key_for(&self, id: BlobId) -> String {
        format!("local:{id}")
    }
}

fn invalid_range(error: impl std::fmt::Display) -> BlobStoreError {
    BlobStoreError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        error.to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::TryStreamExt;

    #[tokio::test]
    async fn local_fs_roundtrip() {
        let tmp = tempdir();
        let store = LocalFsBlobStore::new(&tmp).unwrap();
        let id = BlobId::new();
        let key = store.put(id, Bytes::from_static(b"hello")).await.unwrap();
        assert!(key.starts_with("local:"));
        let out = store.get(id).await.unwrap();
        assert_eq!(&out[..], b"hello");
    }

    #[tokio::test]
    async fn local_fs_health_check_is_ok() {
        // The local store is always reachable (dir created at construction), so
        // the default health_check reports healthy — readiness never fails on it.
        let tmp = tempdir();
        let store = LocalFsBlobStore::new(&tmp).unwrap();
        assert!(store.health_check().await.is_ok());
    }

    #[tokio::test]
    async fn local_fs_streams_full_blob_and_resolved_range() {
        let tmp = tempdir();
        let store = LocalFsBlobStore::new(&tmp).unwrap();
        let id = BlobId::new();
        store
            .put(id, Bytes::from_static(b"0123456789"))
            .await
            .unwrap();

        let full = store
            .get_stream(id, None)
            .await
            .unwrap()
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        assert_eq!(join_chunks(full), b"0123456789");

        let range = BlobRange::new(3, 6).unwrap();
        let partial = store
            .get_stream(id, Some(range))
            .await
            .unwrap()
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        assert_eq!(join_chunks(partial), b"3456");
    }

    #[tokio::test]
    async fn default_stream_seam_keeps_get_only_fake_stores_compatible() {
        struct Fake;

        #[async_trait]
        impl BlobStore for Fake {
            async fn put(&self, _id: BlobId, _bytes: Bytes) -> Result<String, BlobStoreError> {
                Ok("fake".into())
            }

            async fn get(&self, _id: BlobId) -> Result<Bytes, BlobStoreError> {
                Ok(Bytes::from_static(b"abcdefgh"))
            }

            async fn delete(&self, _id: BlobId) -> Result<(), BlobStoreError> {
                Ok(())
            }

            fn key_for(&self, _id: BlobId) -> String {
                "fake".into()
            }
        }

        let bytes = Fake
            .get_stream(BlobId::new(), BlobRange::new(2, 4))
            .await
            .unwrap()
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        assert_eq!(join_chunks(bytes), b"cde");
    }

    fn join_chunks(chunks: Vec<Bytes>) -> Vec<u8> {
        chunks
            .into_iter()
            .flat_map(|chunk| chunk.to_vec())
            .collect()
    }

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("aero-blob-test-{}", BlobId::new()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
