//! SCIM 2.0 Group resource handlers (RFC 7643 §4.2 / RFC 7644).
//!
//! Split out of the `scim` module root; reaches the shared schema types and
//! helpers (`scim_workspace`, `scim_repo`, `scim_err`, `rfc3339`,
//! `parse_filter`, `is_unique_violation`) via `use super::*;`.

use super::*;

// ============================================================ Group handlers

/// Build a [`UserGroupRepo`] over the shared pool, mirroring [`scim_repo`].
fn user_group_repo(s: &AppState) -> UserGroupRepo {
    UserGroupRepo::new(s.participants.pool().clone())
}

/// Parse a SCIM Group `id` (a [`UserGroupId`] ULID) → typed id, else a 400.
fn parse_group_id(s: &str) -> AeroResult<UserGroupId> {
    UserGroupId::from_str(s).map_err(|e| AeroError::Invalid(format!("group id: {e}")))
}

/// Maximum mention handle length (chars, post-normalization). Mirrors the value
/// the [`crate::user_groups`] REST surface enforces.
pub(super) const MAX_GROUP_HANDLE_LEN: usize = 32;

/// Derive a `[a-z0-9_-]`, `1..=32`-char mention handle from a SCIM `displayName`.
/// A SCIM Group carries no `handle` (Aero's unique mention key), so we slugify the
/// display name: lowercase, spaces/illegal chars → `-`, collapse repeats, trim
/// dashes, truncate. Empty/degenerate input falls back to `"group"`. Pure (no
/// I/O), so it is unit-tested offline. The caller resolves collisions on the
/// `(workspace, handle)` unique index by appending a numeric suffix.
#[must_use]
pub fn slugify_handle(display_name: &str) -> String {
    let mut out = String::with_capacity(display_name.len());
    let mut prev_dash = false;
    for ch in display_name.trim().to_lowercase().chars() {
        let mapped = if ch.is_ascii_alphanumeric() || ch == '_' {
            Some(ch)
        } else if ch == '-' || ch.is_whitespace() {
            Some('-')
        } else {
            None // drop any other character (punctuation, emoji, non-ascii).
        };
        match mapped {
            Some('-') => {
                if !prev_dash && !out.is_empty() {
                    out.push('-');
                    prev_dash = true;
                }
            }
            Some(c) => {
                out.push(c);
                prev_dash = false;
            }
            None => {}
        }
    }
    // Trim a trailing dash left by truncation/trailing punctuation, and cap length.
    while out.ends_with('-') {
        out.pop();
    }
    if out.chars().count() > MAX_GROUP_HANDLE_LEN {
        out = out.chars().take(MAX_GROUP_HANDLE_LEN).collect();
        while out.ends_with('-') {
            out.pop();
        }
    }
    if out.is_empty() {
        "group".to_owned()
    } else {
        out
    }
}

/// Map a stored [`UserGroup`] + its member participant ids to the RFC 7643
/// `Group` resource. `members` carry only `value` (the participant id); SCIM
/// `display` is left `None` (best-effort — populating it would require a
/// per-member participant lookup the IdP does not need for reconciliation).
fn to_scim_group(group: &UserGroup, members: &[ParticipantId]) -> ScimGroup {
    let group_members = members
        .iter()
        .map(|p| ScimGroupMember {
            value: p.to_string(),
            display: None,
        })
        .collect();
    ScimGroup {
        schemas: vec![SCHEMA_GROUP.to_owned()],
        id: Some(group.id.to_string()),
        display_name: group.name.clone(),
        members: group_members,
        meta: Some(ScimMeta {
            resource_type: "Group".to_owned(),
            created: rfc3339(group.created_at),
            last_modified: rfc3339(group.created_at),
            location: Some(format!("/scim/v2/Groups/{}", group.id)),
        }),
    }
}

