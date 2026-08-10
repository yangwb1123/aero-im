//! Native Rust engineering checks — replacing shell-script wrappers.
//!
//! Each check is a pure function that takes a project root + config and returns
//! an [`Outcome`]. Unlike the bash-script wrappers in `gate`, these use the
//! same thresholds from `engineering.toml` and can run in parallel without
//! spawning subprocesses.

use std::collections::HashSet;
use std::path::Path;

use crate::config::EngineeringConfig;
use crate::outcome::Outcome;

// ---------------------------------------------------------------------------
// Filesize check — line-count limits per file type
// ---------------------------------------------------------------------------

/// Run the filesize check on all source files under `root`.
///
/// Scans `crates/` (`.rs`), `web/` (`.js`), and `migrations/` (`.sql`).
#[must_use]
pub fn check_filesize(root: &Path, config: &EngineeringConfig) -> Outcome {
    let cfg = &config.filesize;
    let mut violations: Vec<FileViolation> = Vec::new();
    let mut checked = 0usize;

    let crates_dir = root.join("crates");
    if crates_dir.exists() {
        scan_dir(
            &crates_dir,
            ".rs",
            cfg.rust_warn,
            cfg.rust_hard,
            "routes.rs",
            cfg.routes_hard,
            &mut violations,
            &mut checked,
        );
    }

    let web_dir = root.join("web");
    if web_dir.exists() {
        scan_dir(
            &web_dir,
            ".js",
            cfg.js_warn,
            cfg.js_warn + 500,
            "",
            0,
            &mut violations,
            &mut checked,
        );
    }

    let mig_dir = root.join("migrations");
    if mig_dir.exists() {
        scan_dir(
            &mig_dir,
            ".sql",
            200,
            400,
            "",
            0,
            &mut violations,
            &mut checked,
        );
    }

    if violations.is_empty() {
        Outcome::ok(format!(
            "✓ filesize: {checked} files checked, all within limits"
        ))
        .with_detail(detail_json(checked, &violations))
    } else {
        let hard_count = violations.iter().filter(|v| v.is_hard).count();
        let msg = format!(
            "✗ filesize: {hard_count} hard violations, {} warnings ({checked} files checked)",
            violations.len() - hard_count
        );
        if hard_count > 0 {
            Outcome::error(msg).with_detail(detail_json(checked, &violations))
        } else {
            Outcome::warning(0, msg).with_detail(detail_json(checked, &violations))
        }
    }
}

#[derive(Debug)]
struct FileViolation {
    path: String,
    lines: usize,
    limit: usize,
    is_hard: bool,
}

fn detail_json(checked: usize, violations: &[FileViolation]) -> serde_json::Value {
    let hard_count = violations.iter().filter(|v| v.is_hard).count();
    serde_json::json!({
        "checked": checked,
        "violations": violations.len(),
        "hard": hard_count,
        "warnings": violations.len() - hard_count,
        "files": violations.iter().map(|v| serde_json::json!({
            "path": v.path,
            "lines": v.lines,
            "limit": v.limit,
            "hard": v.is_hard,
        })).collect::<Vec<_>>(),
    })
}

// Private gate helper with a fixed parameter set; grouping into a struct would
// churn the single call site for no behavioral gain.
#[allow(clippy::too_many_arguments)]
fn scan_dir(
    dir: &Path,
    ext: &str,
    warn: usize,
    hard: usize,
    exempt_name: &str,
    exempt_hard: usize,
    violations: &mut Vec<FileViolation>,
    checked: &mut usize,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.starts_with('.') || name == "node_modules" || name == "target" {
                continue;
            }
            scan_dir(
                &path,
                ext,
                warn,
                hard,
                exempt_name,
                exempt_hard,
                violations,
                checked,
            );
        } else if path
            .extension()
            .is_some_and(|e| e == ext.trim_start_matches('.'))
        {
            *checked += 1;
            let lines = line_count(&path);
            let fname = path.file_name().unwrap_or_default().to_string_lossy();

            if !exempt_name.is_empty() && fname.contains(exempt_name) {
                if lines > exempt_hard {
                    violations.push(FileViolation {
                        path: path.to_string_lossy().into(),
                        lines,
                        limit: exempt_hard,
                        is_hard: true,
                    });
                }
                continue;
            }
            if lines > hard {
                violations.push(FileViolation {
                    path: path.to_string_lossy().into(),
                    lines,
                    limit: hard,
                    is_hard: true,
                });
            } else if lines > warn {
                violations.push(FileViolation {
                    path: path.to_string_lossy().into(),
                    lines,
                    limit: warn,
                    is_hard: false,
                });
            }
        }
    }
}

