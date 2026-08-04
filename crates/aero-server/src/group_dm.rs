//! Group direct-message (multi-person) find-or-create HTTP surface.
//!
//! A group DM is a small fixed-member room with a persistent identity independent
//! of its mutable display name. Opening one is idempotent find-or-create over that
//! *exact* set: the first request among a given group of people creates a room;
//! every later request for the same set resolves to it, even after it is named.
//! [`GroupDmRepo`](aero_storage::GroupDmRepo) owns the atomic operation: an
//! advisory lock over the workspace + sorted member set serializes concurrent
//! opens, followed by one transaction that rechecks or inserts the room and every
//! membership edge.
//!
//! The member set is the caller ∪ the requested ids, de-duplicated, and must hold
//! 3–8 people: a 2-person set is a 1:1 DM (steered to `POST /api/dm`), and the
//! upper bound keeps a "group DM" small (larger groups are channels). Group DMs are
//! not a per-tenant concept here, so they are created in the legacy all-zero
//! default workspace (mirroring [`crate::dm`]). Every supplied member must have
//! effective access to that workspace; listing applies the same effective-access
//! boundary. Mounted via [`routes`] and `.merge`d into the main router.

use std::{collections::HashSet, fmt, str::FromStr};

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, Room, RoomEvent, RoomId, WorkspaceId};
use aero_storage::{
    GroupDmRepo, GroupDmWriteError, MAX_CHANNEL_DESCRIPTION_CHARS, MAX_GROUP_DM_NAME_CHARS,
};
use axum::{
    extract::{Path, State},
    routing::post,
    Json, Router,
};
use serde::{de, Deserialize};

use crate::error::ApiResult;
use crate::routes::helpers::{
    assert_effective_workspace_member, assert_effective_workspace_members,
};
use crate::state::AppState;

/// All group-DM routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/group-dm", post(open_group_dm).get(list_group_dms))
        // Group DM naming (ROADMAP6 Lane A): PATCH name/description of a group room.
        .route(
            "/api/rooms/:id/name",
            axum::routing::patch(set_group_dm_name),
        )
}

/// The legacy / default workspace (the all-zero `WorkspaceId`) that group DMs are
/// created in, mirroring [`crate::dm`]'s `DEFAULT_WORKSPACE_ID`. Group DMs are not a
/// per-tenant concept here, so they land in the default workspace alongside other
/// untenanted rooms.
const DEFAULT_WORKSPACE_ID: WorkspaceId = WorkspaceId(ulid::Ulid(0));

/// Smallest group-DM size — fewer than this is a 1:1 DM (or nonsensical), so the
/// caller is steered to `POST /api/dm` instead.
const MIN_MEMBERS: usize = 3;
/// Largest group-DM size — beyond this a conversation should be a channel.
const MAX_MEMBERS: usize = 8;
/// The caller is implicit, so the request may name at most seven other people.
const MAX_REQUESTED_PARTICIPANTS: usize = MAX_MEMBERS - 1;
fn parse_participant(s: &str) -> Result<ParticipantId, AeroError> {
    ParticipantId::from_str(s.trim())
        .map_err(|e| AeroError::Invalid(format!("participant id: {e}")))
}

#[derive(Debug, Deserialize)]
struct CreateGroupDmReq {
    /// The other participants to include (the caller is added automatically). Parsed
    /// then unioned with the caller and de-duplicated to form the member set.
    #[serde(deserialize_with = "deserialize_participant_ids")]
    participant_ids: Vec<String>,
}

struct BoundedParticipantIds;

impl<'de> de::Visitor<'de> for BoundedParticipantIds {
    type Value = Vec<String>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "an array of at most {MAX_REQUESTED_PARTICIPANTS} participant ids"
        )
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: de::SeqAccess<'de>,
    {
        let mut ids = Vec::with_capacity(MAX_REQUESTED_PARTICIPANTS);
        while let Some(id) = seq.next_element::<String>()? {
            if ids.len() == MAX_REQUESTED_PARTICIPANTS {
                return Err(de::Error::invalid_length(ids.len() + 1, &self));
            }
            ids.push(id);
        }
        Ok(ids)
    }
}

fn deserialize_participant_ids<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserializer.deserialize_seq(BoundedParticipantIds)
}

