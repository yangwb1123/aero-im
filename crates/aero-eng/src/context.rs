//! [`ExecutionContext`] — shared state passed to every command.
//!
//! Carries the project root, parsed config, verbosity flag, and any shared
//! runtime handles (e.g. tokio executor). Created once at the top of `main()`
//! and borrowed by every command.

use std::path::PathBuf;

use crate::config::EngineeringConfig;

/// Shared context for all command invocations.
#[derive(Debug, Clone)]
pub struct ExecutionContext {
    /// Absolute path to the project root.
    pub root: PathBuf,
    /// Whether to emit verbose / debug output.
    pub verbose: bool,
    /// Engineering config, loaded from `engineering.toml`.
    pub eng_config: EngineeringConfig,
}

impl ExecutionContext {
    /// Create a new context rooted at `root`, loading config from `engineering.toml`.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        let eng_config = EngineeringConfig::load(&root);
        Self { root, verbose: false, eng_config }
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn context_creates_with_default_config() {
        let ctx = ExecutionContext::new(PathBuf::from("/nonexistent"));
        assert_eq!(ctx.root, Path::new("/nonexistent"));
        assert!(!ctx.verbose);
        // Default config values
        assert_eq!(ctx.eng_config.filesize.rust_warn, 800);
    }

    #[test]
    fn context_root_is_accessible() {
        let ctx = ExecutionContext::new(PathBuf::from("/tmp"));
        assert_eq!(ctx.root.to_string_lossy(), "/tmp");
    }
}

