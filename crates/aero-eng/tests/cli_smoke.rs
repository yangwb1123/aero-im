//! End-to-end smoke tests for the aero-cli binary.
//! Run: cargo test --test cli_smoke -- --ignored

use std::process::Command;

fn run_cli(args: &[&str]) -> (i32, String) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();
    let binary = root.join("target").join("debug").join("aero-cli");
    let output = Command::new(&binary).args(args).output().expect("run aero-cli");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    (output.status.code().unwrap_or(-1), stdout)
}

#[test]
#[ignore = "requires compiled aero-cli binary"]
fn smoke_help_exit_0() {
    let (code, out) = run_cli(&["help"]);
    assert_eq!(code, 0, "help should exit 0");
    assert!(out.contains("Commands:"), "help should list commands");
    assert!(out.contains("check"), "help should mention check");
}

#[test]
#[ignore = "requires compiled aero-cli binary"]
fn smoke_unknown_command_exit_1() {
    let (code, _) = run_cli(&["definitely-nonexistent"]);
    assert_eq!(code, 1, "unknown should exit 1");
}

#[test]
#[ignore = "requires compiled aero-cli binary"]
fn smoke_doctor_exit_0() {
    let (code, out) = run_cli(&["doctor"]);
    assert!(code == 0 || code == 1, "doctor should not crash: exit={code}");
    assert!(out.contains("Rust toolchain"), "doctor checks toolchain");
}

#[test]
#[ignore = "requires compiled aero-cli binary"]
fn smoke_gate_list_exit_0() {
    let (code, out) = run_cli(&["gate", "list"]);
    assert_eq!(code, 0, "gate list exit 0");
    assert!(out.contains("filesize"), "gate list shows filesize");
    assert!(out.contains("deps-native"), "gate list shows deps-native");
}

#[test]
#[ignore = "requires compiled aero-cli binary"]
fn smoke_completion_bash() {
    let (code, out) = run_cli(&["completion", "bash"]);
    assert_eq!(code, 0, "completion bash exit 0");
    assert!(out.contains("complete"), "bash completion output");
}

#[test]
#[ignore = "requires compiled aero-cli binary"]
fn smoke_completion_zsh() {
    let (code, out) = run_cli(&["completion", "zsh"]);
    assert_eq!(code, 0, "completion zsh exit 0");
    assert!(out.contains("compdef"), "zsh completion has compdef");
}

#[test]
#[ignore = "requires compiled aero-cli binary"]
fn smoke_completion_fish() {
    let (code, out) = run_cli(&["completion", "fish"]);
    assert_eq!(code, 0, "completion fish exit 0");
    assert!(out.contains("complete"), "fish completion output");
}

#[test]
#[ignore = "requires compiled aero-cli binary"]
fn smoke_skill_list() {
    let (code, _) = run_cli(&["skill", "list"]);
    assert!(code == 0 || code == 1, "skill list no crash: exit={code}");
}

#[test]
#[ignore = "requires compiled aero-cli binary"]
fn smoke_skill_view() {
    let (code, _) = run_cli(&["skill", "view", "testing"]);
    assert!(code == 0 || code == 1, "skill view no crash: exit={code}");
}

#[test]
#[ignore = "requires compiled aero-cli binary"]
fn smoke_check_available() {
    let (code, _) = run_cli(&["check"]);
    assert!(code == 0 || code == 1, "check no crash: exit={code}");
}

#[test]
#[ignore = "requires compiled aero-cli binary"]
fn smoke_migrate_help() {
    let (code, _) = run_cli(&["migrate", "--help"]);
    assert!(code == 0 || code == 1, "migrate --help no crash");
}
