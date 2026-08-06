//! AUTHZ GUARDRAIL source-lint (ROADMAP 第三版 方向五 鉴权护栏).
//!
//! Tenant isolation in this server lives in per-handler guard calls
//! (`assert_room_access`, `effective_member_role`, …) — one forgotten call in a new
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
    // Channel routes use the canonical room guard above and then require the
    // exact `RoomKind::Channel`; argument order remains participant, room.
    "assert_channel_access(",
    // Effective workspace role resolution also rejects deleted/deactivated
    // callers and members missing mandatory 2FA.
    "effective_member_role(",
    // Shared server helpers over effective_member_role / its batch equivalent.
    "assert_effective_workspace_member(",
    "assert_effective_workspace_members(",
    // RBAC check over an already-resolved workspace role (Owner/Admin).
    "can_administer",
    // Per-module "caller must be a workspace member" helpers (the
    // saved_searches/directory pattern): effective role + Forbidden on None.
    "assert_member(",
    // Per-module admin gates: assert_admin / assert_admin_or_creator
    // (announcements, deactivation, legal_holds, channel_retention, …).
    "assert_admin",
    // channel_roles: room-owner gate.
    "assert_owner(",
    // join_requests: room-creator-or-workspace-admin decision guard.
    "assert_can_decide(",
    // org_chart: selected-workspace effective membership and self/admin
    // management decisions.
    "assert_effective_member(",
    "assert_can_manage(",
    // workspaces/emoji/guests/invitations…: effective role + Forbidden on
    // non-member, returning the role for a follow-up RBAC check.
    "caller_role(",
    // user_groups: membership guard returning the caller's role.
    "require_member(",
    // SCIM: resolves the bearer token to ITS workspace; handler-supplied ids
    // are only honored when equal to the token's tenant (else 404).
    "scim_workspace(",
];

/// Sanctioned delegations: exact `ImService`/storage methods that bind the
/// acting participant and enforce access inside the delegated operation,
/// verified one by one at their definitions. Room-facing service calls keep
/// the canonical participant-first argument order. A handler that hands the
/// parsed id straight to one of these is guarded by construction.
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
    // Workspace guest mutations lock and re-check the caller's current
    // owner/admin membership, target membership and room tenancy in the same
    // transaction that writes both workspace/room edges.
    ".add_guest_authorized(",
    ".remove_guest_authorized(",
    // User-group mutations likewise resolve the current workspace role (or
    // group creator), group tenant and target membership under row locks before
    // applying the mutation. These names are intentionally exact rather than a
    // broad `_authorized` marker, so an unrelated method cannot bypass the lint.
    ".create_authorized(",
    ".delete_authorized(",
    ".add_member_authorized(",
    ".remove_member_authorized(",
    // Deactivation governance resolves and locks the effective caller role,
    // target role, hierarchy, and audit event in the storage transaction.
    ".deactivate_authorized(",
    ".reactivate_authorized(",
    // SCIM credential minting and revocation lock the token/workspace and
    // re-check the effective Owner/Admin before changing durable credentials.
    ".create_token_authorized(",
    ".revoke_token_authorized(",
    // The inventory read takes the same workspace governance lock and performs
    // the same effective Owner/Admin recheck before returning credential data.
    ".list_tokens_authorized(",
    // These service methods delegate to transaction-owned RoomRepo mutations:
    // workspace -> room -> membership locks, effective-access recheck, update,
    // then commit. Keep the exact names so a generic setter cannot pass.
    ".set_channel_slowmode(",
    ".set_channel_reaction_limit(",
    // Global session termination locks the workspace and validates an effective
    // owner plus target membership before revoking the participant's sessions.
    ".revoke_workspace_member_sessions_authorized(",
    // Enterprise security settings and allowlist mutations use the shared
    // workspace governance lock and effective-admin recheck in storage.
    ".set_require_2fa_authorized(",
    ".set_region_code_authorized(",
    ".add_authorized(",
    ".remove_authorized(",
    // Rate-tier changes are Owner-only. The storage transaction locks the
    // workspace and rechecks the actor's current effective owner role before
    // updating, so the handler intentionally avoids a stale preflight read.
    ".set_rate_tier_authorized(",
    // AI DLQ requeue binds job id + workspace and rechecks the current
    // effective Owner/Admin under the workspace governance lock.
    ".requeue_for_workspace_authorized(",
    // Canvas/op/bookmark repositories bind every opaque resource id to the
    // parsed room and hold canonical effective live-channel access through the
    // read/write transaction. Keep the exact feature-specific method names.
    ".create_canvas_authorized(",
    ".get_canvas_authorized(",
    ".list_canvases_authorized(",
    ".update_canvas_authorized(",
    ".delete_canvas_authorized(",
    ".append_canvas_op_authorized(",
    ".list_canvas_ops_authorized(",
    ".add_channel_bookmark_authorized(",
    ".get_channel_bookmark_authorized(",
    ".list_channel_bookmarks_authorized(",
    ".update_channel_bookmark_authorized(",
    ".delete_channel_bookmark_authorized(",
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

