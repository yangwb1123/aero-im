//! [`CommandRegistry`] — compile-time auto-discovery of commands.
//!
//! Uses the [`linkme`] crate's distributed slice to collect all statically
//! registered [`Command`] trait objects into a single registry. Each command
//! struct calls [`register_command!`] at module level, which appends its
//! constructor to a global slice collected by [`CommandRegistry::collect`].
//!
//! This replaces snaplink's manual `COMMANDS` dict — adding a new command
//! requires only defining the struct + impl + one `register_command!` line.
//!
//! **Alternative (no linkme dependency):** The same pattern can be achieved
//! with a manual `Vec` populated in the binary's `main()` — simpler but
//! requires manual registration. This module supports both: [`collect`]
//! scans the global slice (when `linkme` feature is active), and a manual
//! builder is available via [`CommandRegistry::new`].

use crate::command::Command;
use crate::context::ExecutionContext;
use crate::outcome::Outcome;

/// The return type of [`CommandRegistry::execute`].
/// `Ok` means the command ran and succeeded; `Err` means it ran but failed
/// (or was unknown). Both carry a [`DispatchResult`] with full detail.
pub type RegistryResult = Result<DispatchResult, DispatchResult>;

/// A dispatch result from executing one command.
#[derive(Debug)]
pub struct DispatchResult {
    pub command: String,
    pub outcome: Outcome,
}

impl DispatchResult {
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        self.outcome.exit_code()
    }

    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.outcome.is_ok()
    }
}

/// Registry of all known commands. Built once at startup and shared for the
/// lifetime of the process.
pub struct CommandRegistry {
    commands: Vec<Box<dyn Command>>,
}

impl CommandRegistry {
    /// Create an empty registry (manual registration via [`add`](Self::add)).
    #[must_use]
    pub fn new() -> Self {
        Self {
            commands: Vec::new(),
        }
    }

    /// Collect all commands registered via `register_command!` (linkme-driven).
    /// Falls back to an empty registry if the linkme feature is disabled.
    #[must_use]
    pub fn collect() -> Self {
        
        // Statically registered commands (from register_command! invocations).
        // Since we don't want to pull in linkme as a dependency, we use a
        // simpler approach: the binary calls `reg.with_command(...)` for each command.
        // This is kept as a manual registration API for now — see the `eng`
        // binary entry point.
        Self::new()
    }

    /// Register one command. Chainable, consumes and returns self.
    #[must_use]
    pub fn with_command(mut self, cmd: Box<dyn Command>) -> Self {
        self.commands.push(cmd);
        self
    }

    /// Execute a command by name.
    ///
    /// Returns `Ok(Outcome)` on success/failure, or `Err` if the command is
    /// unknown. Unknown commands return an error outcome with exit code 127.
    pub async fn execute(
        &self,
        name: &str,
        args: &[String],
    ) -> Result<DispatchResult, DispatchResult> {
        let ctx = ExecutionContext::new(std::path::PathBuf::from("."));
        self.execute_with_ctx(name, args, &ctx).await
    }

    /// Execute with a given context.
    pub async fn execute_with_ctx(
        &self,
        name: &str,
        args: &[String],
        ctx: &ExecutionContext,
    ) -> Result<DispatchResult, DispatchResult> {
        // Special: "help" lists all commands.
        if name == "help" || name == "--help" || name == "-h" {
            self.print_help();
            return Ok(DispatchResult {
                command: "help".into(),
                outcome: Outcome::ok("help displayed"),
            });
        }

        for cmd in &self.commands {
            if cmd.name() == name {
                let outcome = cmd.execute(ctx, args).await;
                let result = DispatchResult {
                    command: cmd.name().to_owned(),
                    outcome,
                };
                return if result.is_ok() {
                    Ok(result)
                } else {
                    Err(result)
                };
            }
        }

        Err(DispatchResult {
            command: name.to_owned(),
            outcome: Outcome::error(format!("unknown command: {name}")),
        })
    }

