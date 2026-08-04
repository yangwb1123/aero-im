//! Region-aware blob storage.
//!
//! [`RegionRouter`] validates configured backend codes and selects them
//! fail-closed. [`PersistedRegionBlobStore`] first reads the immutable placement
//! snapshot on the blob row, so upload, download, transcription, export cleanup,
//! and GC keep using the same backend even if workspace configuration changes.

use std::collections::HashMap;
use std::sync::Arc;

use aero_common::BlobId;
use async_trait::async_trait;
use bytes::Bytes;

use crate::blob::BlobRepo;
use crate::blob_store::{BlobRange, BlobStore, BlobStoreError, BlobStream};

/// Canonical code persisted for new blobs placed in the default backend.
pub const DEFAULT_STORAGE_REGION: &str = "default";

/// A regional blob-store router.
///
/// Holds the default (non-regional) store plus zero or more region-specific
/// backends. Unknown non-default codes are rejected instead of silently
/// violating a residency policy.
#[derive(Clone)]
pub struct RegionRouter {
    /// The default blob store (local FS or the primary S3 bucket).
    default: Arc<dyn BlobStore>,
    /// Region-specific backends keyed by region code.
    regionals: HashMap<String, Arc<dyn BlobStore>>,
}

impl RegionRouter {
    /// Build a router with a default store and an optional set of region maps.
    /// Region stores are built lazily — `new` only records the configs and builds
    /// the stores up front so a misconfiguration (wrong credentials, unreachable
    /// endpoint) is caught at boot, not on the first blob upload.
    ///
    /// # Errors
    /// Returns [`BlobStoreError::Config`] if any region config is invalid.
    pub fn new(
        default: Arc<dyn BlobStore>,
        regions: HashMap<String, crate::s3_blob_store::S3Config>,
    ) -> Result<Self, BlobStoreError> {
        let mut regionals: HashMap<String, Arc<dyn BlobStore>> = HashMap::new();
        for (code, s3cfg) in regions {
            let store =
                Arc::new(crate::s3_blob_store::S3BlobStore::try_new(s3cfg)?) as Arc<dyn BlobStore>;
            regionals.insert(code, store);
        }
        Self::from_stores(default, regionals)
    }

    /// Build from already-constructed stores.
    ///
    /// This is also useful for deployments that provide a custom `BlobStore`
    /// implementation rather than S3 and makes routing independently testable.
    pub fn from_stores(
        default: Arc<dyn BlobStore>,
        regionals: HashMap<String, Arc<dyn BlobStore>>,
    ) -> Result<Self, BlobStoreError> {
        for code in regionals.keys() {
            validate_configured_code(code)?;
        }
        Ok(Self { default, regionals })
    }

    /// Select the blob store for the given region code.
    ///
    /// `None`, blank legacy values, and `"default"` select the default store.
    /// Any other code must match configured storage exactly.
    pub fn select(&self, region_code: Option<&str>) -> Result<&dyn BlobStore, BlobStoreError> {
        let code = region_code.map_or("", str::trim);
        if code.is_empty() || code == DEFAULT_STORAGE_REGION {
            return Ok(&*self.default);
        }
        self.regionals
            .get(code)
            .map(|store| &**store)
            .ok_or_else(|| {
                BlobStoreError::Config(format!("storage region `{code}` is not configured"))
            })
    }

    /// Validate and canonicalize a mutable workspace setting before snapshotting
    /// it onto a new blob.
    pub fn canonical_region_code(
        &self,
        region_code: Option<&str>,
    ) -> Result<String, BlobStoreError> {
        let code = region_code.map_or("", str::trim);
        if code.is_empty() || code == DEFAULT_STORAGE_REGION {
            return Ok(DEFAULT_STORAGE_REGION.to_owned());
        }
        self.select(Some(code))?;
        Ok(code.to_owned())
    }

    /// Configured codes exposed to the workspace administration API.
    #[must_use]
    pub fn configured_region_codes(&self) -> Vec<String> {
        let mut codes = self.regionals.keys().cloned().collect::<Vec<_>>();
        codes.sort();
        codes.insert(0, DEFAULT_STORAGE_REGION.to_owned());
        codes
    }

    /// Reference to the default store (for operations that don't need region
    /// routing, e.g. health checks).
    #[must_use]
    pub fn default_store(&self) -> &dyn BlobStore {
        &*self.default
    }

    /// The number of configured region-specific backends (excl. the default).
    #[must_use]
    pub fn region_count(&self) -> usize {
        self.regionals.len()
    }

    /// Check health of the default and every configured regional store.
    pub async fn health_check(&self) -> Result<(), BlobStoreError> {
        let stores =
            std::iter::once(&*self.default).chain(self.regionals.values().map(|store| &**store));
        for result in futures::future::join_all(stores.map(BlobStore::health_check)).await {
            result?;
        }
        Ok(())
    }
}

fn validate_configured_code(code: &str) -> Result<(), BlobStoreError> {
    if code.is_empty() || code != code.trim() || code == DEFAULT_STORAGE_REGION || code.len() > 16 {
        return Err(BlobStoreError::Config(format!(
            "invalid storage region code `{code}` (must be trimmed, 1..=16 bytes, and not `default`)"
        )));
    }
    Ok(())
}

/// `BlobStore` facade that routes every operation by persisted blob metadata.
#[derive(Clone)]
pub struct PersistedRegionBlobStore {
    blobs: BlobRepo,
    router: RegionRouter,
}

impl PersistedRegionBlobStore {
    #[must_use]
    pub fn new(blobs: BlobRepo, router: RegionRouter) -> Self {
        Self { blobs, router }
    }

