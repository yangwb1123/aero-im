use super::*;
use std::io::Write;

fn tmp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("create temp dir")
}

fn create_crate_file(dir: &std::path::Path, rel_path: &str, lines: usize) {
    let path = dir.join("crates").join(rel_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let mut f = std::fs::File::create(&path).unwrap();
    for i in 0..lines {
        writeln!(f, "// line {i}").unwrap();
    }
}

// ---- Filesize tests ----

#[test]
fn filesize_ok_for_small_file() {
    let dir = tmp_dir();
    create_crate_file(dir.path(), "aero-foo/src/lib.rs", 10);
    let cfg = EngineeringConfig::default();
    let outcome = check_filesize(dir.path(), &cfg);
    assert!(outcome.is_ok(), "small file should pass: {outcome}");
}

#[test]
fn filesize_warns_above_warn_limit() {
    let dir = tmp_dir();
    create_crate_file(dir.path(), "aero-foo/src/lib.rs", 900);
    let cfg = EngineeringConfig::default();
    let outcome = check_filesize(dir.path(), &cfg);
    assert!(!outcome.is_ok(), "big file should warn: {outcome}");
    assert!(!outcome.is_error(), "warn-level should not be error");
}

#[test]
fn filesize_fails_above_hard_limit() {
    let dir = tmp_dir();
    create_crate_file(dir.path(), "aero-foo/src/lib.rs", 1300);
    let cfg = EngineeringConfig::default();
    let outcome = check_filesize(dir.path(), &cfg);
    assert!(outcome.is_error(), "huge file should error: {outcome}");
}

#[test]
fn filesize_skips_node_modules() {
    let dir = tmp_dir();
    let nm = dir.path().join("web").join("node_modules");
    std::fs::create_dir_all(&nm).unwrap();
    let mut f = std::fs::File::create(nm.join("huge.js")).unwrap();
    for i in 0..5000usize {
        writeln!(f, "// line {i}").unwrap();
    }
    let cfg = EngineeringConfig::default();
    let outcome = check_filesize(dir.path(), &cfg);
    assert!(outcome.is_ok(), "node_modules should be skipped: {outcome}");
}

// ---- Deps tests ----

fn write_cargo(path: &std::path::Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

#[test]
fn parse_workspace_members_extracts_crate_names() {
    let content = r#"
[workspace]
members = [
"crates/aero-common",
"crates/aero-storage",
"crates/aero-server",
]
"#;
    let members = parse_workspace_members(content);
    assert_eq!(members, vec!["aero-common", "aero-storage", "aero-server"]);
}

#[test]
fn parse_workspace_members_inline() {
    let content = r#"
[workspace]
members = ["crates/aero-common", "crates/aero-bus"]
"#;
    let members = parse_workspace_members(content);
    assert_eq!(members, vec!["aero-common", "aero-bus"]);
}

#[test]
fn parse_deps_finds_aero_internal_deps() {
    let content = r#"
[dependencies]
aero-common.workspace = true
aero-storage.workspace = true
serde.workspace = true
tokio = "1"
"#;
    let deps = parse_deps(content);
    assert!(deps.contains(&"aero-common".to_string()));
    assert!(deps.contains(&"aero-storage".to_string()));
    assert!(!deps.contains(&"serde".to_string()));
    assert!(!deps.contains(&"tokio".to_string()));
}

#[test]
fn parse_deps_finds_path_deps() {
    let content = r#"
[dependencies]
aero-common = { path = "../aero-common" }
aero-storage = { path = "../aero-storage" }
"#;
    let deps = parse_deps(content);
    assert!(deps.contains(&"aero-common".to_string()));
    assert_eq!(deps.len(), 2);
}

#[test]
fn check_deps_ok_on_leaf_crate() {
    let dir = tmp_dir();
    let root = dir.path();
    // Workspace Cargo.toml
    write_cargo(
        &root.join("Cargo.toml"),
        r#"
[workspace]
members = ["crates/aero-common"]
"#,
    );
    // aero-common has no internal deps (correct for leaf)
    write_cargo(
        &root.join("crates/aero-common/Cargo.toml"),
        r#"
[package]
name = "aero-common"
[dependencies]
serde.workspace = true
"#,
    );
    let outcome = check_deps(root);
    assert!(
        outcome.is_ok(),
        "leaf crate with only external deps: {outcome}"
    );
}

#[test]
fn check_deps_rejects_illegal_upward_dep() {
    let dir = tmp_dir();
    let root = dir.path();
    write_cargo(
        &root.join("Cargo.toml"),
        r#"
[workspace]
members = ["crates/aero-common", "crates/aero-storage"]
"#,
    );
    // aero-common depends on aero-storage — ILLEGAL (leaf depends on higher layer)
    write_cargo(
        &root.join("crates/aero-common/Cargo.toml"),
        r#"
[package]
name = "aero-common"
[dependencies]
aero-storage.workspace = true
"#,
    );
    write_cargo(
        &root.join("crates/aero-storage/Cargo.toml"),
        r#"
[package]
name = "aero-storage"
[dependencies]
aero-common.workspace = true
"#,
    );
    let outcome = check_deps(root);
    assert!(
        outcome.is_error(),
        "leaf → storage should be illegal: {outcome}"
    );
}

#[test]
fn allowed_deps_admits_aero_cli_and_audit_connector() {
    // Regression (AC4 deps-audit gap): aero-cli and aero-audit-connector must be
    // in ALLOWED_DEPS with exactly their real internal deps. The reverse check is
    // advisory, so over-allowlisting is silent — exact equality is the discipline.
    let entry = |name: &str| ALLOWED_DEPS.iter().find(|(n, _)| *n == name);
    assert_eq!(
        entry("aero-cli"),
        Some(&("aero-cli", &["aero-eng"][..])),
        "aero-cli whitelist must exactly match crates/aero-cli/Cargo.toml deps"
    );
    assert_eq!(
        entry("aero-audit-connector"),
        Some(&(
            "aero-audit-connector",
            &["aero-common", "aero-auth", "aero-storage"][..]
        )),
        "connector whitelist must include its regular storage parity dependency"
    );
}

#[test]
fn parse_deps_ignores_dev_dependencies() {
    let deps = parse_deps(
        "[dependencies]\naero-common.workspace = true\n[dev-dependencies]\naero-audit-connector.workspace = true\n",
    );
    assert_eq!(deps, vec!["aero-common"]);
}

#[test]
fn check_deps_ok_for_newly_registered_members() {
    // Regression (AC4): a member absent from ALLOWED_DEPS was invisible to the
    // audit — every internal dep fired "unknown crate". Registering the member
    // must move it to the normal ILLEGAL/OK path instead.
    let dir = tmp_dir();
    let root = dir.path();
    write_cargo(
        &root.join("Cargo.toml"),
        r#"
[workspace]
members = ["crates/aero-cli", "crates/aero-audit-connector"]
"#,
    );
    // aero-cli depends on aero-eng (allowed); connector on aero-common/aero-auth (allowed)
    write_cargo(
        &root.join("crates/aero-cli/Cargo.toml"),
        r#"
[package]
name = "aero-cli"
[dependencies]
aero-eng.workspace = true
"#,
    );
    write_cargo(
        &root.join("crates/aero-audit-connector/Cargo.toml"),
        r#"
[package]
name = "aero-audit-connector"
[dependencies]
aero-common.workspace = true
aero-auth.workspace = true
"#,
    );
    let outcome = check_deps(root);
    assert!(
        outcome.is_ok(),
        "registered members with allowed deps must pass: {outcome}"
    );
}

#[test]
fn check_workspace_members_ok_for_all_crates() {
    let dir = tmp_dir();
    let root = dir.path();
    // Create workspace Cargo.toml
    write_cargo(
        &root.join("Cargo.toml"),
        r#"
[workspace]
members = ["crates/aero-foo", "crates/aero-bar"]
"#,
    );
    std::fs::create_dir_all(root.join("crates/aero-foo/src")).unwrap();
    std::fs::create_dir_all(root.join("crates/aero-bar/src")).unwrap();
    let outcome = check_workspace_members(root);
    assert!(outcome.is_ok(), "all members present: {outcome}");
}

#[test]
fn check_workspace_members_fails_on_missing() {
    let dir = tmp_dir();
    let root = dir.path();
    write_cargo(
        &root.join("Cargo.toml"),
        r#"
[workspace]
members = ["crates/aero-foo", "crates/aero-missing"]
"#,
    );
    std::fs::create_dir_all(root.join("crates/aero-foo/src")).unwrap();
    let outcome = check_workspace_members(root);
    assert!(outcome.is_error(), "missing member should fail: {outcome}");
}

#[test]
fn check_crate_metadata_ok_for_workspace_crates() {
    let dir = tmp_dir();
    let root = dir.path();
    write_cargo(
        &root.join("Cargo.toml"),
        r#"
[workspace]
members = ["crates/aero-foo"]
"#,
    );
    std::fs::create_dir_all(root.join("crates/aero-foo/src")).unwrap();
    write_cargo(
        &root.join("crates/aero-foo/Cargo.toml"),
        r#"
[package]
name = "aero-foo"
version.workspace = true
edition.workspace = true
license.workspace = true
"#,
    );
    let outcome = check_crate_metadata(root);
    assert!(outcome.is_ok(), "metadata ok: {outcome}");
}

#[test]
fn check_crate_metadata_fails_on_missing_field() {
    let dir = tmp_dir();
    let root = dir.path();
    write_cargo(
        &root.join("Cargo.toml"),
        r#"
[workspace]
members = ["crates/aero-foo"]
"#,
    );
    std::fs::create_dir_all(root.join("crates/aero-foo/src")).unwrap();
    write_cargo(
        &root.join("crates/aero-foo/Cargo.toml"),
        r#"
[package]
name = "aero-foo"
version = "0.1.0"
"#,
    );
    let outcome = check_crate_metadata(root);
    assert!(outcome.is_error(), "missing workspace fields: {outcome}");
}

#[test]
fn check_todos_finds_nothing_in_clean_code() {
    let dir = tmp_dir();
    let root = dir.path();
    let crates = root.join("crates/aero-foo/src");
    std::fs::create_dir_all(&crates).unwrap();
    std::fs::write(crates.join("lib.rs"), "// clean code\npub fn hello() {}\n").unwrap();
    let outcome = check_todos(root);
    assert!(outcome.is_ok(), "no TODOs: {outcome}");
}

#[test]
fn check_todos_finds_todo_comment() {
    let dir = tmp_dir();
    let root = dir.path();
    let crates = root.join("crates/aero-foo/src");
    std::fs::create_dir_all(&crates).unwrap();
    std::fs::write(
        crates.join("lib.rs"),
        "// TODO: implement this later\npub fn maybe() {}\n",
    )
    .unwrap();
    let outcome = check_todos(root);
    assert!(!outcome.is_ok(), "should find TODO: {outcome}");
    if let Some(d) = outcome.detail() {
        assert!(d["total"] == 1, "should find exactly 1 TODO");
    }
}

#[test]
fn check_readme_reports_missing_all() {
    let dir = tmp_dir();
    let root = dir.path();
    write_cargo(
        &root.join("Cargo.toml"),
        r#"
[workspace]
members = ["crates/aero-foo"]
"#,
    );
    std::fs::create_dir_all(root.join("crates/aero-foo/src")).unwrap();
    // No README.md
    let outcome = check_readme(root);
    assert!(outcome.is_error(), "missing README must fail: {outcome}");
    assert!(
        outcome.message().contains("aero-foo"),
        "should mention missing crate: {}",
        outcome.message()
    );
}

#[test]
fn check_readme_ok_when_present() {
    let dir = tmp_dir();
    let root = dir.path();
    write_cargo(
        &root.join("Cargo.toml"),
        r#"
[workspace]
members = ["crates/aero-foo"]
"#,
    );
    std::fs::create_dir_all(root.join("crates/aero-foo/src")).unwrap();
    std::fs::write(root.join("crates/aero-foo/README.md"), "# aero-foo\n").unwrap();
    let outcome = check_readme(root);
    assert!(outcome.is_ok(), "scoped README must pass: {outcome}");
    assert!(
        outcome.message().contains("all"),
        "all present: {}",
        outcome.message()
    );
}

#[test]
fn check_readme_rejects_empty_or_unscoped_heading() {
    let dir = tmp_dir();
    let root = dir.path();
    write_cargo(
        &root.join("Cargo.toml"),
        r#"
[workspace]
members = ["crates/aero-foo"]
"#,
    );
    std::fs::create_dir_all(root.join("crates/aero-foo/src")).unwrap();

    let readme = root.join("crates/aero-foo/README.md");
    std::fs::write(&readme, "\n\n").unwrap();
    assert!(check_readme(root).is_error(), "empty README must fail");

    std::fs::write(&readme, "# unrelated\n").unwrap();
    assert!(
        check_readme(root).is_error(),
        "README for another crate must fail"
    );
}