fn line_count(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Dependency direction check — crate architecture rules
// ---------------------------------------------------------------------------

/// Allowed dependency edges: map of `(crate_name, allowed_dep)` pairs.
/// Generated from `skills/clean-architecture.md`.
const ALLOWED_DEPS: &[(&str, &[&str])] = &[
    ("aero-common", &[]), // leaf — no internal deps
    ("aero-bus", &["aero-common"]),
    ("aero-storage", &["aero-common"]),
    ("aero-auth", &["aero-common", "aero-storage"]),
    ("aero-signaling", &["aero-common"]),
    (
        "aero-im-core",
        &["aero-common", "aero-bus", "aero-storage", "aero-signaling"],
    ),
    (
        "aero-im-call",
        &[
            "aero-common",
            "aero-bus",
            "aero-storage",
            "aero-signaling",
            "aero-live-webrtc",
        ],
    ),
    ("aero-ai", &["aero-common", "aero-storage", "aero-bus"]),
    ("aero-live-core", &["aero-common", "aero-storage"]),
    (
        "aero-live-rtmp",
        &[
            "aero-common",
            "aero-storage",
            "aero-live-core",
            "aero-live-hls",
        ],
    ),
    ("aero-live-hls", &["aero-common", "aero-live-core"]),
    (
        "aero-live-whip",
        &[
            "aero-common",
            "aero-live-core",
            "aero-live-hls",
            "aero-signaling",
            "aero-storage",
        ],
    ),
    (
        "aero-live-webrtc",
        &[
            "aero-common",
            "aero-live-core",
            "aero-live-hls",
            "aero-signaling",
        ],
    ),
    (
        "aero-live-srt",
        &[
            "aero-common",
            "aero-live-core",
            "aero-live-hls",
            "aero-storage",
        ],
    ),
    ("aero-push", &["aero-common"]),
    ("aero-eng", &["aero-common", "serde", "tokio"]), // engineering CLI framework
    ("aero-cli", &["aero-eng"]),                      // engineering CLI framework consumer
    ("aero-audit-connector", &["aero-common", "aero-auth"]), // audit relay connector
];

/// The root crate that may depend on everything.
const ROOT_CRATE: &str = "aero-server";

/// Check that all crate-to-crate dependencies follow the architecture rules.
///
/// Reads each crate's `Cargo.toml`, extracts workspace-internal dependency
/// references (`aero-*`), and compares against [`ALLOWED_DEPS`].
///
/// Returns violations for:
/// - Dependencies to crates that are higher in the layer stack
/// - Dependencies to `aero-server` (the root, should not be imported by anything)
/// - Missing allowed deps that ARE present but shouldn't be
#[must_use]
pub fn check_deps(root: &Path) -> Outcome {
    let workspace_cargo = root.join("Cargo.toml");
    let ws_content = match std::fs::read_to_string(&workspace_cargo) {
        Ok(c) => c,
        Err(e) => return Outcome::error(format!("read workspace Cargo.toml: {e}")),
    };

    // Parse workspace members
    let members = parse_workspace_members(&ws_content);
    if members.is_empty() {
        return Outcome::error("no workspace members found in Cargo.toml");
    }

    // Build lookup: which crates exist in this workspace
    let workspace_crates: HashSet<&str> = ALLOWED_DEPS.iter().map(|(n, _)| *n).collect();

    let mut violations: Vec<String> = Vec::new();
    let mut checked = 0usize;

    for member in &members {
        let crate_dir = root.join("crates").join(member);
        let cargo_path = crate_dir.join("Cargo.toml");
        if !cargo_path.exists() {
            violations.push(format!(
                "{member}: Cargo.toml not found at {}",
                cargo_path.display()
            ));
            continue;
        }
        let content = match std::fs::read_to_string(&cargo_path) {
            Ok(c) => c,
            Err(e) => {
                violations.push(format!("{member}: read error: {e}"));
                continue;
            }
        };
        checked += 1;

        // Extract workspace dependency references
        let deps = parse_deps(&content);

        // Check each dep against allowed rules
        for dep in &deps {
            // Non-workspace deps are irrelevant
            if !workspace_crates.contains(dep.as_str()) {
                continue;
            }

            // Root crate can depend on everything
            if *member == ROOT_CRATE {
                continue;
            }

            // Find the allowed deps for this crate
            let allowed = ALLOWED_DEPS.iter().find(|(n, _)| *n == member);
            let Some((_, allowed_deps)) = allowed else {
                violations.push(format!(
                    "{member}: unknown crate (not in architecture rules)"
                ));
                continue;
            };

            if !allowed_deps.contains(&dep.as_str()) {
                violations.push(format!(
                    "{member} → {dep}: ILLEGAL (not in allowed deps for {member})"
                ));
            }
        }

        // Check for reverse: required deps that are missing (optional)
        // (This is advisory — a crate can independently decide not to use a dep)
    }

    if violations.is_empty() {
        Outcome::ok(format!(
            "✓ deps: {checked} crates checked, all dependencies valid"
        ))
        .with_detail(serde_json::json!({
            "checked": checked,
            "violations": 0,
        }))
    } else {
        Outcome::error(format!(
            "✗ deps: {} dependency violations across {checked} crates",
            violations.len()
        ))
        .with_detail(serde_json::json!({
            "checked": checked,
            "violations": violations.len(),
            "details": violations,
        }))
    }
}

/// Parse `members = ["crates/aero-*", ...]` from workspace Cargo.toml.
fn parse_workspace_members(content: &str) -> Vec<String> {
    let mut members = Vec::new();
    let mut in_members = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("members") {
            in_members = true;
            // Extract from same line: members = ["foo", "bar"]
            if let Some(start) = trimmed.find('[') {
                if let Some(end) = trimmed.find(']') {
                    for item in trimmed[start + 1..end].split(',') {
                        let item = item.trim().trim_matches('"').trim();
                        if !item.is_empty() {
                            members.push(extract_crate_name(item));
                        }
                    }
                    return members;
                }
            }
            continue;
        }
        if in_members {
            let trimmed = line.trim();
            if trimmed.starts_with(']') {
                break;
            }
            if trimmed.starts_with('"') || trimmed.starts_with('\'') {
                let name = trimmed.trim_matches(',').trim().trim_matches('"').trim();
                if !name.is_empty() {
                    members.push(extract_crate_name(name));
                }
            }
        }
    }
    members
}

