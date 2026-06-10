//! AUTHZ GUARDRAIL source-lint (ROADMAP 第三版 方向五 鉴权护栏).
//!
//! Tenant isolation in this server lives in per-handler guard calls
//! (`assert_room_access`, `member_role`, …) — one forgotten call in a new
//! feature is a cross-tenant data leak. This test string-scans every handler
//! in `src/*.rs`: an `async fn` that takes a `Path<…>` extractor AND parses a
//! `RoomId`/`WorkspaceId` out of it must contain at least one sanctioned
//! authorization call in its body, or be explicitly allowlisted below with a
//! justification.
//!
//! Hermetic: reads the crate's own sources via `CARGO_MANIFEST_DIR`; no DB,
//! no network, no extra dependencies.

use std::fs;
use std::path::PathBuf;

/// Body markers meaning "this handler resolved a room id from request input".
/// Every module either calls its local `parse_room`/`parse_room_id` helper or
/// `RoomId::from_str` directly — there are no typed `Path<RoomId>` extractors
/// in this codebase (all paths are `Path<String>` + parse).
const ROOM_ID_MARKERS: &[&str] = &["parse_room(", "parse_room_id(", "RoomId::from_str("];

/// Same, for workspace (tenant) ids.
const WORKSPACE_ID_MARKERS: &[&str] = &[
    "parse_workspace(",
    "parse_workspace_id(",
    "parse_ws(",
    "WorkspaceId::from_str(",
];

/// Sanctioned guard calls: each of these strings, appearing in a handler body,
/// is a direct authorization decision. Derived from the actual guard helpers
/// in this crate / `aero-im-core` — verified to reject unauthorized callers.
const SANCTIONED_GUARDS: &[&str] = &[
    // The canonical tenant guard (ImService): workspace membership + room
    // membership + deactivation + 2FA gates, NotFound on unknown room.
    "assert_room_access(",
    // Direct room/workspace membership lookup used as a gate.
    ".is_member(",
    // Workspace role resolution: `None` for non-members; every caller rejects
    // on `None` or feeds the role into an RBAC decision.
    "member_role(",
    // RBAC check over an already-resolved workspace role (Owner/Admin).
    "can_administer",
    // Per-module "caller must be a workspace member" helpers (the
    // saved_searches/directory pattern): member_role + Forbidden on None.
    "assert_member(",
    // Per-module admin gates: assert_admin / assert_admin_or_creator
    // (announcements, deactivation, legal_holds, channel_retention, …).
    "assert_admin",
    // channel_roles: room-owner gate.
    "assert_owner(",
    // join_requests: room-creator-or-workspace-admin decision guard.
    "assert_can_decide(",
    // workspaces/emoji/guests/invitations…: member_role + Forbidden on
    // non-member, returning the role for a follow-up RBAC check.
    "caller_role(",
    // user_groups: membership guard returning the caller's role.
    "require_member(",
    // SCIM: resolves the bearer token to ITS workspace; handler-supplied ids
    // are only honored when equal to the token's tenant (else 404).
    "scim_workspace(",
];

/// Sanctioned delegations: `ImService` methods that take the acting
/// participant FIRST and enforce access INSIDE the service (verified one by
/// one in `aero-im-core/src/service.rs`). A handler that hands the parsed id
/// straight to one of these is guarded by construction.
const SERVICE_ENFORCED_CALLS: &[&str] = &[
    // workspace-membership + guest-confinement + private/archived gates.
    ".join_channel(",
    // self-scoped: removes only the actor; room existence checked first.
    ".leave_channel(",
    // room-membership gate.
    ".archive_channel(",
    // room-membership gate.
    ".set_channel_meta(",
    // room-creator-or-workspace-admin gate.
    ".set_room_post_policy(",
    // assert_room_access inside.
    ".room_post_policy(",
    // workspace-membership gate.
    ".list_workspace_channels(",
    // assert_room_access inside (all three pin operations).
    ".list_pins(",
    ".pin_message(",
    ".unpin_message(",
    // room-membership + post-policy + moderation gates.
    ".send_message(",
];

/// Handlers that parse a room/workspace id but legitimately need NO authz
/// call. Every entry must carry a justification; entries that stop matching a
/// real handler fail the test (stale allowlist).
const ALLOWLIST: &[(&str, &str)] = &[
    // Unstar a channel: owner-scoped SQL (`DELETE … WHERE participant_id =
    // caller`) — only the caller's own favorite row is ever touched and no
    // room data is returned, so room access is irrelevant.
    ("favorites.rs", "remove_favorite"),
    // Sidebar section organization: the SQL resolves the section only when
    // the caller OWNS it, so a foreign section/room id changes nothing and
    // nothing about the room is read back.
    ("channel_sections.rs", "add_channel"),
    ("channel_sections.rs", "remove_channel"),
];

/// `Some(fn_name)` when `line` starts (column 0) a top-level `async fn`.
/// Indented fns (impl blocks, nested test modules) are intentionally skipped —
/// Axum handlers in this crate are free `async fn`s at the top level.
fn top_level_async_fn(line: &str) -> Option<&str> {
    let rest = line
        .strip_prefix("pub(crate) ")
        .or_else(|| line.strip_prefix("pub "))
        .unwrap_or(line);
    let rest = rest.strip_prefix("async fn ")?;
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    (end > 0).then_some(&rest[..end])
}

/// One scanned handler: where it is and whether it passed.
struct Candidate {
    file: String,
    name: String,
    guarded: bool,
}

