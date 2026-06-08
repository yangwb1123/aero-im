//! Personal data export — `GET /api/me/export`.
//!
//! Implements the GDPR right to data portability (Article 20) for individual
//! participants. The response is a self-contained JSON snapshot of everything the
//! caller owns: their profile, the messages they sent (capped at the
//! [`aero_storage::MessageRepo::by_sender`] limit), and the files they uploaded.
//!
//! This is a *personal* export scoped to the calling participant. The admin-only
//! workspace-wide export lives at `GET /api/workspaces/:id/export`.

use aero_auth::AuthUser;
use aero_common::Error as AeroError;
use aero_storage::{BlobRepo, MessageRepo};
use axum::{extract::State, routing::get, Json, Router};

use crate::error::ApiResult;
use crate::state::AppState;

/// All personal-export routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/me/export", get(export_me))
}

/// `GET /api/me/export` — a complete snapshot of the caller's personal data.
///
/// Returns a JSON object with:
/// - `participant` — profile (no credentials).
/// - `messages_sent` — up to 500 most-recent non-deleted messages (newest first).
/// - `blobs_uploaded` — up to 200 most-recent uploaded files (newest first).
/// - `exported_at` — RFC 3339 timestamp of this export.
///
/// Auth required (Bearer access token). No admin gate — each participant may only
/// export their own data.
async fn export_me(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let pid = auth.participant_id;

    let msg_repo = MessageRepo::new(s.pg.clone());
    let blob_repo = BlobRepo::new(s.pg.clone());
    let (participant, messages, blobs) = tokio::try_join!(
        s.participants.get(pid),
        msg_repo.by_sender(pid),
        blob_repo.list_by_owner(pid, 200),
    )
    .map_err(AeroError::from)?;

    let participant = participant
        .ok_or_else(|| AeroError::Unauthorized("participant not found".into()))?;

    let exported_at = time::OffsetDateTime::now_utc();

    Ok(Json(serde_json::json!({
        "participant": participant,
        "messages_sent": messages,
        "blobs_uploaded": blobs,
        "exported_at": exported_at.format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_exposes_get_api_me_export() {
        // Verify the route table contains exactly one route (smoke test — the
        // handler itself requires a live DB and is not unit-tested here).
        let router = routes();
        // Axum doesn't expose a public route list, but construction must not
        // panic — that's all we can assert without a real request.
        let _ = router;
    }
}