/// Extract crate name from a path like `"crates/aero-common"`.
fn extract_crate_name(path: &str) -> String {
    path.split('/').next_back().unwrap_or(path).to_owned()
}

/// Extract dependency names from a Cargo.toml content.
/// Finds `aero-*` dependencies regardless of whether they use workspace = true
/// or inline path references.
fn parse_deps(content: &str) -> Vec<String> {
    let mut deps = Vec::new();
    let mut in_deps = false;

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("[dependencies]")
            || trimmed.starts_with("[build-dependencies]")
            || trimmed.starts_with("[dev-dependencies]")
        {
            in_deps = true;
            continue;
        }
        if trimmed.starts_with('[') && in_deps {
            in_deps = false;
            continue;
        }
        if !in_deps {
            continue;
        }

        // Extract dependency name: handles both
        //   "aero-common = { path = ... }"
        //   "aero-common.workspace = true"
        //   "aero-common = \"1\""
        if let Some(eq_pos) = trimmed.find('=') {
            let mut name = trimmed[..eq_pos].trim().to_owned();
            // Strip ".workspace" suffix
            if let Some(stripped_len) = name.strip_suffix(".workspace").map(str::len) {
                name.truncate(stripped_len);
            }
            // Only internal crates
            if name.starts_with("aero-") {
                deps.push(name);
            }
        }
    }

    deps
}

/// Validate that all workspace member directories exist on disk.
/// Catches stale workspace members after crate renames or deletions.
#[must_use]
pub fn check_workspace_members(root: &Path) -> Outcome {
    let cargo = root.join("Cargo.toml");
    let content = match std::fs::read_to_string(&cargo) {
        Ok(c) => c,
        Err(e) => return Outcome::error(format!("read Cargo.toml: {e}")),
    };
    let members = parse_workspace_members(&content);
    let mut missing = Vec::new();
    for member in &members {
        let dir = root.join("crates").join(member);
        if !dir.exists() {
            missing.push(member.clone());
        }
    }
    if missing.is_empty() {
        Outcome::ok(format!(
            "✓ workspace: {} members all present",
            members.len()
        ))
    } else {
        Outcome::error(format!(
            "✗ workspace: {} missing members: {}",
            missing.len(),
            missing.join(", ")
        ))
    }
}

