//! Custom user profile fields — title / pronouns / timezone / phone / status.
//!
//! Self-service profile metadata a participant fills in about themselves, kept
//! in a SIDE table ([`ProfileRepo`](aero_storage::ProfileRepo)) keyed by
//! `participant_id`. The core `participants` row and its `update_me` handler are
//! deliberately untouched — a profile is created lazily on first `PUT` and every
//! field is optional. `PUT` replaces the whole profile (REST upsert semantics):
//! an omitted or blank field clears that column.
//!
//! Thin handlers over [`ProfileRepo`](aero_storage::ProfileRepo). Fields are
//! trimmed, blanks treated as "unset", and each is capped at
//! [`MAX_FIELD_LEN`] characters (pure [`normalize_field`], unit-tested offline).
//! Reading another participant's profile is auth-gated only (profiles are
//! directory-style public metadata, mirroring `GET /api/participants/:id`).
//! Mounted via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId};
use aero_storage::ProfileRepo;
use axum::{
    extract::{Path, State},
    routing::{get, put},
    Json, Router,
};
use time::OffsetDateTime;
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All custom-profile routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/me/profile", put(put_my_profile).get(get_my_profile))
        .route("/api/me/profile/status", axum::routing::patch(patch_profile_status))
        .route("/api/participants/:id/profile", get(get_participant_profile))
}

/// Maximum length (in characters) of any single profile field.
const MAX_FIELD_LEN: usize = 128;

/// Build a [`ProfileRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> ProfileRepo {
    ProfileRepo::new(s.pg.clone())
}

fn parse_participant(s: &str) -> Result<ParticipantId, AeroError> {
    ParticipantId::from_str(s.trim())
        .map_err(|e| AeroError::Invalid(format!("participant id: {e}")))
}

/// Validate and normalize one optional profile field: trim it, treat an empty
/// result as "unset" (`None`), and reject anything longer than
/// [`MAX_FIELD_LEN`] characters. Pure, so the length cap is unit-tested offline.
fn normalize_field(field: &str, value: Option<&str>) -> Result<Option<String>, AeroError> {
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) if v.chars().count() > MAX_FIELD_LEN => Err(AeroError::Invalid(format!(
            "{field} too long (max {MAX_FIELD_LEN} chars)"
        ))),
        Some(v) => Ok(Some(v.to_owned())),
        None => Ok(None),
    }
}

/// The empty/default profile JSON returned by `GET /api/me/profile` when the
/// caller has never saved one — same shape as a real row, all fields null.
fn empty_profile(participant: ParticipantId) -> serde_json::Value {
    serde_json::json!({
        "participant_id": participant,
        "title": serde_json::Value::Null,
        "pronouns": serde_json::Value::Null,
        "timezone": serde_json::Value::Null,
        "phone": serde_json::Value::Null,
        "status_text": serde_json::Value::Null,
        "updated_at": serde_json::Value::Null,
    })
}

#[derive(Deserialize)]
struct UpdateProfileReq {
    /// Job title / role; omitted or blank clears it.
    #[serde(default)]
    title: Option<String>,
    /// Preferred pronouns; omitted or blank clears it.
    #[serde(default)]
    pronouns: Option<String>,
    /// IANA timezone name; omitted or blank clears it.
    #[serde(default)]
    timezone: Option<String>,
    /// Contact phone number; omitted or blank clears it.
    #[serde(default)]
    phone: Option<String>,
    /// Free-form status text; omitted or blank clears it.
    #[serde(default)]
    status_text: Option<String>,
}