    async fn backend(&self, id: BlobId) -> Result<Option<&dyn BlobStore>, BlobStoreError> {
        let scope = self
            .blobs
            .storage_scope(id)
            .await
            .map_err(|error| BlobStoreError::Io(std::io::Error::other(error.to_string())))?;
        scope
            .map(|scope| self.router.select(scope.storage_region.as_deref()))
            .transpose()
    }
}

#[async_trait]
impl BlobStore for PersistedRegionBlobStore {
    async fn put(&self, id: BlobId, bytes: Bytes) -> Result<String, BlobStoreError> {
        self.backend(id)
            .await?
            .ok_or(BlobStoreError::NotFound)?
            .put(id, bytes)
            .await
    }

    async fn get(&self, id: BlobId) -> Result<Bytes, BlobStoreError> {
        self.backend(id)
            .await?
            .ok_or(BlobStoreError::NotFound)?
            .get(id)
            .await
    }

    async fn get_stream(
        &self,
        id: BlobId,
        range: Option<BlobRange>,
    ) -> Result<BlobStream, BlobStoreError> {
        self.backend(id)
            .await?
            .ok_or(BlobStoreError::NotFound)?
            .get_stream(id, range)
            .await
    }

    async fn delete(&self, id: BlobId) -> Result<(), BlobStoreError> {
        let Some(store) = self.backend(id).await? else {
            return Ok(());
        };
        store.delete(id).await
    }

    fn key_for(&self, id: BlobId) -> String {
        format!("persisted-region:{id}")
    }

    async fn health_check(&self) -> Result<(), BlobStoreError> {
        self.router.health_check().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob_store::LocalFsBlobStore;
    use aero_common::BlobId;
    use bytes::Bytes;

    fn tmp_store(label: &str) -> (Arc<dyn BlobStore>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("aero_region_test_{label}"));
        let _ = std::fs::create_dir_all(&dir);
        (Arc::new(LocalFsBlobStore::new(&dir).unwrap()), dir)
    }

    #[tokio::test]
    async fn select_unknown_region_is_rejected() {
        let (def, _d) = tmp_store("default");
        let router = RegionRouter::new(def.clone(), HashMap::new()).unwrap();
        assert!(matches!(
            router.select(Some("eu-moon-1")),
            Err(BlobStoreError::Config(_))
        ));
    }

    #[tokio::test]
    async fn select_none_returns_default() {
        let (def, _d) = tmp_store("def2");
        let router = RegionRouter::new(def.clone(), HashMap::new()).unwrap();
        let store = router.select(None).unwrap();
        let id = BlobId::new();
        store.put(id, Bytes::from_static(b"default")).await.unwrap();
        assert_eq!(def.get(id).await.unwrap(), Bytes::from_static(b"default"));
    }

    #[tokio::test]
    async fn select_known_region_returns_region_store() {
        let (def, _d) = tmp_store("def3");
        let (regional, _r) = tmp_store("regional");
        let router = RegionRouter::from_stores(
            def.clone(),
            HashMap::from([("eu-west-1".to_owned(), regional.clone())]),
        )
        .unwrap();
        assert_eq!(router.region_count(), 1);
        let store = router.select(Some("eu-west-1")).unwrap();
        let id = BlobId::new();
        store.put(id, Bytes::from("hello")).await.unwrap();
        let got = store.get(id).await.unwrap();
        assert_eq!(got, Bytes::from("hello"));
        assert!(matches!(def.get(id).await, Err(BlobStoreError::NotFound)));
        store.delete(id).await.unwrap();
        assert!(matches!(store.get(id).await, Err(BlobStoreError::NotFound)));
    }

    #[tokio::test]
    async fn explicit_default_code_returns_default() {
        let (def, _d) = tmp_store("empty");
        let router = RegionRouter::new(def.clone(), HashMap::new()).unwrap();
        assert_eq!(router.region_count(), 0);
        let store = router.select(Some(DEFAULT_STORAGE_REGION)).unwrap();
        let id = BlobId::new();
        store.put(id, Bytes::from_static(b"default")).await.unwrap();
        assert_eq!(def.get(id).await.unwrap(), Bytes::from_static(b"default"));
    }

    #[tokio::test]
    async fn router_health_check_ok_with_default_only() {
        let (def, _d) = tmp_store("health");
        let router = RegionRouter::new(def.clone(), HashMap::new()).unwrap();
        let result = router.health_check().await;
        assert!(result.is_ok(), "health check should pass: {result:?}");
    }

    #[tokio::test]
    async fn round_trip_blob_via_select() {
        let (def, _d) = tmp_store("roundtrip");
        let router = RegionRouter::new(def.clone(), HashMap::new()).unwrap();
        let store = router.select(None).unwrap();
        let id = BlobId::new();
        let data = Bytes::from("round trip test data");
        store.put(id, data.clone()).await.unwrap();
        let result = store.get(id).await.unwrap();
        assert_eq!(result, data);
        store.delete(id).await.unwrap();
        assert!(matches!(store.get(id).await, Err(BlobStoreError::NotFound)));
    }

    #[tokio::test]
    async fn default_store_is_accessible() {
        let (def, _d) = tmp_store("default_access");
        let router = RegionRouter::new(def.clone(), HashMap::new()).unwrap();
        let store = router.default_store();
        let id = BlobId::new();
        store.put(id, Bytes::from("data")).await.unwrap();
        let got = store.get(id).await.unwrap();
        assert_eq!(got, Bytes::from("data"));
    }
}
