//! Aero Engineering CLI — trait-based command framework (Phase 1).
//!
//! Provides the [`Command`] trait, [`CommandRegistry`] for auto-discovery,
//! and shared context/result types. Commands are registered via
//! [`register_command!`] macro at compile time.
//!
//! ## Architecture
//!
//! Each command is a unit struct implementing [`Command`]. The
//! [`CommandRegistry::collect`] scans all registered commands and builds a
//! dispatch table. The binary's `main()` calls [`dispatch`] or iterates the
//! registry for help output.
//!
//! Inspired by snaplink's `cli.py` engineering CLI, adapted to Rust's trait
//! system for type safety, parallelism, and zero runtime dependencies.

use std::path::PathBuf;

// Re-export key types for downstream convenience.
pub use command::Command;
pub use command::CommandResult;
pub use context::ExecutionContext;
pub use outcome::{Outcome, Severity};
pub use registry::CommandRegistry;
pub use registry::DispatchResult;
pub use registry::RegistryResult;

pub mod checks;
pub mod command;
pub mod config;
pub mod context;
pub mod outcome;
pub mod registry;
pub mod run;
pub mod term;

// ---------------------------------------------------------------------------
// Convenience re-exports
// ---------------------------------------------------------------------------

/// Collect all registered commands into a registry and dispatch `args`.
///
/// Returns the [`DispatchResult`] on success, or an error outcome on failure/unknown.
/// Unlike snaplink's exit-code-sum approach, this preserves the full structured outcome.
pub async fn dispatch(args: &[String]) -> RegistryResult {
    let registry = CommandRegistry::collect();
    let cmd_name = args.first().map(String::as_str).unwrap_or("help");
    registry.execute(cmd_name, args).await
}

/// Build a default [`ExecutionContext`] rooted at the current directory.
/// Reads `engineering.toml` if present.
pub fn default_context() -> ExecutionContext {
    ExecutionContext::new(PathBuf::from("."))
}