/// Deliberate raw workspace-role reads. These are target/governance lookups,
/// never authorization of the current request caller.
const RAW_MEMBER_ROLE_ALLOWLIST: &[(&str, &str)] = &[];

/// Deliberate raw membership reads. These inspect an internal target, never
/// authorize an HTTP/WS caller.
const RAW_IS_MEMBER_ALLOWLIST: &[(&str, &str)] =
    &[("agent_bot.rs", "state.rooms.is_member(room, bot.id)")];

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
                out.push(Candidate {
                    file: file_name.clone(),
                    name: name.to_owned(),
                    guarded,
                });
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
        candidates
            .iter()
            .any(|c| c.file == "polls.rs" && c.name == "create_poll"),
        "authz lint scanner lost a known-guarded handler (polls.rs::create_poll) — \
         the scan heuristics in tests/authz_lint.rs have rotted; fix the scanner"
    );

    // Stale-allowlist check: every entry must still name a real, scanned
    // handler, otherwise the exemption is dead weight (or hiding a rename).
    for (file, name) in ALLOWLIST {
        assert!(
            candidates
                .iter()
                .any(|c| c.file == *file && c.name == *name),
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
         (room data), or gate on `s.workspaces.effective_member_role(ws, caller)` / a local \
         `assert_member`/`assert_admin` helper (workspace data), BEFORE touching \
         tenant data. If the handler is genuinely public or is authorized another \
         way (owner-scoped SQL, token-derived tenant, service-internal gate), add \
         it to ALLOWLIST / SERVICE_ENFORCED_CALLS in tests/authz_lint.rs WITH a \
         comment justifying why.\n",
        offenders.join("\n")
    );
}