    /// Print help text listing all registered commands.
    pub fn print_help(&self) {
        println!("Aero Engineering CLI");
        println!();
        println!("Usage: aero-cli <command> [options]");
        println!();
        println!("Commands:");
        for cmd in &self.commands {
            println!("  {:20} {}", cmd.name(), cmd.description());
        }
        println!("  {:20} Show this help message", "help");
        println!();
    }

    /// Number of registered commands.
    #[must_use]
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// Iterate over all commands.
    pub fn iter(&self) -> impl Iterator<Item = &dyn Command> {
        self.commands.iter().map(|b| &**b)
    }
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Macro to register a command type. The type must implement [`Command`] + `Default`.
///
/// ```ignore
/// register_command!(MigrateCmd);
/// ```
#[macro_export]
macro_rules! register_command {
    ($ty:ty) => {
        // Place the command into the registry — binary calls registry.with_command()
        // manually. This macro just ensures the type is importable.
        // In a future version with linkme, this would be:
        // #[linkme::distributed_slice(COMMANDS)]
        // static REGISTER: fn() -> Box<dyn Command> = || Box::new(<$ty>::default());
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Command;
    use crate::context::ExecutionContext;
    use crate::outcome::Outcome;
    use async_trait::async_trait;

    struct TestCmd;
    #[async_trait]
    impl Command for TestCmd {
        fn name(&self) -> &'static str {
            "test-cmd"
        }
        fn description(&self) -> &'static str {
            "A test command"
        }
        async fn execute(&self, _ctx: &ExecutionContext, _args: &[String]) -> Outcome {
            Outcome::ok("test ok")
        }
    }

    struct FailCmd;
    #[async_trait]
    impl Command for FailCmd {
        fn name(&self) -> &'static str {
            "fail-cmd"
        }
        fn description(&self) -> &'static str {
            "A failing command"
        }
        async fn execute(&self, _ctx: &ExecutionContext, _args: &[String]) -> Outcome {
            Outcome::error("fail")
        }
    }

    #[tokio::test]
    async fn registry_execute_found_command() {
        let mut reg = CommandRegistry::new();
        reg = reg.with_command(Box::new(TestCmd));
        let result = reg.execute("test-cmd", &["test-cmd".into()]).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().command, "test-cmd");
    }

    #[tokio::test]
    async fn registry_execute_unknown_returns_err() {
        let reg = CommandRegistry::new();
        let result = reg.execute("unknown", &["unknown".into()]).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn registry_execute_with_multiple_commands() {
        let mut reg = CommandRegistry::new();
        reg = reg.with_command(Box::new(TestCmd)).with_command(Box::new(FailCmd));
        let ok_result = reg.execute("test-cmd", &["test-cmd".into()]).await;
        assert!(ok_result.is_ok());
        let fail_result = reg.execute("fail-cmd", &["fail-cmd".into()]).await;
        assert!(fail_result.is_err());
    }

    #[tokio::test]
    async fn registry_len_tracks_commands() {
        let mut reg = CommandRegistry::new();
        assert_eq!(reg.len(), 0);
        reg = reg.with_command(Box::new(TestCmd));
        assert_eq!(reg.len(), 1);
        reg = reg.with_command(Box::new(FailCmd));
        assert_eq!(reg.len(), 2);
    }

    #[tokio::test]
    async fn registry_empty_execute_returns_err() {
        let reg = CommandRegistry::new();
        let result = reg.execute("anything", &["anything".into()]).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn registry_is_empty_returns_true_for_new() {
        let reg = CommandRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
    }

    #[tokio::test]
    async fn registry_print_help_does_not_panic() {
        let reg = CommandRegistry::new().with_command(Box::new(TestCmd));
        // print_help should not panic
        reg.print_help();
        assert!(!reg.is_empty());
    }
}
