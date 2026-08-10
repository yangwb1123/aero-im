//! SCIM 2.0 Group resource handlers (RFC 7643 §4.2 / RFC 7644).
//!
//! Split out of the `scim` module root; reaches the shared schema types and
//! helpers (`scim_workspace`, `scim_repo`, `scim_err`, `rfc3339`,
//! `parse_filter`, `is_unique_violation`) via `use super::*;`.

use super::{
    parse_filter, rfc3339, scim_err, scim_workspace, validate_patch_operation_count, AeroError,
    AeroResult, AppState, Deserialize, FromStr, HeaderMap, IntoResponse, Json, ParticipantId, Path,
    Query, Response, ScimGroup, ScimGroupMember, ScimListResponse, ScimMeta, State, StatusCode,
    UserGroup, UserGroupId, UserGroupRepo, WorkspaceId, SCHEMA_GROUP,
};
use aero_storage::user_group::{ScimGroupMutation, ScimGroupWrite, UserGroupWriteError};

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
/// Match the first-party user-group API's display-name budget.
const MAX_GROUP_NAME_BYTES: usize = 64;
/// Bound SCIM bulk expansion and the single-transaction member lock/insert set.
const MAX_SCIM_GROUP_MEMBERS: usize = 1_000;
const DEFAULT_SCIM_GROUP_PAGE_SIZE: i64 = 100;
const MAX_SCIM_GROUP_PAGE_SIZE: i64 = 200;

fn validate_display_name(raw: &str) -> AeroResult<&str> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(AeroError::Invalid("displayName is required".into()));
    }
    if name.len() > MAX_GROUP_NAME_BYTES {
        return Err(AeroError::Invalid(format!(
            "displayName must be at most {MAX_GROUP_NAME_BYTES} bytes"
        )));
    }
    Ok(name)
}

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
/// per-member participant lookup the `IdP` does not need for reconciliation).
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
    let members = user_group_repo(s)
        .members(group.id)
        .await
        .map_err(AeroError::from)?;
    Ok(to_scim_group(group, &members))
}

fn map_group_write_error(error: UserGroupWriteError) -> AeroError {
    match error {
        UserGroupWriteError::NotFound => AeroError::NotFound("group".into()),
        UserGroupWriteError::MemberNotInWorkspace(participant) => AeroError::Invalid(format!(
            "member {participant} is not a member of this workspace"
        )),
        UserGroupWriteError::NotAuthorized => {
            AeroError::Forbidden("not authorized to manage this group".into())
        }
        UserGroupWriteError::WorkspaceHasNoCreator => {
            AeroError::Conflict("workspace has no eligible group creator".into())
        }
        UserGroupWriteError::HandleExhausted => {
            AeroError::Conflict("could not allocate a unique group handle".into())
        }
        UserGroupWriteError::Storage(error) => AeroError::from(error),
    }
}

fn committed_scim_group(write: &ScimGroupWrite) -> ScimGroup {
    to_scim_group(&write.group, &write.members)
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
    let (start_index, count, offset) = group_pagination(q.start_index, q.count);
    let (total, page) = match user_group_repo(&s)
        .list_scim_groups_page(ws, count, offset)
        .await
    {
        Ok(page) => page,
        Err(e) => return scim_err(&AeroError::from(e)),
    };
    let resources = page.into_iter().map(|w| committed_scim_group(&w)).collect();
    Json(ScimListResponse::new(resources, total, start_index)).into_response()
}

#[derive(Deserialize)]
pub(super) struct ListGroupsQuery {
    #[serde(rename = "startIndex")]
    start_index: Option<i64>,
    count: Option<i64>,
}

