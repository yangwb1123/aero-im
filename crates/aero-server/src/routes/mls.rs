//! MLS (Messaging Layer Security) opaque-bytes relay endpoints.
//!
//! The server acts as a transparent relay for MLS KeyPackages and group state:
//! clients run OpenMLS and upload/download encrypted blobs that the server
//! cannot decrypt. Membership gating is enforced by room access checks.
//! Split from the monolithic `routes.rs` (REFACTOR_PLAN.md Step 7 clean-up).

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId};
use axum::{
    extract::{Path, State},
    routing::get,
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::routes::helpers::parse_room_id;
use crate::state::AppState;

/// All MLS endpoints, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/mls/key-packages", axum::routing::post(mls_publish_kp))
        .route("/api/mls/key-packages/:participant", get(mls_consume_kp))
        .route("/api/mls/groups", axum::routing::post(mls_upsert_group))
        .route("/api/mls/groups/:gid", get(mls_get_group))
}

#[derive(Deserialize)]
struct PublishKpReq {
    ciphersuite: String,
    /// Base64-encoded KeyPackage bytes.
    payload_b64: String,
}

fn b64_decode(s: &str) -> Result<Vec<u8>, AeroError> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| AeroError::Invalid(format!("base64: {e}")))
}
fn b64_encode(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(b)
}

/// `POST /api/mls/key-packages` — publish a KeyPackage for the current user.
/// The payload is an opaque MLS KeyPackage byte blob (base64-encoded).
async fn mls_publish_kp(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<PublishKpReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let payload = b64_decode(&req.payload_b64)?;
    if payload.len() > 16 * 1024 {
        return Err(AeroError::Invalid("KeyPackage too large".into()).into());
    }
    let kp = s
        .key_packages
        .publish(auth.participant_id, &req.ciphersuite, payload)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({
        "id": kp.id,
        "ciphersuite": kp.ciphersuite,
        "created_at": kp.created_at,
    })))
}

/// `GET /api/mls/key-packages/:participant` — consume one KeyPackage for a
/// target participant (atomically claims and deletes it, so it cannot be
/// re-consumed). The caller must be authenticated but does NOT need room access
/// to see KPs — KPs are public-key material and contain no room-scoped data.
async fn mls_consume_kp(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(pid_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let target = ParticipantId::from_str(&pid_str)
        .map_err(|e| AeroError::Invalid(format!("participant id: {e}")))?;
    let kp = s
        .key_packages
        .consume_one(target)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("no fresh KeyPackages".into()))?;
    Ok(Json(serde_json::json!({
        "id": kp.id,
        "participant_id": kp.participant_id,
        "ciphersuite": kp.ciphersuite,
        "payload_b64": b64_encode(&kp.payload),
        "created_at": kp.created_at,
    })))
}

#[derive(Deserialize)]
struct UpsertGroupReq {
    group_id_b64: String,
    ciphersuite: String,
    epoch: u64,
    state_b64: String,
    #[serde(default)]
    room_id: Option<String>,
}

/// `POST /api/mls/groups` — upsert the caller's MLS group state.
/// Authorization: the group is bound to a room and only room members may read
/// or write its state. For an existing group, authorization is against its
/// current room; for a new group, against the requested room.
async fn mls_upsert_group(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<UpsertGroupReq>,
) -> ApiResult<axum::http::StatusCode> {
    let group_id = aero_common::mls::MlsGroupId::new(b64_decode(&req.group_id_b64)?);
    let state_bytes = b64_decode(&req.state_b64)?;
    let req_room = req.room_id.as_deref().map(parse_room_id).transpose()?;

    // Authorization: an MLS group is bound to a room and only its members may
    // read or write its state. For an EXISTING group authorize against its
    // CURRENT room; for a NEW group, the requested room. Room-less groups are
    // rejected (fail closed).
    let existing = s.mls_groups.get(&group_id).await.map_err(AeroError::from)?;
    let room = match &existing {
        Some(g) => g.room_id,
        None => req_room,
    }
    .ok_or_else(|| AeroError::Forbidden("mls group must be room-bound".into()))?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    // An existing group may not be moved to a different room.
    if let Some(g) = &existing {
        if req_room.is_some() && req_room != g.room_id {
            return Err(
                AeroError::Forbidden("cannot reassign an mls group to another room".into()).into(),
            );
        }
    }

    let g = aero_common::mls::MlsGroupState {
        group_id,
        room_id: Some(room),
        ciphersuite: req.ciphersuite,
        epoch: req.epoch,
        state: state_bytes,
        updated_at: time::OffsetDateTime::now_utc(),
    };
    s.mls_groups.upsert(&g).await.map_err(AeroError::from)?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// `GET /api/mls/groups/:gid` — get the MLS group state (base64-encoded).
/// Only members of the group's room may read its (encrypted) state + metadata.
async fn mls_get_group(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(gid_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let gid_bytes = b64_decode(&gid_str)?;
    let gid = aero_common::mls::MlsGroupId::new(gid_bytes);
    let g = s
        .mls_groups
        .get(&gid)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("group".into()))?;
    // Only members of the group's room may read its (encrypted) state + metadata.
    let room = g
        .room_id
        .ok_or_else(|| AeroError::Forbidden("mls group not room-bound".into()))?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    Ok(Json(serde_json::json!({
        "group_id_b64": b64_encode(g.group_id.as_bytes()),
        "room_id": g.room_id,
        "ciphersuite": g.ciphersuite,
        "epoch": g.epoch,
        "state_b64": b64_encode(&g.state),
        "updated_at": g.updated_at,
    })))
}