/// Load a group, asserting it belongs to the token's `workspace` (else 404 — a
/// group id from another tenant can never be read or mutated through a SCIM
/// token scoped to this workspace).
async fn group_in_scim_workspace(
    s: &AppState,
    ws: WorkspaceId,
    gid: UserGroupId,
) -> AeroResult<UserGroup> {
    user_group_repo(s)
        .get(gid)
        .await
        .map_err(AeroError::from)?
        .filter(|g| g.workspace_id == ws)
        .ok_or_else(|| AeroError::NotFound("group".into()))
}

/// Render a group as a SCIM resource, fetching its members in one place so every
/// Group handler (get/create/put/patch) returns a consistent body.
async fn render_group(s: &AppState, group: &UserGroup) -> AeroResult<ScimGroup> {
    let members = user_group_repo(s).members(group.id).await.map_err(AeroError::from)?;
    Ok(to_scim_group(group, &members))
}

/// Persist a brand-new group named `display_name` in `ws`, deriving a unique
/// mention handle from the name (numeric suffix on collision). Returns the
/// created [`UserGroup`]. `created_by` is a synthetic id: SCIM has no acting
/// human, and `user_groups.created_by` carries no FK (it is attribution only).
async fn create_group_named(
    s: &AppState,
    ws: WorkspaceId,
    display_name: &str,
) -> AeroResult<UserGroup> {
    let repo = user_group_repo(s);
    let base = slugify_handle(display_name);
    let created_by = ParticipantId::new(); // no acting user / no FK on created_by.
    // Try the base handle, then base-2, base-3, … until the unique index accepts
    // one. Bounded so a pathological collision storm can't loop forever.
    for attempt in 0..1000u32 {
        let handle = if attempt == 0 {
            base.clone()
        } else {
            // Keep within the handle length budget when appending the suffix.
            let suffix = format!("-{}", attempt + 1);
            let keep = MAX_GROUP_HANDLE_LEN.saturating_sub(suffix.len());
            let trimmed: String = base.chars().take(keep).collect();
            format!("{}{suffix}", trimmed.trim_end_matches('-'))
        };
        match repo.create(ws, &handle, display_name, created_by).await {
            Ok(id) => {
                return repo
                    .get(id)
                    .await
                    .map_err(AeroError::from)?
                    .ok_or_else(|| {
                        AeroError::Internal(anyhow::anyhow!("group vanished after create"))
                    });
            }
            Err(e) if is_unique_violation(&e) => continue, // handle taken — try next suffix.
            Err(e) => return Err(AeroError::from(e)),
        }
    }
    Err(AeroError::Conflict("could not allocate a unique group handle".into()))
}

/// `GET /scim/v2/Groups` — list every real group in the token's workspace,
/// paginated by `startIndex`/`count`, as a SCIM `ListResponse`.
pub(super) async fn list_groups(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListGroupsQuery>,
) -> Response {
    let ws = match scim_workspace(&s, &headers).await {
        Ok(w) => w,
        Err(e) => return scim_err(&e),
    };
    let repo = user_group_repo(&s);
    let mut all = match repo.list_for_workspace(ws).await {
        Ok(g) => g,
        Err(e) => return scim_err(&AeroError::from(e)),
    };
    let total = i64::try_from(all.len()).unwrap_or(i64::MAX);
    // SCIM pagination is 1-based; clamp `startIndex`/`count` to the slice bounds.
    let start_index = q.start_index.filter(|n| *n >= 1).unwrap_or(1);
    let skip = usize::try_from(start_index - 1).unwrap_or(0).min(all.len());
    let mut page: Vec<UserGroup> = all.split_off(skip);
    if let Some(count) = q.count.filter(|c| *c >= 0) {
        page.truncate(usize::try_from(count).unwrap_or(usize::MAX));
    }
    let mut resources = Vec::with_capacity(page.len());
    for g in &page {
        match render_group(&s, g).await {
            Ok(r) => resources.push(r),
            Err(e) => return scim_err(&e),
        }
    }
    Json(ScimListResponse::new(resources, total, start_index)).into_response()
}