fn parse_group_dm_members(
    caller: ParticipantId,
    requested: &[String],
) -> Result<Vec<ParticipantId>, AeroError> {
    // Keep this defensive check even though the custom deserializer rejects the
    // eighth element without materializing an unbounded Vec. It also protects
    // non-HTTP callers and pins validation order before UUID parsing/DB work.
    if requested.len() > MAX_REQUESTED_PARTICIPANTS {
        return Err(AeroError::Invalid(format!(
            "a group DM request may include at most {MAX_REQUESTED_PARTICIPANTS} participant ids"
        )));
    }

    let mut members = Vec::with_capacity(requested.len() + 1);
    let mut seen = HashSet::with_capacity(requested.len() + 1);
    members.push(caller);
    seen.insert(caller);
    for raw in requested {
        let participant = parse_participant(raw)?;
        if seen.insert(participant) {
            members.push(participant);
        }
    }

    let count = members.len();
    if count == 2 {
        return Err(AeroError::Invalid(
            "a 2-person conversation is a 1:1 — use POST /api/dm".into(),
        ));
    }
    if count < MIN_MEMBERS {
        return Err(AeroError::Invalid(format!(
            "a group DM needs at least {MIN_MEMBERS} people"
        )));
    }
    if count > MAX_MEMBERS {
        return Err(AeroError::Invalid(format!(
            "a group DM may have at most {MAX_MEMBERS} people"
        )));
    }
    Ok(members)
}

/// `POST /api/group-dm` — open the group DM among the caller and the
/// requested participants, creating it on first use. The member set is the caller ∪
/// the requested ids, de-duplicated, and must hold 3–8 people: a 2-person set is
/// rejected `400` ("use /api/dm"), as is a set smaller than 3 or larger than 8.
/// Idempotent: returns the existing group DM when one already exists for the exact
/// set, otherwise creates a fresh nameless `group` room (caller enrolled as owner,
/// every other member added). Returns the room.
async fn open_group_dm(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateGroupDmReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let members = parse_group_dm_members(auth.participant_id, &req.participant_ids)?;

    // Group DMs are default-workspace resources. Gate the caller before barrier
    // lookup, find-existing, or creation so retained membership rows cannot be
    // used after account deletion, workspace deactivation, or a 2FA mandate.
    assert_effective_workspace_members(&s, DEFAULT_WORKSPACE_ID, &members).await?;

    // Information barrier (ethical wall) enforcement: no two members may sit across
    // a barred pair of user-groups. Checked over every unordered pair in the member
    // set; built inline from the shared pool so the check is ALWAYS available. Group
    // DMs live in the default workspace, so the barrier is evaluated there.
    let barriers = aero_storage::BarrierRepo::new(s.pg.clone());
    for (i, x) in members.iter().enumerate() {
        for y in &members[i + 1..] {
            if barriers
                .barred(DEFAULT_WORKSPACE_ID, *x, *y)
                .await
                .map_err(AeroError::from)?
            {
                return Err(AeroError::Forbidden("information barrier".into()).into());
            }
        }
    }

    let repo = GroupDmRepo::new(s.pg.clone());
    let room = repo
        .find_or_create_in_workspace(DEFAULT_WORKSPACE_ID, &members, auth.participant_id)
        .await
        .map_err(map_group_dm_open_error)?;
    Ok(Json(serde_json::to_value(room).map_err(AeroError::from)?))
}

/// `GET /api/group-dm` — the caller's marker-identified group DMs, newest first.
async fn list_group_dms(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    assert_effective_workspace_member(&s, DEFAULT_WORKSPACE_ID, auth.participant_id).await?;
    let rooms: Vec<Room> = GroupDmRepo::new(s.pg.clone())
        .list_for_participant_in_workspace(auth.participant_id, DEFAULT_WORKSPACE_ID)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "rooms": rooms })))
}

// ---------- Group DM naming (ROADMAP6 Lane A) ----------

#[derive(Deserialize)]
struct SetGroupDmNameReq {
    /// New display name for the group DM. Pass `null` to clear it (revert to
    /// nameless). Optional — absent means "don't change".
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    #[allow(clippy::option_option)]
    name: Option<Option<String>>,
    /// New description for the group DM. Pass `null` to clear. Optional.
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    #[allow(clippy::option_option)]
    description: Option<Option<String>>,
}