/// Recursively collect `src/**/*.rs` (skipping `src/bin/` wiring): the recall
/// entry points live in subdirectories (`routes/handlers/`, `ws/ws_impl/`),
/// unlike the top-level handlers the authz scan above targets.
fn collect_src_files() -> Vec<PathBuf> {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    let mut dirs = vec![src];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).expect("read src dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name != "bin") {
                    dirs.push(path);
                }
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// Rate-gate regression scan (security review stage 02/06, finding F3): every
/// entry point that invokes the ~8-10-query recall mutation (WS `handle_text`
/// arm + REST `recall_message`) must charge the room's workspace rate budget
/// (`check_ws_rate_room`) first — the same gate shape as edit. A handler that
/// calls `.recall_message(` without the gate in its body fails this lint, so
/// the fix cannot silently regress.
///
/// Gate S1 (rate-gate `DoS` amplifier, security review round 2): the role gate
/// lives INSIDE the preflight, so the lint also requires the arm to call
/// `assert_message_recall_preflight(` and `check_ws_rate_room(` IN THAT ORDER
/// before the mutation — budget may only be charged for callers the preflight
/// has already authorized (author or room owner/admin). Reordering or removing
/// either step fails the lint, pinning "no charge without the role gate".
///
/// Hermetic: reads the crate's own sources via `CARGO_MANIFEST_DIR`; no DB,
/// no network, no extra dependencies.
#[test]
fn every_recall_entry_point_charges_ws_rate_budget() {
    let mut matched = 0;
    let mut offenders = Vec::new();
    for path in collect_src_files() {
        let file_name = path
            .file_name()
            .expect("file name")
            .to_string_lossy()
            .into_owned();
        let text = fs::read_to_string(&path).expect("read source file");
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if !line.contains(".recall_message(") {
                continue;
            }
            matched += 1;
            // Enclosing region: the WS `ClientFrame::RecallMessage` arm, or
            // (REST) the top-level `async fn`, through the call line. Scoping
            // to the ARM matters: the WS dispatch fn contains other rate-gated
            // arms (edit), which must not mask a bare recall call.
            let arm_start = (0..=i)
                .rev()
                .find(|&k| lines[k].contains("ClientFrame::RecallMessage"))
                .unwrap_or_else(|| {
                    (0..=i)
                        .rev()
                        .find(|&k| {
                            let rest = lines[k]
                                .strip_prefix("pub(crate) ")
                                .or_else(|| lines[k].strip_prefix("pub "))
                                .or_else(|| lines[k].strip_prefix("pub(super) "))
                                .unwrap_or(lines[k]);
                            rest.starts_with("async fn ")
                        })
                        .expect("recall_message call sits inside a top-level async fn")
                });
            let region = lines[arm_start..=i].join("\n");
            // Gate S1: budget may only be charged AFTER the role-gated
            // preflight has authorized the caller. `region.find` order check
            // pins preflight → charge → mutation; removing either step (or
            // charging before the preflight) fails the lint.
            let preflight_idx = region.find("assert_message_recall_preflight(");
            let charge_idx = region.find("check_ws_rate_room(");
            match (preflight_idx, charge_idx) {
                (Some(p), Some(c)) if p < c => {}
                _ => offenders.push(format!(
                    "  {file_name} (call at line {}): gate chain must be \
                     `assert_message_recall_preflight` THEN `check_ws_rate_room` \
                     before `recall_message` (S1: budget may only be charged for \
                     role-gated callers)",
                    i + 1
                )),
            }
        }
    }

    // Scanner self-check: if a refactor breaks the scan, this test must fail
    // loudly instead of silently linting nothing (2 call sites calibrated: WS
    // frame arm + REST handler).
    assert!(
        matched >= 2,
        "recall rate-gate scanner matched only {matched} `.recall_message(` call \
         site(s) (expected >= 2) — the source scan in tests/authz_lint.rs no \
         longer recognizes the recall entry points; fix the scanner"
    );
    assert!(
        offenders.is_empty(),
        "\nRATE-GATE GUARDRAIL (F3/S1): handler(s) invoke `recall_message` without \
         the role-gated charge sequence (preflight → `check_ws_rate_room` → \
         mutation):\n{}\n\n\
         Fix: resolve the room via `assert_message_recall_preflight` (which \
         rejects non-author members with Forbidden BEFORE any charge) and call \
         `check_ws_rate_room` AFTER it, BEFORE `recall_message` (same shape as \
         edit).",
        offenders.join("\n")
    );
}