#[derive(Deserialize)]
pub(super) struct ListGroupsQuery {
    #[serde(rename = "startIndex")]
    start_index: Option<i64>,
    count: Option<i64>,
}

/// `GET /scim/v2/Groups/:id` — fetch one group (404 if absent or in another
/// tenant), with its members.
pub(super) async fn get_group(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let ws = match scim_workspace(&s, &headers).await {
        Ok(w) => w,
        Err(e) => return scim_err(&e),
    };
    let gid = match parse_group_id(&id) {
        Ok(g) => g,
        Err(e) => return scim_err(&e),
    };
    match group_in_scim_workspace(&s, ws, gid).await {
        Ok(group) => match render_group(&s, &group).await {
            Ok(g) => Json(g).into_response(),
            Err(e) => scim_err(&e),
        },
        Err(e) => scim_err(&e),
    }
}

/// `POST /scim/v2/Groups` — create a group from `displayName` + `members`. The
/// supplied member `value`s are added to the new group. 201 with the created
/// Group; 400 if `displayName` is blank.
pub(super) async fn create_group(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ScimGroup>,
) -> Response {
    let ws = match scim_workspace(&s, &headers).await {
        Ok(w) => w,
        Err(e) => return scim_err(&e),
    };
    let display_name = payload.display_name.trim();
    if display_name.is_empty() {
        return scim_err(&AeroError::Invalid("displayName is required".into()));
    }
    let group = match create_group_named(&s, ws, display_name).await {
        Ok(g) => g,
        Err(e) => return scim_err(&e),
    };
    // Seed the initial membership from the payload's `members[].value`.
    let repo = user_group_repo(&s);
    for member in &payload.members {
        match ParticipantId::from_str(&member.value) {
            Ok(pid) => {
                if let Err(e) = repo.add_member(group.id, pid).await {
                    return scim_err(&AeroError::from(e));
                }
            }
            Err(e) => return scim_err(&AeroError::Invalid(format!("member value: {e}"))),
        }
    }
    match render_group(&s, &group).await {
        Ok(g) => (StatusCode::CREATED, Json(g)).into_response(),
        Err(e) => scim_err(&e),
    }
}

/// `PUT /scim/v2/Groups/:id` — replace a group (RFC 7644 §3.5.1): set its
/// `displayName` and make its membership exactly the supplied `members` set
/// (adding the new, removing any not listed). 200 with the updated Group, 404 if
/// absent.
pub(super) async fn put_group(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(payload): Json<ScimGroup>,
) -> Response {
    let ws = match scim_workspace(&s, &headers).await {
        Ok(w) => w,
        Err(e) => return scim_err(&e),
    };
    let gid = match parse_group_id(&id) {
        Ok(g) => g,
        Err(e) => return scim_err(&e),
    };
    let group = match group_in_scim_workspace(&s, ws, gid).await {
        Ok(g) => g,
        Err(e) => return scim_err(&e),
    };
    let display_name = payload.display_name.trim();
    if display_name.is_empty() {
        return scim_err(&AeroError::Invalid("displayName is required".into()));
    }
    let repo = user_group_repo(&s);

    // Rename: PUT replaces the resource, so reflect displayName onto the group's
    // `name` (the handle is immutable — it is Aero's stable mention key).
    if display_name != group.name {
        if let Err(e) = repo.rename(gid, ws, display_name).await {
            return scim_err(&AeroError::from(e));
        }
    }

    // Reconcile membership to exactly the supplied set.
    let desired = match parse_member_values(&payload.members) {
        Ok(d) => d,
        Err(e) => return scim_err(&e),
    };
    let current = match repo.members(gid).await {
        Ok(m) => m,
        Err(e) => return scim_err(&AeroError::from(e)),
    };
    for pid in &desired {
        if !current.contains(pid) {
            if let Err(e) = repo.add_member(gid, *pid).await {
                return scim_err(&AeroError::from(e));
            }
        }
    }
    for pid in &current {
        if !desired.contains(pid) {
            if let Err(e) = repo.remove_member(gid, *pid).await {
                return scim_err(&AeroError::from(e));
            }
        }
    }
    finish_group(&s, ws, gid).await
}