#[allow(clippy::option_option)]
fn deserialize_optional_field<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Debug, PartialEq, Eq)]
struct GroupDmMetadataPatch {
    #[allow(clippy::option_option)]
    name: Option<Option<String>>,
    #[allow(clippy::option_option)]
    description: Option<Option<String>>,
}

fn normalize_metadata_patch(request: SetGroupDmNameReq) -> Result<GroupDmMetadataPatch, AeroError> {
    if request.name.is_none() && request.description.is_none() {
        return Err(AeroError::Invalid(
            "at least one of name or description is required".into(),
        ));
    }

    // Whitespace-only values clear the field, matching channel metadata PATCH.
    let name = request.name.map(|value| {
        value
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty())
    });
    let description = request.description.map(|value| {
        value
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty())
    });
    if name
        .as_ref()
        .and_then(Option::as_ref)
        .is_some_and(|value| value.chars().count() > MAX_GROUP_DM_NAME_CHARS)
    {
        return Err(AeroError::Invalid(format!(
            "group DM name is too long (max {MAX_GROUP_DM_NAME_CHARS} chars)"
        )));
    }
    if description
        .as_ref()
        .and_then(Option::as_ref)
        .is_some_and(|value| value.chars().count() > MAX_CHANNEL_DESCRIPTION_CHARS)
    {
        return Err(AeroError::Invalid(format!(
            "group DM description is too long (max {MAX_CHANNEL_DESCRIPTION_CHARS} chars)"
        )));
    }
    Ok(GroupDmMetadataPatch { name, description })
}

