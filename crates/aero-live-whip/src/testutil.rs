//! Test-only helpers.
//!
//! A tiny self-contained RAII temp directory so the integration tests can write
//! real `.ts`/`index.m3u8` files to disk and have them cleaned up afterwards,
//! without pulling an extra dev-dependency into this crate (the workspace
//! lockfile is treated as off-limits here).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Process-unique counter so concurrently-running tests never collide on a dir.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// An owned temporary directory that is removed (recursively) on drop.
///
/// Equivalent in spirit to `tempfile::tempdir()` but dependency-free: the path
/// is `{std::env::temp_dir()}/aero-whip-test-{pid}-{seq}`.
pub(crate) struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Create a fresh, empty, process-unique temporary directory.
    pub(crate) fn new() -> std::io::Result<Self> {
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aero-whip-test-{}-{seq}",
            std::process::id()
        ));
        // Clear any stale directory from a previous aborted run, then recreate.
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    /// The directory's absolute path.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Best-effort cleanup; ignore errors so a teardown failure never masks
        // a test assertion.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