/// A bare membership role ignores account deletion, workspace deactivation,
/// and mandatory 2FA. Keep raw reads out of request authorization; the sole
/// exception is an effective admin inspecting a target membership so they can
/// revoke that target's sessions even after deactivation.
#[test]
fn raw_workspace_role_reads_are_precisely_allowlisted() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut pending = vec![src.clone()];
    let mut sources = Vec::new();
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).expect("read server source directory") {
            let path = entry.expect("source entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                sources.push(path);
            }
        }
    }
    sources.sort();

    let mut offenders = Vec::new();
    let mut allowlist_hits = vec![0_usize; RAW_MEMBER_ROLE_ALLOWLIST.len()];
    for path in sources {
        let relative = path
            .strip_prefix(&src)
            .expect("source path under source root")
            .to_string_lossy()
            .replace('\\', "/");
        let text = fs::read_to_string(&path).expect("read source file");
        for (line_index, line) in text.lines().enumerate() {
            if !line.contains(".member_role(") {
                continue;
            }
            let allowed = RAW_MEMBER_ROLE_ALLOWLIST
                .iter()
                .enumerate()
                .find(|(_, (file, needle))| relative == *file && line.contains(needle));
            if let Some((index, _)) = allowed {
                allowlist_hits[index] += 1;
            } else {
                offenders.push(format!("  {relative}:{}", line_index + 1));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "\nAUTHZ GUARDRAIL: raw `WorkspaceRepo::member_role` reads are forbidden \
         in server request paths because they bypass account deletion, workspace \
         deactivation, and mandatory 2FA:\n{}\n\nUse \
         `effective_member_role` for the current caller. Add an exception only \
         for a precisely documented target/governance lookup.\n",
        offenders.join("\n")
    );
    for ((file, needle), hits) in RAW_MEMBER_ROLE_ALLOWLIST.iter().zip(allowlist_hits) {
        assert_eq!(
            hits, 1,
            "raw member-role allowlist entry `{file}` / `{needle}` matched {hits} \
             times; keep every exception precise and remove stale entries"
        );
    }
}

/// A bare `is_member` result ignores account deletion, workspace deactivation,
/// and mandatory 2FA. Keep it unavailable as a generic request guard.
#[test]
fn raw_membership_reads_are_precisely_allowlisted() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut pending = vec![src.clone()];
    let mut sources = Vec::new();
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).expect("read server source directory") {
            let path = entry.expect("source entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                sources.push(path);
            }
        }
    }
    sources.sort();

    let mut offenders = Vec::new();
    let mut allowlist_hits = vec![0_usize; RAW_IS_MEMBER_ALLOWLIST.len()];
    for path in sources {
        let relative = path
            .strip_prefix(&src)
            .expect("source path under source root")
            .to_string_lossy()
            .replace('\\', "/");
        let text = fs::read_to_string(&path).expect("read source file");
        for (line_index, line) in text.lines().enumerate() {
            if !line.contains(".is_member(") {
                continue;
            }
            let allowed = RAW_IS_MEMBER_ALLOWLIST
                .iter()
                .enumerate()
                .find(|(_, (file, needle))| relative == *file && line.contains(needle));
            if let Some((index, _)) = allowed {
                allowlist_hits[index] += 1;
            } else {
                offenders.push(format!("  {relative}:{}", line_index + 1));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "\nAUTHZ GUARDRAIL: raw `is_member` reads are not caller authorization; \
         use `assert_room_access` or `effective_member_role`:\n{}",
        offenders.join("\n")
    );
    for ((file, needle), hits) in RAW_IS_MEMBER_ALLOWLIST.iter().zip(allowlist_hits) {
        assert_eq!(
            hits, 1,
            "raw is-member allowlist `{file}` / `{needle}` matched {hits} times"
        );
    }
}

fn top_level_fn_body<'a>(source: &'a str, name: &str) -> &'a str {
    let marker = format!("async fn {name}(");
    let start = source
        .find(&marker)
        .unwrap_or_else(|| panic!("missing function {name}"));
    let rest = &source[start..];
    let end = rest
        .find("\n}\n")
        .unwrap_or_else(|| panic!("missing end of function {name}"));
    &rest[..end + 2]
}