fn parse_room_for_name(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

fn map_group_dm_write_error(error: GroupDmWriteError, room: RoomId) -> AeroError {
    match error {
        GroupDmWriteError::NotFound => AeroError::NotFound(format!("group DM {room}")),
        GroupDmWriteError::NotGroupDm => {
            AeroError::Invalid("only marker-identified group DMs support PATCH /name".into())
        }
        GroupDmWriteError::Forbidden => AeroError::Forbidden("group DM access was revoked".into()),
        GroupDmWriteError::InformationBarrier => AeroError::Forbidden("information barrier".into()),
        GroupDmWriteError::InvalidInput(message) => AeroError::Invalid(message),
        GroupDmWriteError::Storage(source) => AeroError::from(source),
    }
}

fn map_group_dm_open_error(error: GroupDmWriteError) -> AeroError {
    match error {
        GroupDmWriteError::Forbidden => {
            AeroError::Forbidden("group DM member access was revoked".into())
        }
        GroupDmWriteError::InformationBarrier => AeroError::Forbidden("information barrier".into()),
        GroupDmWriteError::Storage(source) => AeroError::from(source),
        GroupDmWriteError::NotFound => AeroError::NotFound("group DM workspace".into()),
        GroupDmWriteError::NotGroupDm => {
            AeroError::Invalid("legacy group-DM candidate changed identity".into())
        }
        GroupDmWriteError::InvalidInput(message) => AeroError::Invalid(message),
    }
}

/// `PATCH /api/rooms/:id/name` — update the display name and/or description of a
/// marker-identified group DM. The caller must have canonical room access. Both
/// fields are optional; absent fields are left unchanged. Passing `null` or only
/// whitespace clears the field.
///
/// After a successful update, a `Membership { Join }` event is broadcast so
/// connected clients refresh the room metadata (no dedicated `RoomUpdated` event
/// exists yet; membership events already trigger room-list refreshes in clients).
async fn set_group_dm_name(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<SetGroupDmNameReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_for_name(&id_str)?;
    let patch = normalize_metadata_patch(req)?;

    // Canonical room boundary FIRST: unlike a bare room-membership lookup this
    // rejects deleted accounts, workspace deactivation, and unmet mandatory 2FA.
    s.im.assert_room_access(auth.participant_id, room).await?;

    let name = patch.name.as_ref().map(|value| value.as_deref());
    let description = patch.description.as_ref().map(|value| value.as_deref());
    GroupDmRepo::new(s.pg.clone())
        .patch_metadata(room, auth.participant_id, name, description)
        .await
        .map_err(|error| map_group_dm_write_error(error, room))?;

    // Broadcast a metadata-change hint (Membership Join reuses the existing event path;
    // no dedicated RoomUpdated event exists yet). Best-effort.
    s.im.broadcast_room_event(
        room,
        RoomEvent::Membership {
            room_id: room,
            participant: auth.participant_id,
            op: aero_common::MembershipOp::Join,
        },
    )
    .await;

    Ok(Json(
        serde_json::json!({ "room_id": room, "updated": true }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn participant(seed: u128) -> ParticipantId {
        ParticipantId::from_uuid(uuid::Uuid::from_u128(seed))
    }

    #[test]
    fn participant_array_is_bounded_during_deserialization() {
        let accepted = serde_json::from_value::<CreateGroupDmReq>(serde_json::json!({
            "participant_ids": vec!["participant"; MAX_REQUESTED_PARTICIPANTS]
        }))
        .unwrap();
        assert_eq!(accepted.participant_ids.len(), MAX_REQUESTED_PARTICIPANTS);

        let requested = (0..=MAX_REQUESTED_PARTICIPANTS)
            .map(|index| format!("participant-{index}"))
            .collect::<Vec<_>>();
        let error = serde_json::from_value::<CreateGroupDmReq>(
            serde_json::json!({ "participant_ids": requested }),
        )
        .unwrap_err();
        assert!(error.to_string().contains("at most 7 participant ids"));
    }

    #[test]
    fn raw_participant_cap_precedes_uuid_parsing() {
        let requested = vec!["not-a-uuid".to_owned(); MAX_REQUESTED_PARTICIPANTS + 1];
        let error = parse_group_dm_members(participant(1), &requested).unwrap_err();
        assert!(
            error.to_string().contains("at most 7 participant ids"),
            "the raw cardinality guard must run before parsing: {error}"
        );
    }

    #[test]
    fn member_set_is_deduplicated_and_bounded() {
        let caller = participant(1);
        let second = participant(2).to_string();
        let third = participant(3).to_string();
        let members = parse_group_dm_members(caller, &[second.clone(), second, third]).unwrap();
        assert_eq!(members, vec![caller, participant(2), participant(3)]);
    }

    #[test]
    fn metadata_patch_distinguishes_absent_null_and_value() {
        let request: SetGroupDmNameReq = serde_json::from_value(serde_json::json!({
            "name": "  Product trio  ",
            "description": null
        }))
        .unwrap();
        let patch = normalize_metadata_patch(request).unwrap();
        assert_eq!(
            patch,
            GroupDmMetadataPatch {
                name: Some(Some("Product trio".to_owned())),
                description: Some(None),
            }
        );

        let missing: SetGroupDmNameReq = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(normalize_metadata_patch(missing).is_err());
    }

    #[test]
    fn metadata_patch_enforces_unicode_character_bounds() {
        let name: SetGroupDmNameReq = serde_json::from_value(serde_json::json!({
            "name": "界".repeat(MAX_GROUP_DM_NAME_CHARS + 1)
        }))
        .unwrap();
        assert!(normalize_metadata_patch(name).is_err());

        let description: SetGroupDmNameReq = serde_json::from_value(serde_json::json!({
            "description": "界".repeat(MAX_CHANNEL_DESCRIPTION_CHARS + 1)
        }))
        .unwrap();
        assert!(normalize_metadata_patch(description).is_err());
    }

    #[test]
    fn write_errors_map_to_stable_client_error_classes() {
        let room = RoomId::new();
        assert!(matches!(
            map_group_dm_write_error(GroupDmWriteError::NotFound, room),
            AeroError::NotFound(_)
        ));
        assert!(matches!(
            map_group_dm_write_error(GroupDmWriteError::NotGroupDm, room),
            AeroError::Invalid(_)
        ));
        assert!(matches!(
            map_group_dm_write_error(GroupDmWriteError::Forbidden, room),
            AeroError::Forbidden(_)
        ));
        assert!(matches!(
            map_group_dm_write_error(GroupDmWriteError::InformationBarrier, room),
            AeroError::Forbidden(_)
        ));
        assert!(matches!(
            map_group_dm_write_error(GroupDmWriteError::InvalidInput("invalid".to_owned()), room),
            AeroError::Invalid(_)
        ));
    }
}