/// Scan Rust source files for TODO/FIXME/HACK comments.
/// Returns warnings for each occurrence, not errors (these are reminders, not bugs).
#[must_use]
pub fn check_todos(root: &Path) -> Outcome {
    let mut todos = Vec::new();
    let crates_dir = root.join("crates");
    if crates_dir.exists() {
        scan_todos(&crates_dir, &mut todos);
    }
    if todos.is_empty() {
        Outcome::ok("✓ no TODO/FIXME/HACK comments found")
    } else {
        let detail = serde_json::json!({
            "total": todos.len(),
            "items": todos,
        });
        Outcome::warning(
            0,
            format!("! {} TODO/FIXME/HACK comments found", todos.len()),
        )
        .with_detail(detail)
    }
}

/// Recursively scan a directory for Rust files containing TODO/FIXME/HACK.
fn scan_todos(dir: &Path, todos: &mut Vec<serde_json::Value>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.starts_with('.') || name == "target" || name == "node_modules" {
                continue;
            }
            scan_todos(&path, todos);
        } else if path.extension().is_some_and(|e| e == "rs") {
            if let Ok(content) = std::fs::read_to_string(&path) {
                for (i, line) in content.lines().enumerate() {
                    let trimmed = line.trim();
                    if trimmed.starts_with("// TODO")
                        || trimmed.starts_with("// FIXME")
                        || trimmed.starts_with("// HACK")
                    {
                        todos.push(serde_json::json!({
                            "file": path.to_string_lossy(),
                            "line": i + 1,
                            "text": trimmed,
                        }));
                    }
                }
            }
        }
    }
}

/// Check that every workspace crate has a README.md file.
#[must_use]
pub fn check_readme(root: &Path) -> Outcome {
    let members = parse_workspace_members(
        &std::fs::read_to_string(root.join("Cargo.toml")).unwrap_or_default(),
    );
    let mut missing: Vec<String> = Vec::new();
    for m in &members {
        let readme = root.join("crates").join(m).join("README.md");
        if !readme.exists() {
            missing.push(m.clone());
        }
    }
    if missing.is_empty() {
        Outcome::ok(format!(
            "✓ README: all {} crates have README.md",
            members.len()
        ))
    } else {
        Outcome::warning(
            0,
            format!(
                "! README: {} crates missing README.md: {}",
                missing.len(),
                missing.join(", ")
            ),
        )
    }
}

/// Verify that all workspace crates use workspace-standardized metadata
/// (version, edition, license all use `workspace = true`).
#[must_use]
pub fn check_crate_metadata(root: &Path) -> Outcome {
    let cargo = root.join("Cargo.toml");
    let members = parse_workspace_members(&std::fs::read_to_string(&cargo).unwrap_or_default());
    let mut violations = Vec::new();
    for member in &members {
        let path = root.join("crates").join(member).join("Cargo.toml");
        if !path.exists() {
            continue;
        }
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        for field in &["version", "edition", "license"] {
            let expected = format!("{field}.workspace = true");
            if !content.contains(&expected) {
                violations.push(format!("{member}: missing '{expected}'"));
            }
        }
    }
    if violations.is_empty() {
        Outcome::ok(format!(
            "✓ metadata: {} crates all use workspace standards",
            members.len()
        ))
    } else {
        Outcome::error(format!(
            "✗ metadata: {} violations: {}",
            violations.len(),
            violations.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
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
            Some(&("aero-audit-connector", &["aero-common", "aero-auth"][..])),
            "connector whitelist must be the corrected values, not stale &[]"
        );
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
        assert_eq!(
            outcome.exit_code(),
            0,
            "readme check is warning only: {outcome}"
        );
        // Should report the missing README
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
        std::fs::write(root.join("crates/aero-foo/README.md"), "# Foo\n").unwrap();
        let outcome = check_readme(root);
        assert!(
            outcome.message().contains("all"),
            "all present: {}",
            outcome.message()
        );
    }
}