fn group_pagination(start_index: Option<i64>, count: Option<i64>) -> (i64, i64, i64) {
    let start_index = start_index.filter(|value| *value >= 1).unwrap_or(1);
    let count = count
        .filter(|value| *value >= 0)
        .unwrap_or(DEFAULT_SCIM_GROUP_PAGE_SIZE)
        .min(MAX_SCIM_GROUP_PAGE_SIZE);
    (start_index, count, start_index.saturating_sub(1))
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
    let display_name = match validate_display_name(&payload.display_name) {
        Ok(name) => name,
        Err(error) => return scim_err(&error),
    };
    // Parse the complete body before the first write. A malformed member later in
    // the array must not leave an orphan group or an applied membership prefix.
    let members = match parse_member_values(&payload.members) {
        Ok(members) => members,
        Err(error) => return scim_err(&error),
    };
    let group = match user_group_repo(&s)
        .create_scim_group(ws, &slugify_handle(display_name), display_name, &members)
        .await
    {
        Ok(g) => g,
        Err(error) => return scim_err(&map_group_write_error(error)),
    };
    (StatusCode::CREATED, Json(committed_scim_group(&group))).into_response()
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
    let display_name = match validate_display_name(&payload.display_name) {
        Ok(name) => name,
        Err(error) => return scim_err(&error),
    };
    // Parse the complete desired set before the storage transaction. Membership
    // validity and group tenancy are then rechecked under row locks in storage.
    let desired = match parse_member_values(&payload.members) {
        Ok(d) => d,
        Err(e) => return scim_err(&e),
    };
    let group = match user_group_repo(&s)
        .replace_scim_group(ws, gid, display_name, &desired)
        .await
    {
        Ok(group) => group,
        Err(error) => return scim_err(&map_group_write_error(error)),
    };
    Json(committed_scim_group(&group)).into_response()
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
    match repo.delete_scim_group(ws, gid).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => scim_err(&AeroError::NotFound("group".into())),
        Err(e) => scim_err(&map_group_write_error(e)),
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
/// Supports the member mutations every real `IdP` sends — `add`/`remove` on the
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
                    || (path.is_empty() && op.value.get("displayName").is_some()) =>
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
    if let Err(error) = validate_patch_operation_count(patch.operations.len()) {
        return scim_err(&error);
    }
    let gid = match parse_group_id(&id) {
        Ok(g) => g,
        Err(e) => return scim_err(&e),
    };
    // Convert every raw operation before touching storage. This makes a malformed
    // final operation reject the whole PATCH without committing an earlier rename
    // or member mutation.
    let mutations = match parse_scim_group_mutations(&patch.operations) {
        Ok(mutations) => mutations,
        Err(error) => return scim_err(&error),
    };
    let group = match user_group_repo(&s)
        .patch_scim_group(ws, gid, &mutations)
        .await
    {
        Ok(group) => group,
        Err(error) => return scim_err(&map_group_write_error(error)),
    };
    Json(committed_scim_group(&group)).into_response()
}

/// Parse the `members[].value`s of a SCIM Group body into typed participant ids,
/// failing the whole op on the first malformed id (a 400). Pure helper.
fn parse_member_values(members: &[ScimGroupMember]) -> AeroResult<Vec<ParticipantId>> {
    if members.len() > MAX_SCIM_GROUP_MEMBERS {
        return Err(AeroError::Invalid(format!(
            "members must contain at most {MAX_SCIM_GROUP_MEMBERS} entries"
        )));
    }
    members
        .iter()
        .map(|m| {
            ParticipantId::from_str(&m.value)
                .map_err(|e| AeroError::Invalid(format!("member value: {e}")))
        })
        .collect()
}

fn parse_scim_group_mutations(ops: &[GroupPatchOp]) -> AeroResult<Vec<ScimGroupMutation>> {
    let mut mutations = Vec::new();
    let mut member_actions = 0;
    for op in ops {
        let verb = op.op.to_ascii_lowercase();
        let path = op.path.as_deref().unwrap_or("").trim();
        let path_lower = path.to_ascii_lowercase();
        match verb.as_str() {
            "add" if path_lower == "members" => append_member_mutations(
                &op.value,
                ScimGroupMutation::AddMember,
                &mut member_actions,
                &mut mutations,
            )?,
            "remove" if path_lower == "members" => append_member_mutations(
                &op.value,
                ScimGroupMutation::RemoveMember,
                &mut member_actions,
                &mut mutations,
            )?,
            "remove" if path_lower.starts_with("members[") => {
                let value = member_filter_value(path).ok_or_else(|| {
                    AeroError::Invalid("invalid members remove filter".to_owned())
                })?;
                append_member_value(
                    &value,
                    ScimGroupMutation::RemoveMember,
                    &mut member_actions,
                    &mut mutations,
                )?;
            }
            "replace" | "add" if path_lower == "displayname" => {
                let name = op
                    .value
                    .as_str()
                    .ok_or_else(|| AeroError::Invalid("displayName must be a string".to_owned()))?;
                mutations.push(ScimGroupMutation::SetName(
                    validate_display_name(name)?.to_owned(),
                ));
            }
            "replace" | "add" if path.is_empty() => {
                if let Some(object) = op.value.as_object() {
                    if let Some(name) = object.get("displayName") {
                        let name = name.as_str().ok_or_else(|| {
                            AeroError::Invalid("displayName must be a string".to_owned())
                        })?;
                        mutations.push(ScimGroupMutation::SetName(
                            validate_display_name(name)?.to_owned(),
                        ));
                    }
                    if let Some(members) = object.get("members") {
                        append_member_mutations(
                            members,
                            ScimGroupMutation::AddMember,
                            &mut member_actions,
                            &mut mutations,
                        )?;
                    } else if object.contains_key("value") {
                        append_member_mutations(
                            &op.value,
                            ScimGroupMutation::AddMember,
                            &mut member_actions,
                            &mut mutations,
                        )?;
                    }
                } else if verb == "add" {
                    append_member_mutations(
                        &op.value,
                        ScimGroupMutation::AddMember,
                        &mut member_actions,
                        &mut mutations,
                    )?;
                }
            }
            _ => {} // Preserve the existing compatibility seam for unknown operations/paths.
        }
    }
    Ok(mutations)
}

