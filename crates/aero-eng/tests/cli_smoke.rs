//! End-to-end smoke tests for the aero-eng binary (`cargo build -p aero-cli`).
//! Run: `cargo test --test cli_smoke -- --ignored`
//!
//! The binary under test is `target/debug/aero-eng` — the `aero-cli` crate's
//! bin. (`target/debug/aero-cli` is a *different* binary from the aero-server
//! crate; pointing these smokes at it was a latent wrong-binary bug.)

use std::process::Command;

fn run_cli(args: &[&str]) -> (i32, String) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let binary = root.join("target").join("debug").join("aero-eng");
    let output = Command::new(&binary)
        .args(args)
        .output()
        .expect("run aero-eng");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    (output.status.code().unwrap_or(-1), stdout)
}

#[test]
#[ignore = "requires compiled aero-eng binary"]
fn smoke_help_exit_0() {
    let (code, out) = run_cli(&["help"]);
    assert_eq!(code, 0, "help should exit 0");
    assert!(out.contains("Commands:"), "help should list commands");
    assert!(out.contains("check"), "help should mention check");
    // Parity tripwire at the binary level: registry-derived help must list
    // every built-in command, including the historically-drifted ones.
    assert!(out.contains("bench"), "help should list bench");
    assert!(out.contains("dashboard"), "help should list dashboard");
}

#[test]
#[ignore = "requires compiled aero-eng binary"]
fn smoke_unknown_command_exit_1() {
    let (code, _) = run_cli(&["definitely-nonexistent"]);
    assert_eq!(code, 1, "unknown should exit 1");
}

#[test]
#[ignore = "requires compiled aero-eng binary"]
fn smoke_doctor_exit_0() {
    let (code, out) = run_cli(&["doctor"]);
    assert!(
        code == 0 || code == 1,
        "doctor should not crash: exit={code}"
    );
    assert!(out.contains("Rust toolchain"), "doctor checks toolchain");
}

#[test]
#[ignore = "requires compiled aero-eng binary"]
fn smoke_gate_list_exit_0() {
    let (code, out) = run_cli(&["gate", "list"]);
    assert_eq!(code, 0, "gate list exit 0");
    assert!(out.contains("filesize"), "gate list shows filesize");
    assert!(out.contains("deps-native"), "gate list shows deps-native");
    // AC2: the B5 gate must be advertised by `gate list`.
    assert!(out.contains("b5"), "gate list shows b5");
}

#[test]
#[ignore = "requires compiled aero-eng binary"]
fn smoke_completion_bash() {
    let (code, out) = run_cli(&["completion", "bash"]);
    assert_eq!(code, 0, "completion bash exit 0");
    assert!(out.contains("complete"), "bash completion output");
    // Tripwire: completion words derive from the registry — bench/dashboard
    // were historically missing from the hand-maintained literal.
    assert!(out.contains("bench"), "completion includes bench");
    assert!(out.contains("dashboard"), "completion includes dashboard");
}

#[test]
#[ignore = "requires compiled aero-eng binary"]
fn smoke_completion_zsh() {
    let (code, out) = run_cli(&["completion", "zsh"]);
    assert_eq!(code, 0, "completion zsh exit 0");
    assert!(out.contains("compdef"), "zsh completion has compdef");
}

#[test]
#[ignore = "requires compiled aero-eng binary"]
fn smoke_completion_fish() {
    let (code, out) = run_cli(&["completion", "fish"]);
    assert_eq!(code, 0, "completion fish exit 0");
    assert!(out.contains("complete"), "fish completion output");
    assert!(out.contains("bench"), "fish completion includes bench");
}

#[test]
#[ignore = "requires compiled aero-eng binary"]
fn smoke_skill_list() {
    let (code, _) = run_cli(&["skill", "list"]);
    assert!(code == 0 || code == 1, "skill list no crash: exit={code}");
}

#[test]
#[ignore = "requires compiled aero-eng binary"]
fn smoke_skill_view() {
    let (code, _) = run_cli(&["skill", "view", "testing"]);
    assert!(code == 0 || code == 1, "skill view no crash: exit={code}");
}

#[test]
#[ignore = "requires compiled aero-eng binary"]
fn smoke_check_available() {
    let (code, _) = run_cli(&["check"]);
    assert!(code == 0 || code == 1, "check no crash: exit={code}");
}

#[test]
#[ignore = "requires compiled aero-eng binary"]
fn smoke_network_help_exit_0() {
    // AC2 corrected leg: `network` must dispatch and advertise relay-probe.
    let (code, out) = run_cli(&["network", "help"]);
    assert_eq!(code, 0, "network help exit 0");
    assert!(
        out.contains("relay-probe"),
        "network help lists relay-probe"
    );
}

#[test]
#[ignore = "requires compiled aero-eng binary"]
fn smoke_network_relay_probe() {
    // AC2 relay-check leg: exit 0 = probe suite PASS, 1 = probe not landed
    // (or a scenario failed) — never a crash or unknown-command path.
    let (code, out) = run_cli(&["network", "relay-probe"]);
    assert!(
        code == 0 || code == 1,
        "network relay-probe should exit 0 or 1: exit={code}"
    );
    assert!(
        out.is_empty() || out.contains("probe"),
        "relay-probe output unexpected: {out}"
    );
}
