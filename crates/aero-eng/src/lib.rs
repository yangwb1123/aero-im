//! Aero Engineering CLI — trait-based command framework.
//!
//! Provides the [`Command`] trait, [`CommandRegistry`], [`Outcome`], and
//! utility modules for building engineering CLI tools in Rust.
//!
//! # Quick Start
//!
//! ```rust
//! use aero_eng::{Command, CommandRegistry, ExecutionContext, Outcome};
//! use async_trait::async_trait;
//!
//! struct HelloCmd;
//! #[async_trait]
//! impl Command for HelloCmd {
//!     fn name(&self) -> &'static str { "hello" }
//!     fn description(&self) -> &'static str { "Say hello" }
//!     async fn execute(&self, _: &ExecutionContext, _: &[String]) -> Outcome {
//!         Outcome::ok("Hello, world!")
//!     }
//! }
//!
//! # async fn example() {
//! let reg = CommandRegistry::new().add(Box::new(HelloCmd));
//! let result = reg.execute("hello", &["hello".into()]).await;
//! assert!(result.is_ok());
//! # }
//! ```
//!
//! ## Architecture
//!
//! Each command is a unit struct implementing [`Command`]. Commands are
//! registered in a [`CommandRegistry`] and dispatched by name.
//! [`Outcome`] provides structured result reporting with severity levels
//! and automatic merging.
//!
//! Inspired by snaplink's `cli.py`, adapted to Rust's type system for
//! compile-time safety, parallelism, and zero runtime dependencies.

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