fn append_member_mutations(
    value: &serde_json::Value,
    mutation: fn(ParticipantId) -> ScimGroupMutation,
    member_actions: &mut usize,
    mutations: &mut Vec<ScimGroupMutation>,
) -> AeroResult<()> {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                let value = member_value(item)?;
                append_member_value(value, mutation, member_actions, mutations)?;
            }
        }
        value => {
            let value = member_value(value)?;
            append_member_value(value, mutation, member_actions, mutations)?;
        }
    }
    Ok(())
}

fn member_value(value: &serde_json::Value) -> AeroResult<&str> {
    value
        .as_str()
        .or_else(|| value.get("value").and_then(serde_json::Value::as_str))
        .ok_or_else(|| AeroError::Invalid("member entry must contain a string value".to_owned()))
}

fn append_member_value(
    value: &str,
    mutation: fn(ParticipantId) -> ScimGroupMutation,
    member_actions: &mut usize,
    mutations: &mut Vec<ScimGroupMutation>,
) -> AeroResult<()> {
    *member_actions += 1;
    if *member_actions > MAX_SCIM_GROUP_MEMBERS {
        return Err(AeroError::Invalid(format!(
            "PATCH may reference at most {MAX_SCIM_GROUP_MEMBERS} members"
        )));
    }
    let participant = ParticipantId::from_str(value)
        .map_err(|error| AeroError::Invalid(format!("member value: {error}")))?;
    mutations.push(mutation(participant));
    Ok(())
}

#[cfg(test)]
mod atomic_input_tests {
    use super::*;

    #[test]
    fn display_name_and_member_limits_fail_before_storage() {
        assert!(validate_display_name(&"x".repeat(MAX_GROUP_NAME_BYTES)).is_ok());
        assert!(validate_display_name(&"x".repeat(MAX_GROUP_NAME_BYTES + 1)).is_err());

        let members = (0..=MAX_SCIM_GROUP_MEMBERS)
            .map(|_| ScimGroupMember {
                value: ParticipantId::new().to_string(),
                display: None,
            })
            .collect::<Vec<_>>();
        assert!(parse_member_values(&members).is_err());
    }

    #[test]
    fn typed_patch_parser_rejects_a_late_invalid_operation() {
        let ops = vec![
            GroupPatchOp {
                op: "replace".into(),
                path: Some("displayName".into()),
                value: serde_json::json!("Changed"),
            },
            GroupPatchOp {
                op: "add".into(),
                path: Some("members".into()),
                value: serde_json::json!([{"value": "not-a-participant-id"}]),
            },
        ];
        assert!(parse_scim_group_mutations(&ops).is_err());
    }

    #[test]
    fn typed_patch_parser_caps_member_actions() {
        let members = (0..=MAX_SCIM_GROUP_MEMBERS)
            .map(|_| serde_json::json!({"value": ParticipantId::new().to_string()}))
            .collect::<Vec<_>>();
        let ops = vec![GroupPatchOp {
            op: "add".into(),
            path: Some("members".into()),
            value: serde_json::Value::Array(members),
        }];

        assert!(parse_scim_group_mutations(&ops).is_err());
    }

    #[test]
    fn typed_patch_parser_rejects_malformed_supported_member_entries() {
        let ops = vec![GroupPatchOp {
            op: "add".into(),
            path: Some("members".into()),
            value: serde_json::json!([
                {"value": ParticipantId::new().to_string()},
                {"display": "missing value"}
            ]),
        }];

        assert!(parse_scim_group_mutations(&ops).is_err());
    }

    #[test]
    fn group_pagination_is_one_based_and_bounded() {
        assert_eq!(group_pagination(None, None), (1, 100, 0));
        assert_eq!(group_pagination(Some(0), Some(-1)), (1, 100, 0));
        assert_eq!(group_pagination(Some(5), Some(10_000)), (5, 200, 4));
        assert_eq!(
            group_pagination(Some(i64::MAX), Some(0)),
            (i64::MAX, 0, i64::MAX - 1)
        );
    }
}