/// Scan every `src/*.rs` (top level only; `src/bin/` is wiring, not handlers)
/// and return the handlers that take a `Path<…>` extractor and parse a
/// room/workspace id. A handler's text runs from its signature line to the
/// next column-0 `}` (rustfmt closes every top-level fn that way).
fn scan_handlers() -> Vec<Candidate> {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files: Vec<PathBuf> = fs::read_dir(&src)
        .expect("read src dir")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "rs"))
        .collect();
    files.sort();

    let mut out = Vec::new();
    for path in files {
        let file_name = path
            .file_name()
            .expect("file name")
            .to_string_lossy()
            .into_owned();
        let text = fs::read_to_string(&path).expect("read source file");
        let lines: Vec<&str> = text.lines().collect();

        let mut i = 0;
        while i < lines.len() {
            let Some(name) = top_level_async_fn(lines[i]) else {
                i += 1;
                continue;
            };
            let mut j = i + 1;
            while j < lines.len() && lines[j] != "}" {
                j += 1;
            }
            let body = lines[i..(j + 1).min(lines.len())].join("\n");
            let sig = &body[..body.find('{').unwrap_or(body.len())];

            let parses_tenant_id = ROOM_ID_MARKERS
                .iter()
                .chain(WORKSPACE_ID_MARKERS)
                .any(|m| body.contains(m));
            if sig.contains("Path<") && parses_tenant_id {
                let guarded = SANCTIONED_GUARDS
                    .iter()
                    .chain(SERVICE_ENFORCED_CALLS)
                    .any(|g| body.contains(g));
                out.push(Candidate { file: file_name.clone(), name: name.to_owned(), guarded });
            }
            i = j + 1;
        }
    }
    out
}

#[test]
fn every_room_or_workspace_handler_is_authz_guarded() {
    let candidates = scan_handlers();

    // Scanner self-check: if a refactor breaks the string-scan, this test must
    // fail loudly instead of silently linting nothing. 118 handlers matched at
    // calibration time; 80 is a generous floor.
    assert!(
        candidates.len() >= 80,
        "authz lint scanner matched only {} handlers (expected >= 80) — the \
         source scan in tests/authz_lint.rs no longer recognizes handler fns; \
         fix the scanner, do not delete the lint",
        candidates.len()
    );
    assert!(
        candidates.iter().any(|c| c.file == "polls.rs" && c.name == "create_poll"),
        "authz lint scanner lost a known-guarded handler (polls.rs::create_poll) — \
         the scan heuristics in tests/authz_lint.rs have rotted; fix the scanner"
    );

    // Stale-allowlist check: every entry must still name a real, scanned
    // handler, otherwise the exemption is dead weight (or hiding a rename).
    for (file, name) in ALLOWLIST {
        assert!(
            candidates.iter().any(|c| c.file == *file && c.name == *name),
            "stale allowlist entry {file}::{name} in tests/authz_lint.rs — the \
             handler no longer exists (or no longer parses a room/workspace id); \
             remove the entry"
        );
    }

    let offenders: Vec<String> = candidates
        .iter()
        .filter(|c| !c.guarded)
        .filter(|c| !ALLOWLIST.iter().any(|(f, n)| c.file == *f && c.name == *n))
        .map(|c| format!("  {}::{}", c.file, c.name))
        .collect();

    assert!(
        offenders.is_empty(),
        "\nAUTHZ GUARDRAIL: handler(s) take a room/workspace id from the request \
         path but never call a sanctioned authorization guard:\n{}\n\n\
         Fix: call `s.im.assert_room_access(auth.participant_id, room).await?` \
         (room data), or gate on `s.workspaces.member_role(ws, caller)` / a local \
         `assert_member`/`assert_admin` helper (workspace data), BEFORE touching \
         tenant data. If the handler is genuinely public or is authorized another \
         way (owner-scoped SQL, token-derived tenant, service-internal gate), add \
         it to ALLOWLIST / SERVICE_ENFORCED_CALLS in tests/authz_lint.rs WITH a \
         comment justifying why.\n",
        offenders.join("\n")
    );
}

/// The lint must actually catch a future unguarded handler: feed the scanner
/// logic a synthetic offender and a synthetic guarded handler and check the
/// classification, so the marker/guard tables can't drift into matching
/// nothing.
#[test]
fn lint_classifies_synthetic_handlers_correctly() {
    let unguarded = "async fn steal_data(\n    State(s): State<AppState>,\n    \
                     auth: AuthUser,\n    Path(room_str): Path<String>,\n) -> \
                     ApiResult<Json<serde_json::Value>> {\n    let room = \
                     parse_room(&room_str)?;\n    let rows = repo(&s).list(room).await?;\n    \
                     Ok(Json(serde_json::json!({ \"rows\": rows })))\n}";
    let guarded = "async fn list_things(\n    State(s): State<AppState>,\n    \
                   auth: AuthUser,\n    Path(room_str): Path<String>,\n) -> \
                   ApiResult<Json<serde_json::Value>> {\n    let room = \
                   parse_room(&room_str)?;\n    s.im.assert_room_access(auth.participant_id, \
                   room).await?;\n    Ok(Json(serde_json::json!({})))\n}";

    for (body, expect_guarded) in [(unguarded, false), (guarded, true)] {
        let sig = &body[..body.find('{').unwrap()];
        assert!(sig.contains("Path<"), "synthetic handler must look like a handler");
        assert!(
            ROOM_ID_MARKERS.iter().any(|m| body.contains(m)),
            "synthetic handler must parse a room id"
        );
        let guarded = SANCTIONED_GUARDS
            .iter()
            .chain(SERVICE_ENFORCED_CALLS)
            .any(|g| body.contains(g));
        assert_eq!(guarded, expect_guarded, "misclassified synthetic handler");
    }
}