/// `DELETE /scim/v2/Groups/:id` — delete the group (its memberships cascade).
/// 204 on success, 404 if absent / another tenant.
pub(super) async fn delete_group(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let ws = match scim_workspace(&s, &headers).await {
        Ok(w) => w,
        Err(e) => return scim_err(&e),
    };
    let gid = match parse_group_id(&id) {
        Ok(g) => g,
        Err(e) => return scim_err(&e),
    };
    let repo = user_group_repo(&s);
    match repo.delete(gid, ws).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => scim_err(&AeroError::NotFound("group".into())),
        Err(e) => scim_err(&AeroError::from(e)),
    }
}

/// A SCIM Group PATCH op (RFC 7644 §3.5.2). The op's `value` for a `members`
/// change is an array of member objects (`[{"value": "<id>"}]`); for a
/// `displayName` replace it is a bare string.
#[derive(Deserialize)]
pub(super) struct GroupPatchOp {
    #[serde(default)]
    op: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    value: serde_json::Value,
}

#[derive(Deserialize)]
pub(super) struct GroupPatch {
    #[serde(rename = "Operations", default)]
    pub(super) operations: Vec<GroupPatchOp>,
}

/// One parsed, mappable Group PATCH action, distilled from the raw op grammar.
/// Pure value type so the parser ([`parse_group_patch`]) is unit-tested offline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupPatchAction {
    /// Replace the group's `displayName`.
    SetDisplayName(String),
    /// Add a member by participant id.
    AddMember(String),
    /// Remove a member by participant id.
    RemoveMember(String),
}

/// Extract member `value`s from a PATCH op's `value` — either an array of member
/// objects (`[{"value": "<id>"}]`, what Okta/Azure send) or a single member
/// object / bare string. Pure helper used by the parser.
pub(super) fn member_values_from(value: &serde_json::Value) -> Vec<String> {
    let one = |v: &serde_json::Value| -> Option<String> {
        if let Some(s) = v.as_str() {
            Some(s.to_owned())
        } else {
            v.get("value")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        }
    };
    match value {
        serde_json::Value::Array(items) => items.iter().filter_map(one).collect(),
        other => one(other).into_iter().collect(),
    }
}

/// Parse a SCIM Group PATCH body into the ordered list of actions we apply.
/// Supports the member mutations every real IdP sends — `add`/`remove` on the
/// `members` path (RFC 7644 §3.5.2.1/§3.5.2.3), including a targeted
/// `members[value eq "<id>"]` remove — plus a `replace` of `displayName`.
/// Unknown ops/paths are skipped (a documented seam). Pure (no I/O).
#[must_use]
#[allow(private_interfaces)]
pub fn parse_group_patch_ops(ops: &[GroupPatchOp]) -> Vec<GroupPatchAction> {
    let mut actions = Vec::new();
    for op in ops {
        let verb = op.op.to_ascii_lowercase();
        let path = op.path.as_deref().unwrap_or("").trim();
        let path_lower = path.to_ascii_lowercase();
        match verb.as_str() {
            "add" if path_lower == "members" || path.is_empty() => {
                for v in member_values_from(&op.value) {
                    actions.push(GroupPatchAction::AddMember(v));
                }
            }
            "remove" if path_lower == "members" => {
                // `remove` with no value removes all — but IdPs target one member.
                for v in member_values_from(&op.value) {
                    actions.push(GroupPatchAction::RemoveMember(v));
                }
            }
            "remove" if path_lower.starts_with("members[") => {
                // Targeted remove: `members[value eq "<id>"]`.
                if let Some(v) = member_filter_value(path) {
                    actions.push(GroupPatchAction::RemoveMember(v));
                }
            }
            "replace" | "add"
                if path_lower == "displayname"
                    || (path.is_empty()
                        && op.value.get("displayName").is_some()) =>
            {
                let name = if path_lower == "displayname" {
                    op.value.as_str().map(str::to_owned)
                } else {
                    op.value
                        .get("displayName")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                };
                if let Some(n) = name {
                    actions.push(GroupPatchAction::SetDisplayName(n));
                }
                // A path-less replace may also carry members.
                if path.is_empty() {
                    if let Some(m) = op.value.get("members") {
                        for v in member_values_from(m) {
                            actions.push(GroupPatchAction::AddMember(v));
                        }
                    }
                }
            }
            _ => {} // unknown op/path — skip (documented seam).
        }
    }
    actions
}

