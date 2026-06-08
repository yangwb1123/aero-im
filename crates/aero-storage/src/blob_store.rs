//! Blob byte storage.
//!
//! `BlobRepo` tracks metadata; `BlobStore` owns the actual bytes. P2 ships a
//! local-filesystem backend keyed by `{prefix}/{blob_id}`; an S3/MinIO backend
//! can swap in later by implementing the same trait.

use std::path::{Path, PathBuf};

use aero_common::BlobId;
use async_trait::async_trait;
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, thiserror::Error)]
pub enum BlobStoreError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("not found")]
    NotFound,
}

#[async_trait]
pub trait BlobStore: Send + Sync + 'static {
    /// Returns the storage key for the blob.
    async fn put(&self, id: BlobId, bytes: Bytes) -> Result<String, BlobStoreError>;
    /// Returns the entire blob contents.
    async fn get(&self, id: BlobId) -> Result<Bytes, BlobStoreError>;
    /// Deletes the storage object. `NotFound` is treated as success (idempotent).
    async fn delete(&self, id: BlobId) -> Result<(), BlobStoreError>;
    /// Returns the storage key without reading anything.
    fn key_for(&self, id: BlobId) -> String;
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

#[cfg(test)]
mod tests {
    use super::*;

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

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("aero-blob-test-{}", BlobId::new()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
