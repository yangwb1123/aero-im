//! [`Command`] trait — any executable sub-command.
//!
//! A command is a stateless unit struct (`struct FooCmd;`) implementing the
//! [`Command`] trait. It provides metadata (name, description, usage) and an
//! [`execute`](Command::execute) method that the registry calls.

use std::time::Duration;

use async_trait::async_trait;

use crate::context::ExecutionContext;
use crate::outcome::Outcome;

/// A named, executable sub-command in the CLI.
#[async_trait]
pub trait Command: Send + Sync {
    /// Short command name (e.g. `"migrate"`, `"check"`).
    fn name(&self) -> &'static str;

    /// One-line description shown in help.
    fn description(&self) -> &'static str;

    /// Optional usage hint (e.g. `"aero-cli migrate"`).
    fn usage(&self) -> &'static str {
        self.name()
    }

    /// Execute the command.
    ///
    /// `args` is the full argument vector (`args[0]` is the command name).
    /// Returns an [`Outcome`] summarising success, failure, or skip.
    async fn execute(&self, ctx: &ExecutionContext, args: &[String]) -> Outcome;
}

/// Structured summary of one command run.
#[derive(Debug, Clone)]
pub struct CommandResult {
    /// Human-readable command name.
    pub command: String,
    /// Exit code: `0` = success, `1` = failure, `2` = skip/not-applicable.
    pub exit_code: i32,
    /// Duration of execution.
    pub duration: Duration,
    /// Machine-readable detail (optional).
    pub detail: Option<serde_json::Value>,
}

impl CommandResult {
    /// Build a success result.
    #[must_use]
    pub fn ok(command: &str, duration: Duration) -> Self {
        Self {
            command: command.to_owned(),
            exit_code: 0,
            duration,
            detail: None,
        }
    }

    /// Build a failure result.
    #[must_use]
    pub fn err(command: &str, duration: Duration, reason: impl Into<String>) -> Self {
        Self {
            command: command.to_owned(),
            exit_code: 1,
            duration,
            detail: Some(serde_json::json!({ "error": reason.into() })),
        }
    }

    /// Build a skipped result.
    #[must_use]
    pub fn skip(command: &str, duration: Duration, reason: impl Into<String>) -> Self {
        Self {
            command: command.to_owned(),
            exit_code: 2,
            duration,
            detail: Some(serde_json::json!({ "skip": reason.into() })),
        }
    }
}

impl From<Outcome> for CommandResult {
    fn from(o: Outcome) -> Self {
        Self {
            command: String::new(),
            exit_code: o.exit_code(),
            duration: o.duration(),
            detail: o.detail(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Outcome;

    #[test]
    fn ok_result() {
        let cr = CommandResult::ok("test", Duration::ZERO);
        assert_eq!(cr.exit_code, 0);
    }

    #[test]
    fn err_result() {
        let cr = CommandResult::err("test", Duration::ZERO, "reason");
        assert_eq!(cr.exit_code, 1);
    }

    #[test]
    fn skip_result() {
        let cr = CommandResult::skip("test", Duration::ZERO, "reason");
        assert_eq!(cr.exit_code, 2);
    }

    #[test]
    fn from_outcome_ok() {
        let cr: CommandResult = Outcome::ok("good").into();
        assert_eq!(cr.exit_code, 0);
    }

    #[test]
    fn from_outcome_error() {
        let cr: CommandResult = Outcome::error("bad").into();
        assert_eq!(cr.exit_code, 1);
    }
}
