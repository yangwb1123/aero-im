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