/// `PUT /api/me/profile` — create or replace the caller's profile. Every field
/// is optional; an omitted or blank field clears that column, and any field
/// longer than [`MAX_FIELD_LEN`] characters is rejected `400`. Returns the
/// canonical stored row.
async fn put_my_profile(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<UpdateProfileReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let title = normalize_field("title", req.title.as_deref())?;
    let pronouns = normalize_field("pronouns", req.pronouns.as_deref())?;
    let timezone = normalize_field("timezone", req.timezone.as_deref())?;
    let phone = normalize_field("phone", req.phone.as_deref())?;
    let status_text = normalize_field("status_text", req.status_text.as_deref())?;

    let r = repo(&s);
    r.upsert(
        auth.participant_id,
        title.as_deref(),
        pronouns.as_deref(),
        timezone.as_deref(),
        phone.as_deref(),
        status_text.as_deref(),
    )
    .await
    .map_err(AeroError::from)?;
    // Re-read so the response carries the full, canonical row (updated_at).
    let row = r
        .get(auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("profile".into()))?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

/// `GET /api/me/profile` — the caller's own profile, or an empty/default object
/// (all fields null) when they have never saved one.
async fn get_my_profile(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let profile = repo(&s)
        .get(auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    match profile {
        Some(p) => Ok(Json(serde_json::to_value(p).map_err(AeroError::from)?)),
        None => Ok(Json(empty_profile(auth.participant_id))),
    }
}

/// `GET /api/participants/:id/profile` — another participant's profile. Auth-only
/// (profiles are directory-style public metadata); `404` if that participant has
/// never saved one.
async fn get_participant_profile(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let target = parse_participant(&id_str)?;
    let profile = repo(&s)
        .get(target)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("profile for {target}")))?;
    Ok(Json(serde_json::to_value(profile).map_err(AeroError::from)?))
}

/// Maximum lifetime (in minutes) for a profile-level status expiry.
/// 60 days × 24 h × 60 min = 86 400 minutes.
const MAX_STATUS_EXPIRES_IN_MINS: i64 = 60 * 24 * 60;

#[derive(Deserialize)]
struct PatchStatusReq {
    /// Status text (e.g. "On vacation"). Absent or null clears it.
    #[serde(default)]
    status_text: Option<String>,
    /// Emoji shorthand (e.g. ":palm_tree:"). Absent or null clears it.
    #[serde(default)]
    status_emoji: Option<String>,
    /// Auto-expire after this many minutes from now. Absent or null = never.
    #[serde(default)]
    expires_in_mins: Option<i64>,
}

/// `PATCH /api/me/profile/status` — set (or clear) the profile-level custom
/// status emoji, text, and optional expiry. The three columns are updated
/// atomically; other profile fields (title, pronouns, etc.) are untouched.
async fn patch_profile_status(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<PatchStatusReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let text = normalize_field("status_text", req.status_text.as_deref())?;
    let emoji = normalize_field("status_emoji", req.status_emoji.as_deref())?;

    let expires_at = match req.expires_in_mins {
        None => None,
        Some(mins) if (1..=MAX_STATUS_EXPIRES_IN_MINS).contains(&mins) => {
            Some(OffsetDateTime::now_utc() + time::Duration::minutes(mins))
        }
        Some(mins) => {
            return Err(AeroError::Invalid(format!(
                "expires_in_mins must be 1..={MAX_STATUS_EXPIRES_IN_MINS}, got {mins}"
            ))
            .into())
        }
    };

    repo(&s)
        .set_status(
            auth.participant_id,
            text.as_deref(),
            emoji.as_deref(),
            expires_at,
        )
        .await
        .map_err(AeroError::from)?;

    // Re-read the full profile row so the response is canonical (includes updated_at).
    let row = repo(&s).get(auth.participant_id).await.map_err(AeroError::from)?;
    match row {
        Some(p) => Ok(Json(serde_json::to_value(p).map_err(AeroError::from)?)),
        None => Ok(Json(serde_json::json!({
            "participant_id": auth.participant_id,
            "status_text": text,
            "status_emoji": emoji,
            "status_expires_at": expires_at.map(|t| t.unix_timestamp()),
        }))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_field_trims_blanks_and_caps_length() {
        // Absent / whitespace-only ⇒ unset.
        assert_eq!(normalize_field("title", None).unwrap(), None);
        assert_eq!(normalize_field("title", Some("   ")).unwrap(), None);
        // Trimmed and kept.
        assert_eq!(
            normalize_field("title", Some("  Engineer  ")).unwrap(),
            Some("Engineer".to_owned())
        );
        // Exactly MAX_FIELD_LEN chars is allowed; one more is rejected.
        let at_cap = "a".repeat(MAX_FIELD_LEN);
        assert_eq!(
            normalize_field("title", Some(&at_cap)).unwrap(),
            Some(at_cap.clone())
        );
        let over_cap = "a".repeat(MAX_FIELD_LEN + 1);
        assert!(normalize_field("status_text", Some(&over_cap)).is_err());
    }
}