/// Pull the quoted id out of a `members[value eq "<id>"]` path filter. Pure.
pub(super) fn member_filter_value(path: &str) -> Option<String> {
    let inner = path.split_once('[')?.1;
    let inner = inner.rsplit_once(']')?.0;
    // Reuse the User filter parser: `value eq "<id>"`.
    parse_filter(inner)
        .filter(|(attr, _)| attr.eq_ignore_ascii_case("value"))
        .map(|(_, v)| v)
}

/// `PATCH /scim/v2/Groups/:id` — apply member `add`/`remove` and `displayName`
/// replace operations (RFC 7644 §3.5.2). 200 with the updated Group, 404 if
/// absent / another tenant.
pub(super) async fn patch_group(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(patch): Json<GroupPatch>,
) -> Response {
    let ws = match scim_workspace(&s, &headers).await {
        Ok(w) => w,
        Err(e) => return scim_err(&e),
    };
    let gid = match parse_group_id(&id) {
        Ok(g) => g,
        Err(e) => return scim_err(&e),
    };
    // Resolve first so an unknown id (or another tenant's) is a clean 404.
    if let Err(e) = group_in_scim_workspace(&s, ws, gid).await {
        return scim_err(&e);
    }
    let repo = user_group_repo(&s);
    for action in parse_group_patch_ops(&patch.operations) {
        match action {
            GroupPatchAction::SetDisplayName(name) => {
                let name = name.trim();
                if name.is_empty() {
                    return scim_err(&AeroError::Invalid("displayName must not be empty".into()));
                }
                if let Err(e) = repo.rename(gid, ws, name).await {
                    return scim_err(&AeroError::from(e));
                }
            }
            GroupPatchAction::AddMember(value) => match ParticipantId::from_str(&value) {
                Ok(pid) => {
                    if let Err(e) = repo.add_member(gid, pid).await {
                        return scim_err(&AeroError::from(e));
                    }
                }
                Err(e) => return scim_err(&AeroError::Invalid(format!("member value: {e}"))),
            },
            GroupPatchAction::RemoveMember(value) => match ParticipantId::from_str(&value) {
                Ok(pid) => {
                    if let Err(e) = repo.remove_member(gid, pid).await {
                        return scim_err(&AeroError::from(e));
                    }
                }
                Err(e) => return scim_err(&AeroError::Invalid(format!("member value: {e}"))),
            },
        }
    }
    finish_group(&s, ws, gid).await
}

/// Re-read a group after a write and render it as the SCIM response body (404 if
/// it vanished — e.g. deleted concurrently).
async fn finish_group(s: &AppState, ws: WorkspaceId, gid: UserGroupId) -> Response {
    match group_in_scim_workspace(s, ws, gid).await {
        Ok(group) => match render_group(s, &group).await {
            Ok(g) => Json(g).into_response(),
            Err(e) => scim_err(&e),
        },
        Err(e) => scim_err(&e),
    }
}

/// Parse the `members[].value`s of a SCIM Group body into typed participant ids,
/// failing the whole op on the first malformed id (a 400). Pure helper.
fn parse_member_values(members: &[ScimGroupMember]) -> AeroResult<Vec<ParticipantId>> {
    members
        .iter()
        .map(|m| {
            ParticipantId::from_str(&m.value)
                .map_err(|e| AeroError::Invalid(format!("member value: {e}")))
        })
        .collect()
}
