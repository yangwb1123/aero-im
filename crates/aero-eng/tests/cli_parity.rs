//! Parity tests: help / completion / registry must all derive from the same
//! single source of truth (`CommandRegistry::collect()` → `commands::all()`).
//!
//! Tripwire: the completion word list was historically a hand-maintained
//! literal in the binary that drifted from the registry — `bench` and
//! `dashboard` were registered but missing from completion. These tests pin
//! the invariant so any future drift (forgot-to-list, dupes, order breaks)
//! fails red in normal `cargo test` runs (no binary spawn, no ignored gate).

use aero_eng::CommandRegistry;

/// The built-in command set as of this direction. Extend when adding a
/// command to `aero-eng/src/commands/` (the test is the forgot-to-list
/// guard).
const BUILTIN_NAMES: &[&str] = &[
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
];

fn registry() -> CommandRegistry {
    CommandRegistry::collect()
}

#[test]
fn registry_collect_contains_every_builtin_command() {
    let names = registry().names();
    for expected in BUILTIN_NAMES {
        assert!(
            names.contains(expected),
            "registry missing {expected:?}; forget to add it to commands::all()? got {names:?}"
        );
    }
}

#[test]
fn registry_collect_names_are_unique() {
    let names = registry().names();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        names.len(),
        "duplicate command names in registry: {names:?}"
    );
}

#[test]
fn help_text_lists_every_command_and_help() {
    let reg = registry();
    let help = reg.help_text();
    for name in reg.names() {
        assert!(help.contains(name), "help text missing {name:?}:\n{help}");
    }
    assert!(
        help.contains("Show this help message"),
        "help text must list the help pseudo-command:\n{help}"
    );
}

#[test]
fn completion_words_cover_every_command_and_help() {
    // Tripwire: the old literal omitted bench/dashboard. Completion must be
    // derived from the registry, never a parallel literal.
    let reg = registry();
    let words_owned = reg.completion_words();
    let words: Vec<&str> = words_owned.split(' ').collect();
    for name in reg.names() {
        assert!(
            words.contains(&name),
            "completion missing {name:?}: {words:?}"
        );
    }
    assert!(
        words.contains(&"help"),
        "completion must include help: {words:?}"
    );
}

#[test]
fn help_order_matches_registration_order() {
    let reg = registry();
    let help = reg.help_text();
    let names = reg.names();
    let mut last = 0usize;
    for name in &names {
        let pattern = format!("  {name} ");
        let pos = help
            .find(&pattern)
            .unwrap_or_else(|| panic!("help text missing line for {name:?}:\n{help}"));
        assert!(
            pos >= last,
            "help order drifted for {name:?} (expected after position {last}):\n{help}"
        );
        last = pos;
    }
}

#[test]
fn completion_order_matches_names_order() {
    let reg = registry();
    let words_owned = reg.completion_words();
    let words: Vec<&str> = words_owned.split(' ').collect();
    let mut names = reg.names();
    names.push("help");
    assert_eq!(words, names, "completion order must equal names() + help");
}

#[test]
fn gate_and_network_subcommands_are_reachable() {
    // AC2 legs: `gate` must be registry-listed (its `list` arm advertises
    // b5) and `network` must exist (its `relay-probe` arm is the corrected
    // relay-check leg). Dispatch-level reachability of the subcommand arms
    // is exercised by the ignored binary smokes + the harness.
    let names = registry().names();
    assert!(names.contains(&"gate"), "gate must be registry-listed");
    assert!(
        names.contains(&"network"),
        "network must be registry-listed"
    );
}

#[tokio::test]
async fn dispatch_routes_builtin_commands() {
    // `aero_eng::dispatch` was previously broken (collect() was a stub);
    // it must now route real commands.
    let result = aero_eng::dispatch(&["network".into(), "network".into(), "help".into()]).await;
    assert!(result.is_ok(), "dispatch network help should succeed");
    let result = aero_eng::dispatch(&["definitely-not-a-command".into()]).await;
    assert!(result.is_err(), "dispatch unknown should error");
}

#[tokio::test]
async fn execute_help_and_subcommand_help_exit_ok() {
    let reg = registry();
    let r = reg.execute("help", &["help".into()]).await;
    assert!(r.is_ok(), "help should dispatch ok");
    let r = reg
        .execute(
            "network",
            &["aero-eng".into(), "network".into(), "help".into()],
        )
        .await;
    assert!(r.is_ok(), "network help should dispatch ok");
    let r = reg
        .execute("gate", &["aero-eng".into(), "gate".into(), "list".into()])
        .await;
    assert!(r.is_ok(), "gate list should dispatch ok");
}
