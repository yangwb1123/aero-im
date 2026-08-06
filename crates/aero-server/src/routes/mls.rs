//! MLS (Messaging Layer Security) opaque-bytes relay endpoints.
//!
//! The server acts as a transparent relay for MLS `KeyPackages` and group state:
//! clients run `OpenMLS` and upload/download encrypted blobs that the server
//! cannot decrypt. Membership gating is enforced by room access checks.
//! Split from the monolithic `routes.rs` (`REFACTOR_PLAN.md` Step 7 clean-up).

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId};
use aero_storage::mls::{
    MAX_MLS_CIPHERSUITE_BYTES, MAX_MLS_GROUP_ID_BYTES, MAX_MLS_GROUP_STATE_BYTES,
    MAX_MLS_KEY_PACKAGE_BYTES,
};
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
        .route(
            "/api/rooms/:room/mls/key-packages/:participant",
            get(mls_consume_room_kp),
        )
        .route("/api/mls/groups", axum::routing::post(mls_upsert_group))
        .route("/api/mls/groups/:gid", get(mls_get_group))
}

#[derive(Deserialize)]
struct PublishKpReq {
    ciphersuite: String,
    /// Base64-encoded `KeyPackage` bytes.
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

fn validate_ciphersuite(raw: &str) -> Result<String, AeroError> {
    let value = raw.trim();
    if value.is_empty() {
        return Err(AeroError::Invalid(
            "MLS ciphersuite must not be empty".into(),
        ));
    }
    if value.len() > MAX_MLS_CIPHERSUITE_BYTES || !value.is_ascii() {
        return Err(AeroError::Invalid("MLS ciphersuite is too large".into()));
    }
    Ok(value.to_owned())
}

fn validate_group_id(bytes: Vec<u8>) -> Result<aero_common::mls::MlsGroupId, AeroError> {
    if bytes.is_empty() {
        return Err(AeroError::Invalid("MLS group id must not be empty".into()));
    }
    if bytes.len() > MAX_MLS_GROUP_ID_BYTES {
        return Err(AeroError::Invalid("MLS group id is too large".into()));
    }
    Ok(aero_common::mls::MlsGroupId::new(bytes))
}

/// `POST /api/mls/key-packages` — publish a `KeyPackage` for the current user.
/// The payload is an opaque MLS `KeyPackage` byte blob (base64-encoded).
async fn mls_publish_kp(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<PublishKpReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let payload = b64_decode(&req.payload_b64)?;
    if payload.is_empty() {
        return Err(AeroError::Invalid("KeyPackage must not be empty".into()).into());
    }
    if payload.len() > MAX_MLS_KEY_PACKAGE_BYTES {
        return Err(AeroError::Invalid("KeyPackage too large".into()).into());
    }
    let ciphersuite = validate_ciphersuite(&req.ciphersuite)?;
    let kp = s
        .key_packages
        .publish(auth.participant_id, &ciphersuite, payload)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({
        "id": kp.id,
        "ciphersuite": kp.ciphersuite,
        "created_at": kp.created_at,
    })))
}

/// Legacy `GET /api/mls/key-packages/:participant` compatibility endpoint.
///
/// It only permits a participant to consume their own package. Adding another
/// participant to a room must use the canonical room-scoped endpoint below.
async fn mls_consume_kp(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(pid_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let target = ParticipantId::from_str(&pid_str)
        .map_err(|e| AeroError::Invalid(format!("participant id: {e}")))?;
    let kp = s
        .key_packages
        .consume_own(auth.participant_id, target)
        .await?
        .ok_or_else(|| AeroError::NotFound("no fresh KeyPackages".into()))?;
    Ok(Json(key_package_json(&kp)))
}

/// `GET /api/rooms/:room/mls/key-packages/:participant` — claim one target
/// package only while both requester and target retain effective room access.
async fn mls_consume_room_kp(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((room_str, pid_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&room_str)?;
    let target = ParticipantId::from_str(&pid_str)
        .map_err(|e| AeroError::Invalid(format!("participant id: {e}")))?;
    let kp = s
        .key_packages
        .consume_one_authorized(auth.participant_id, target, room)
        .await?
        .ok_or_else(|| AeroError::NotFound("no fresh KeyPackages".into()))?;
    Ok(Json(key_package_json(&kp)))
}

fn key_package_json(kp: &aero_common::mls::KeyPackage) -> serde_json::Value {
    serde_json::json!({
        "id": kp.id,
        "participant_id": kp.participant_id,
        "ciphersuite": kp.ciphersuite,
        "payload_b64": b64_encode(&kp.payload),
        "created_at": kp.created_at,
    })
}

#[derive(Deserialize)]
struct UpsertGroupReq {
    group_id_b64: String,
    ciphersuite: String,
    epoch: u64,
    state_b64: String,
    room_id: String,
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
    let group_id = validate_group_id(b64_decode(&req.group_id_b64)?)?;
    let state_bytes = b64_decode(&req.state_b64)?;
    if state_bytes.is_empty() {
        return Err(AeroError::Invalid("MLS group state must not be empty".into()).into());
    }
    if state_bytes.len() > MAX_MLS_GROUP_STATE_BYTES {
        return Err(AeroError::Invalid("MLS group state is too large".into()).into());
    }
    let room = parse_room_id(&req.room_id)?;
    let ciphersuite = validate_ciphersuite(&req.ciphersuite)?;

    let g = aero_common::mls::MlsGroupState {
        group_id,
        room_id: Some(room),
        ciphersuite,
        epoch: req.epoch,
        state: state_bytes,
        updated_at: time::OffsetDateTime::now_utc(),
    };
    s.mls_groups
        .upsert_authorized(auth.participant_id, &g)
        .await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// `GET /api/mls/groups/:gid` — get the MLS group state (base64-encoded).
/// Only members of the group's room may read its (encrypted) state + metadata.
async fn mls_get_group(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(gid_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let gid = validate_group_id(b64_decode(&gid_str)?)?;
    let g = s
        .mls_groups
        .get_authorized(auth.participant_id, &gid)
        .await?;
    Ok(Json(serde_json::json!({
        "group_id_b64": b64_encode(g.group_id.as_bytes()),
        "room_id": g.room_id,
        "ciphersuite": g.ciphersuite,
        "epoch": g.epoch,
        "state_b64": b64_encode(&g.state),
        "updated_at": g.updated_at,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_mls_inputs_are_nonempty_and_bounded() {
        assert!(validate_ciphersuite("").is_err());
        assert!(validate_ciphersuite("   ").is_err());
        assert!(validate_ciphersuite("MLS_TEST").is_ok());
        assert!(validate_ciphersuite(&"x".repeat(MAX_MLS_CIPHERSUITE_BYTES + 1)).is_err());
        assert!(validate_group_id(Vec::new()).is_err());
        assert!(validate_group_id(vec![1; MAX_MLS_GROUP_ID_BYTES]).is_ok());
        assert!(validate_group_id(vec![1; MAX_MLS_GROUP_ID_BYTES + 1]).is_err());
    }
}
