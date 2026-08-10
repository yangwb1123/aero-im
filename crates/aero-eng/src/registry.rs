//! [`CommandRegistry`] — auto-registration of commands.
//!
//! Built-in commands are defined in [`crate::commands`] via the
//! [`register_command!`] macro and returned by `commands::all()` in
//! registration order. [`CommandRegistry::collect`] builds the registry from
//! that list; `help`, `names()` and `completion_words()` are all derived
//! from the same single source of truth, so the three can never drift.
//!
//! Binaries may instead compose a custom registry via
//! [`CommandRegistry::from_commands`] / [`with_command`](Self::with_command)
//! (no `linkme` distributed slice — commands are an ordinary `Vec`).

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

    /// Collect all built-in commands registered via `register_command!`.
    ///
    /// Equivalent to `from_commands(commands::all())`; duplicate names are
    /// deduplicated first-wins.
    #[must_use]
    pub fn collect() -> Self {
        Self::from_commands(crate::commands::all())
    }

    /// Build a registry from an explicit command list (Path A seam for
    /// downstream binaries that do not want the built-in set).
    ///
    /// Registration order is preserved (it is the help/completion order);
    /// duplicate names are deduplicated first-wins.
    #[must_use]
    pub fn from_commands(commands: Vec<Box<dyn Command>>) -> Self {
        let mut seen = std::collections::HashSet::new();
        let mut deduped = Vec::with_capacity(commands.len());
        for cmd in commands {
            if seen.insert(cmd.name()) {
                deduped.push(cmd);
            }
        }
        Self { commands: deduped }
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
        print!("{}", self.help_text());
    }

    /// Full help text: header, usage, and every command in registration
    /// order, plus the `help` pseudo-command. Single source for help output.
    #[must_use]
    pub fn help_text(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        let _ = writeln!(s, "Aero Engineering CLI");
        let _ = writeln!(s);
        let _ = writeln!(s, "Usage: aero-cli <command> [options]");
        let _ = writeln!(s);
        let _ = writeln!(s, "Commands:");
        for cmd in &self.commands {
            let _ = writeln!(s, "  {:20} {}", cmd.name(), cmd.description());
        }
        let _ = writeln!(s, "  {:20} Show this help message", "help");
        let _ = writeln!(s);
        s
    }

    /// Command names in registration order.
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.commands.iter().map(|c| c.name()).collect()
    }

    /// Shell-completion word list: every command name plus `help`,
    /// space-joined, in registration order.
    #[must_use]
    pub fn completion_words(&self) -> String {
        let mut words: Vec<&str> = self.names();
        words.push("help");
        words.join(" ")
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

/// Define a command struct implementing [`Command`] + `Default`-constructible.
///
/// ```ignore
/// register_command!(
///     MigrateCmd,
///     "migrate",
///     "Run database migrations",
///     |ctx, args| { Outcome::ok("migrated") }
/// );
/// ```
///
/// The generated unit struct implements [`Command`] with the given name,
/// description, and async body. Built-in commands live in
/// [`crate::commands`]; add the struct to `commands::all()` for it to be
/// collected by [`CommandRegistry::collect`].
#[macro_export]
macro_rules! register_command {
    ($s:ident, $n:expr, $d:expr, |$ctx:ident, $args:ident| $e:block) => {
        pub(crate) struct $s;
        #[async_trait::async_trait]
        impl $crate::command::Command for $s {
            fn name(&self) -> &'static str { $n }
            fn description(&self) -> &'static str { $d }
            async fn execute(&self, $ctx: &$crate::context::ExecutionContext, $args: &[String]) -> $crate::outcome::Outcome $e
        }
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

    /// Duplicate of [`TestCmd`] (same name, different description) to verify
    /// first-wins dedupe.
    struct TestCmdDup;
    #[async_trait]
    impl Command for TestCmdDup {
        fn name(&self) -> &'static str {
            "test-cmd"
        }
        fn description(&self) -> &'static str {
            "A duplicate test command"
        }
        async fn execute(&self, _ctx: &ExecutionContext, _args: &[String]) -> Outcome {
            Outcome::ok("dup")
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
        reg = reg
            .with_command(Box::new(TestCmd))
            .with_command(Box::new(FailCmd));
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

    #[test]
    fn from_commands_dedupes_first_wins() {
        let reg = CommandRegistry::from_commands(vec![
            Box::new(TestCmd),
            Box::new(TestCmdDup),
            Box::new(FailCmd),
        ]);
        assert_eq!(reg.len(), 2, "duplicate name must be dropped");
        let names = reg.names();
        assert_eq!(names, vec!["test-cmd", "fail-cmd"]);
        // First registration wins: description is TestCmd's, not TestCmdDup's.
        let test_cmd = reg.iter().find(|c| c.name() == "test-cmd").unwrap();
        assert_eq!(test_cmd.description(), "A test command");
    }

    #[test]
    fn from_commands_preserves_registration_order() {
        let reg = CommandRegistry::from_commands(vec![Box::new(FailCmd), Box::new(TestCmd)]);
        assert_eq!(reg.names(), vec!["fail-cmd", "test-cmd"]);
    }

    #[test]
    fn names_returns_registration_order() {
        let reg = CommandRegistry::new()
            .with_command(Box::new(TestCmd))
            .with_command(Box::new(FailCmd));
        assert_eq!(reg.names(), vec!["test-cmd", "fail-cmd"]);
    }

    #[test]
    fn completion_words_covers_names_and_help() {
        let reg = CommandRegistry::new()
            .with_command(Box::new(TestCmd))
            .with_command(Box::new(FailCmd));
        assert_eq!(reg.completion_words(), "test-cmd fail-cmd help");
    }

    #[test]
    fn help_text_lists_commands_in_order_and_help() {
        let reg = CommandRegistry::new()
            .with_command(Box::new(FailCmd))
            .with_command(Box::new(TestCmd));
        let help = reg.help_text();
        assert!(help.starts_with("Aero Engineering CLI\n"));
        assert!(help.contains("Usage: aero-cli <command> [options]"));
        let fail_pos = help.find("  fail-cmd").expect("fail-cmd listed");
        let test_pos = help.find("  test-cmd").expect("test-cmd listed");
        assert!(fail_pos < test_pos, "help must follow registration order");
        assert!(help.contains("  help                 Show this help message"));
    }

    #[test]
    fn collect_builds_builtin_registry() {
        let reg = CommandRegistry::collect();
        assert!(
            !reg.is_empty(),
            "collect() must return the built-in commands"
        );
        let names = reg.names();
        // The parity invariant's anchor set: these names were historically
        // hand-maintained in a completion literal that omitted bench/dashboard.
        for expected in [
            "check",
            "gate",
            "test",
            "integration",
            "skill",
            "doctor",
            "completion",
            "network",
            "audit-provision-check",
            "bench",
            "dashboard",
        ] {
            assert!(names.contains(&expected), "collect() missing {expected}");
        }
    }
}
