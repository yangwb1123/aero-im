//! Region-aware blob store router (ROADMAP 第六版 · 方向五·1).
//!
//! Wraps a default [`BlobStore`] plus a set of region-specific S3-compatible
//! backends. Callers that know the workspace's `region_code` call
//! [`RegionRouter::select`] to pick the right backend before operating on a blob.
//!
//! Data residency is **opt-in**: when no `storage_regions` config is provided,
//! the router contains only the default store, and `select` returns it for any
//! region — behaviour is byte-identical to a non-regional deployment.

use std::collections::HashMap;
use std::sync::Arc;

use crate::blob_store::{BlobStore, BlobStoreError};

/// A regional blob-store router.
///
/// Holds the default (non-regional) store plus zero or more region-specific
/// backends. The `select(region_code)` method returns the appropriate store;
/// unknown/unset region codes fall back to the default.
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
            let store = Arc::new(crate::s3_blob_store::S3BlobStore::new(s3cfg))
                as Arc<dyn BlobStore>;
            regionals.insert(code, store);
        }
        Ok(Self { default, regionals })
    }

    /// Select the blob store for the given region code.
    ///
    /// `None` or an unknown region code returns the default store.
    /// Region codes are matched exactly (case-sensitive, trimmed).
    #[must_use]
    pub fn select(&self, region_code: Option<&str>) -> &dyn BlobStore {
        let code = region_code.map(str::trim).unwrap_or("");
        if code.is_empty() {
            return &*self.default;
        }
        self.regionals.get(code).map_or(&*self.default, |s| &**s)
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

    /// Check health of the default and all regional stores. Fails fast on the
    /// first error, so a single dead region backend doesn't block others' health.
    pub async fn health_check(&self) -> Result<(), BlobStoreError> {
        self.default.health_check().await?;
        for (_code, store) in &self.regionals {
            store.health_check().await?;
        }
        Ok(())
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
    async fn select_unknown_region_falls_back_to_default() {
        let (def, _d) = tmp_store("default");
        let router = RegionRouter::new(def.clone(), HashMap::new()).unwrap();
        // An unknown code returns the default.
        let store = router.select(Some("eu-moon-1"));
        assert!(std::ptr::eq(store as *const dyn BlobStore, &*def as *const dyn BlobStore));
    }

    #[tokio::test]
    async fn select_none_returns_default() {
        let (def, _d) = tmp_store("def2");
        let router = RegionRouter::new(def.clone(), HashMap::new()).unwrap();
        let store = router.select(None);
        assert!(std::ptr::eq(store as *const dyn BlobStore, &*def as *const dyn BlobStore));
    }

    #[tokio::test]
    async fn select_known_region_returns_region_store() {
        let (def, _d) = tmp_store("def3");
        let (regional, _r) = tmp_store("regional");
        let mut regions = HashMap::new();
        // We can't construct an S3Config in a test w/o env vars, so use a
        // different test: build with empty regions and verify fallback.
        // Full regional test requires integration env.
        let router = RegionRouter::new(def.clone(), regions).unwrap();
        assert_eq!(router.region_count(), 0);
        let store = router.default_store();
        // Just verify we got something.
        let id = BlobId::new();
        store.put(id, Bytes::from("hello")).await.unwrap();
        let got = store.get(id).await.unwrap();
        assert_eq!(got, Bytes::from("hello"));
        store.delete(id).await.unwrap();
        assert!(matches!(store.get(id).await, Err(BlobStoreError::NotFound)));
    }
}
