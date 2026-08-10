//! Helpers for running external processes (cargo, shell scripts, etc.).
//!
//! Wraps `std::process::Command` with structured error reporting, timeout,
//! and output capture. Designed for the engineering CLI's check commands
//! that delegate to `cargo check`, `cargo test`, etc.

use std::time::Duration;

use crate::outcome::Outcome;

/// Run an external command with timeout. Captures stdout/stderr.
///
/// # Errors
/// Returns an error outcome if the command fails, times out, or does not exist.
pub async fn run_cmd(program: &str, args: &[&str], timeout: Duration) -> Outcome {
    let start = std::time::Instant::now();
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);

    let result = tokio::time::timeout(timeout, cmd.output()).await;

    match result {
        Ok(Ok(output)) => {
            let dur = start.elapsed();
            if output.status.success() {
                Outcome::ok(format!("{program} succeeded")).with_duration(dur)
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                Outcome::error(format!("{program} failed: {stderr}"))
                    .with_duration(dur)
                    .with_detail(serde_json::json!({
                        "exit_code": output.status.code(),
                        "stdout": String::from_utf8_lossy(&output.stdout),
                        "stderr": stderr,
                    }))
            }
        }
        Ok(Err(e)) => Outcome::error(format!("cannot launch {program}: {e}")),
        Err(_) => Outcome::error(format!("{program} timed out after {timeout:?}")),
    }
}

/// Run `cargo check --workspace` with a default 5-minute timeout.
pub async fn cargo_check() -> Outcome {
    run_cmd(
        "cargo",
        &["check", "--workspace", "-q"],
        Duration::from_secs(300),
    )
    .await
}

/// Run `cargo test --workspace --lib` with a default 10-minute timeout.
pub async fn cargo_test_lib() -> Outcome {
    run_cmd(
        "cargo",
        &["test", "--workspace", "--lib"],
        Duration::from_secs(600),
    )
    .await
}

/// Run `cargo clippy --workspace --all-targets` with a default 5-minute timeout.
pub async fn cargo_clippy() -> Outcome {
    run_cmd(
        "cargo",
        &["clippy", "--workspace", "--all-targets", "-q"],
        Duration::from_secs(300),
    )
    .await
}

/// Run the integration harness (`scripts/test-integration.sh`) with a default
/// 30-minute timeout (mirrors `cargo_test_lib`; the harness owns fresh-DB
/// migration regressions, the drill suites, and the full ignored workspace
/// suite — and must be run from the repository root, same constraint as the
/// `gate` arms).
pub async fn test_integration() -> Outcome {
    run_cmd(
        "bash",
        &["scripts/test-integration.sh"],
        Duration::from_secs(1800),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_cmd_echo_succeeds() {
        let o = run_cmd("echo", &["hello"], Duration::from_secs(5)).await;
        assert!(o.is_ok(), "echo should succeed: {o}");
    }

    #[tokio::test]
    async fn run_cmd_nonexistent_fails() {
        let o = run_cmd(
            "this-command-does-not-exist-12345",
            &[],
            Duration::from_secs(5),
        )
        .await;
        assert!(o.is_error(), "nonexistent command should error: {o}");
    }

    #[tokio::test]
    async fn run_cmd_with_args_passes_them() {
        let o = run_cmd("echo", &["hello", "world"], Duration::from_secs(5)).await;
        assert!(o.is_ok());
    }

    #[tokio::test]
    async fn run_cmd_timeout_yields_error() {
        let o = run_cmd("sleep", &["10"], Duration::from_millis(50)).await;
        assert!(o.is_error(), "sleep should time out: {o}");
    }

    #[tokio::test]
    async fn run_cmd_empty_command_fails() {
        let o = run_cmd("", &[], Duration::from_secs(1)).await;
        assert!(o.is_error(), "empty command should fail: {o}");
    }

    #[tokio::test]
    async fn run_cmd_with_special_characters() {
        let o = run_cmd("echo", &["hello", "world", "!"], Duration::from_secs(5)).await;
        assert!(o.is_ok(), "special chars should work: {o}");
    }

    #[tokio::test]
    async fn run_cmd_long_args() {
        let long_arg = "a".repeat(1000);
        let o = run_cmd("echo", &[&long_arg], Duration::from_secs(5)).await;
        assert!(o.is_ok(), "long args should work: {o}");
    }

    #[tokio::test]
    async fn run_cmd_unicode() {
        let o = run_cmd("echo", &["Hello", "世界", "🌍"], Duration::from_secs(5)).await;
        assert!(o.is_ok(), "unicode should work: {o}");
    }

    #[tokio::test]
    async fn cargo_check_returns_without_panicking() {
        // cargo_check runs against the real workspace
        let o = crate::run::cargo_check().await;
        // Should either succeed or fail gracefully - not panic
        assert!(o.is_ok() || o.is_error(), "cargo check should not panic");
    }

    #[test]
    fn test_integration_script_is_present() {
        // The integration wrapper targets `scripts/test-integration.sh`
        // relative to the repository root (CWD constraint). Guard the path
        // contract so a rename/move breaks this unit suite instead of the
        // gate silently running nothing.
        let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../");
        assert!(
            repo_root.join("scripts/test-integration.sh").exists(),
            "scripts/test-integration.sh must exist at the repository root"
        );
    }
}