/// Pin the ordering on the high-risk room entry points that do not carry a room
/// id until after find-or-create/listing. This complements the generic path-id
/// scanner, which cannot infer authorization from `/api/dm`'s body/target shape.
#[test]
fn dm_and_room_listing_guards_precede_data_access() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let dm = fs::read_to_string(src.join("dm.rs")).unwrap();
    let group = fs::read_to_string(src.join("group_dm.rs")).unwrap();
    let rooms = fs::read_to_string(src.join("routes/handlers/rooms.rs")).unwrap();
    let ws_frame = fs::read_to_string(src.join("ws/ws_impl/frame.rs")).unwrap();

    for (body, guard, access, label) in [
        (
            top_level_fn_body(&dm, "open_dm"),
            "assert_effective_workspace_members(",
            ".find_or_create_in_workspace(",
            "open_dm",
        ),
        (
            top_level_fn_body(&dm, "list_dms"),
            "assert_effective_workspace_member(",
            ".rooms_for_in_workspace(",
            "list_dms",
        ),
        (
            top_level_fn_body(&group, "open_group_dm"),
            "assert_effective_workspace_members(",
            ".find_or_create_in_workspace(",
            "open_group_dm",
        ),
        (
            top_level_fn_body(&group, "list_group_dms"),
            "assert_effective_workspace_member(",
            ".list_for_participant_in_workspace(",
            "list_group_dms",
        ),
        (
            top_level_fn_body(&group, "set_group_dm_name"),
            "assert_room_access(",
            ".patch_metadata(",
            "set_group_dm_name",
        ),
        (
            top_level_fn_body(&rooms, "list_rooms"),
            "assert_effective_workspace_member(",
            ".rooms_for_in_workspace(",
            "list_rooms scoped branch",
        ),
    ] {
        let guard_at = body
            .find(guard)
            .unwrap_or_else(|| panic!("{label} lost guard {guard}"));
        let access_at = body
            .find(access)
            .unwrap_or_else(|| panic!("{label} lost access marker {access}"));
        assert!(
            guard_at < access_at,
            "{label} must guard before touching room/workspace data"
        );
    }

    let join_start = ws_frame.find("ClientFrame::JoinRoom").unwrap();
    let join_end = ws_frame[join_start..]
        .find("ClientFrame::SendMessage")
        .map(|offset| join_start + offset)
        .unwrap();
    let join = &ws_frame[join_start..join_end];
    assert!(
        join.contains("assert_room_access("),
        "WS JoinRoom must use the canonical room guard"
    );
    assert!(
        !join.contains(".is_member("),
        "WS JoinRoom must not fall back to retained room membership"
    );
}

/// Pin the exact guards/delegations behind the channel and SCIM handlers that
/// previously fell through the generic scanner. This prevents a future edit
/// from retaining only a route-level preflight on a mutation: the setter must
/// still pass the actor into the transaction-owned service/storage path.
#[test]
fn channel_and_scim_handlers_keep_effective_authorization_paths() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let channel_roles = fs::read_to_string(src.join("channel_roles.rs")).unwrap();
    let channels = fs::read_to_string(src.join("channels.rs")).unwrap();
    let scim = fs::read_to_string(src.join("scim.rs")).unwrap();

    for (body, data_access, label) in [
        (
            top_level_fn_body(&channel_roles, "list_roles"),
            ".members_with_roles(",
            "channel_roles::list_roles",
        ),
        (
            top_level_fn_body(&channels, "list_topic_history"),
            ".list(room,",
            "channels::list_topic_history",
        ),
    ] {
        let guard_at = body
            .find("assert_channel_access(auth.participant_id, room)")
            .unwrap_or_else(|| panic!("{label} lost participant-first channel access"));
        let access_at = body
            .find(data_access)
            .unwrap_or_else(|| panic!("{label} lost data-access marker {data_access}"));
        assert!(
            guard_at < access_at,
            "{label} must authorize before reading channel data"
        );
    }

    for (handler, authorized_call) in [
        ("set_slowmode", ".set_channel_slowmode("),
        ("set_reaction_limit", ".set_channel_reaction_limit("),
    ] {
        let body = top_level_fn_body(&channels, handler);
        let preflight_at = body
            .find("assert_channel_access(auth.participant_id, room)")
            .unwrap_or_else(|| panic!("channels::{handler} lost participant-first preflight"));
        let write_at = body
            .find(authorized_call)
            .unwrap_or_else(|| panic!("channels::{handler} lost {authorized_call}"));
        assert!(
            preflight_at < write_at,
            "channels::{handler} must preflight before the transaction-owned write"
        );
        assert!(
            body[write_at..].contains("auth.participant_id, room"),
            "channels::{handler} must pass actor then room to the authorized service"
        );
    }

    let list_tokens = top_level_fn_body(&scim, "list_tokens");
    assert!(
        list_tokens.contains(".list_tokens_authorized(ws, auth.participant_id)"),
        "scim::list_tokens must bind the requested workspace to the effective caller"
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
        assert!(
            sig.contains("Path<"),
            "synthetic handler must look like a handler"
        );
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
