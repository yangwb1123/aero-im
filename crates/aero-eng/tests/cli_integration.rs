//! Integration tests for the Aero Engineering CLI framework.
//!
//! Verifies that commands register correctly, Outcome merges work,
//! and the registry dispatches properly end-to-end.

use aero_eng::*;

/// A test command that always succeeds.
struct OkCmd;
#[async_trait::async_trait]
impl Command for OkCmd {
    fn name(&self) -> &'static str {
        "ok"
    }
    fn description(&self) -> &'static str {
        "Always succeeds"
    }
    async fn execute(&self, _ctx: &ExecutionContext, _args: &[String]) -> Outcome {
        Outcome::ok("ok done")
    }
}

/// A test command that always fails.
struct ErrCmd;
#[async_trait::async_trait]
impl Command for ErrCmd {
    fn name(&self) -> &'static str {
        "fail"
    }
    fn description(&self) -> &'static str {
        "Always fails"
    }
    async fn execute(&self, _ctx: &ExecutionContext, _args: &[String]) -> Outcome {
        Outcome::error("fail done")
    }
}

/// A test command that checks args.
struct ArgCmd;
#[async_trait::async_trait]
impl Command for ArgCmd {
    fn name(&self) -> &'static str {
        "arg-test"
    }
    fn description(&self) -> &'static str {
        "Tests argument passing"
    }
    async fn execute(&self, _ctx: &ExecutionContext, args: &[String]) -> Outcome {
        if args.len() > 2 && args[2] == "hello" {
            Outcome::ok("arg received")
        } else {
            Outcome::error("expected 'hello' arg")
        }
    }
}

fn test_registry() -> CommandRegistry {
    CommandRegistry::new()
        .with_command(Box::new(OkCmd))
        .with_command(Box::new(ErrCmd))
        .with_command(Box::new(ArgCmd))
}

#[tokio::test]
async fn registry_ok_command_returns_ok() {
    let reg = test_registry();
    let result = reg.execute("ok", &["ok".into()]).await;
    assert!(result.is_ok(), "ok command should succeed: {result:?}");
    let dispatch = result.unwrap();
    assert_eq!(dispatch.command, "ok");
    assert!(dispatch.is_ok());
}

#[tokio::test]
async fn registry_err_command_returns_err() {
    let reg = test_registry();
    let result = reg.execute("fail", &["fail".into()]).await;
    assert!(result.is_err(), "fail command should error: {result:?}");
    let dispatch = result.unwrap_err();
    assert_eq!(dispatch.command, "fail");
    assert!(!dispatch.is_ok());
    assert_eq!(dispatch.exit_code(), 1);
}

#[tokio::test]
async fn registry_unknown_command_returns_err() {
    let reg = test_registry();
    let result = reg.execute("nonexistent", &["nonexistent".into()]).await;
    assert!(result.is_err(), "unknown command should error");
    let dispatch = result.unwrap_err();
    assert_eq!(dispatch.exit_code(), 1);
    assert!(dispatch.outcome.message().contains("unknown command"));
}

#[tokio::test]
async fn registry_help_returns_ok() {
    let reg = test_registry();
    let result = reg.execute("help", &["help".into()]).await;
    assert!(result.is_ok(), "help should succeed: {result:?}");
}

#[tokio::test]
async fn registry_arg_passing_works() {
    let reg = test_registry();
    // With correct arg (args[0]=prog, args[1]=cmd, args[2]=arg)
    let result = reg
        .execute(
            "arg-test",
            &["aero-cli".into(), "arg-test".into(), "hello".into()],
        )
        .await;
    assert!(
        result.is_ok(),
        "arg-test with hello should succeed: {result:?}"
    );
    // Without correct arg
    let result = reg
        .execute(
            "arg-test",
            &["aero-cli".into(), "arg-test".into(), "wrong".into()],
        )
        .await;
    assert!(result.is_err(), "arg-test with wrong arg should fail");
}

#[tokio::test]
async fn outcome_merge_aggregates_correctly() {
    let outcomes = vec![
        Outcome::ok("first"),
        Outcome::ok("second"),
        Outcome::ok("third"),
    ];
    let merged = Outcome::merge(&outcomes);
    assert!(merged.is_ok());
    assert!(merged.message().contains("first"));
    assert!(merged.message().contains("third"));
}

#[tokio::test]
async fn outcome_merge_worst_wins() {
    let outcomes = vec![
        Outcome::ok("pass"),
        Outcome::error("fail-1"),
        Outcome::warning(0, "warn"),
    ];
    let merged = Outcome::merge(&outcomes);
    assert!(merged.is_error(), "error should dominate: {merged:?}");
    assert_eq!(merged.exit_code(), 1);
}

#[tokio::test]
async fn outcome_merge_all_errors() {
    let outcomes = vec![Outcome::error("err1"), Outcome::error("err2")];
    let merged = Outcome::merge(&outcomes);
    assert!(merged.is_error());
}

#[tokio::test]
async fn outcome_severity_ordering() {
    // Verify that Error > Warning > Skip > Ok when merging
    let outcomes = vec![
        Outcome::ok("okay"),
        Outcome::skip("skipped"),
        Outcome::error("error"),
    ];
    let merged = Outcome::merge(&outcomes);
    assert!(merged.is_error(), "error should beat skip and ok");
    assert_eq!(merged.exit_code(), 1);
}

#[tokio::test]
async fn outcome_with_detail_is_preserved() {
    let detail = serde_json::json!({"key": "value"});
    let o = Outcome::ok("with detail").with_detail(detail.clone());
    assert_eq!(o.detail(), Some(detail));
}

#[tokio::test]
async fn registry_contains_all_added_commands() {
    let reg = test_registry();
    assert_eq!(reg.len(), 3);
    let names: Vec<&str> = reg.iter().map(aero_eng::Command::name).collect();
    assert!(names.contains(&"ok"));
    assert!(names.contains(&"fail"));
    assert!(names.contains(&"arg-test"));
}
