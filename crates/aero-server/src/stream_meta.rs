//! Live-stream metadata edit (change the title while live).
//!
//! The standard Twitch/YouTube "edit stream info" control: a stream's *owner*
//! (`stream.owner_id == auth.participant_id`) renames their broadcast without
//! recreating it or interrupting the ingest. The `streams` table carries no
//! other free-text metadata column (description/category live in separate
//! tables, see migration 0050), so only `title` is editable here.
//!
//! Route (owner-gated):
//! * `PATCH /api/streams/:id` body `{ "title"?: "..." }` — load the stream
//!   (`404` if absent), require caller == creator (`403` otherwise), validate
//!   the new title (`400` on empty/over-long), persist, and return the updated
//!   stream. Mounted via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, Result as AeroResult};
use axum::{
    extract::{Path, State},
    routing::patch,
    Json, Router,
};
use serde::Deserialize;
use ulid::Ulid;

use crate::error::ApiResult;
use crate::state::AppState;

/// Maximum stream-title length. Mirrors the `stream_create` body (no explicit
/// cap there, but a stream card embeds the title); keep edits bounded so a
/// renamed title stays renderable everywhere the original was.
const MAX_TITLE_LEN: usize = 200;

/// Stream-metadata routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/streams/:id", patch(update_meta))
}

fn parse_stream_id(s: &str) -> AeroResult<Ulid> {
    Ulid::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("stream id: {e}")))
}

#[derive(Deserialize)]
struct UpdateMetaReq {
    #[serde(default)]
    title: Option<String>,
}

/// Validate and normalize a requested title: trim, reject empty, reject over-long.
/// Pure, so the edge validation is unit-tested offline (no database needed).
///
/// # Errors
/// [`AeroError::Invalid`] when the trimmed title is empty or exceeds
/// [`MAX_TITLE_LEN`].
fn validate_title(raw: &str) -> AeroResult<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AeroError::Invalid("title must not be empty".into()));
    }
    if trimmed.chars().count() > MAX_TITLE_LEN {
        return Err(AeroError::Invalid(format!("title too long (max {MAX_TITLE_LEN} chars)")));
    }
    Ok(trimmed.to_owned())
}

/// `PATCH /api/streams/:id` — owner-only stream metadata edit.
///
/// * `404` when the stream does not exist.
/// * `403` when the caller is not the stream's owner.
/// * `400` when a provided `title` is empty or over-long.
///
/// When `title` is absent the request is a no-op edit and the unchanged stream
/// is returned (so a client can `PATCH {}` to fetch the canonical owner view).
async fn update_meta(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<UpdateMetaReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_stream_id(&id_str)?;

    // Load first so a missing stream is 404 and a non-owner is a distinct 403
    // (rather than collapsing both into one status, which would leak less but
    // also confuse a legitimate owner editing a typo'd id).
    let stream = s
        .streams
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("stream {id}")))?;

    if stream.owner_id != auth.participant_id {
        return Err(AeroError::Forbidden("not the stream owner".into()).into());
    }

    if let Some(raw) = req.title.as_deref() {
        let title = validate_title(raw)?;
        // The owner check above already authorized this; an unmatched row here
        // would mean the stream was deleted between the load and the update — a
        // benign race we surface as 404.
        let matched = s
            .streams
            .update_title(id, &title)
            .await
            .map_err(AeroError::from)?;
        if !matched {
            return Err(AeroError::NotFound(format!("stream {id}")).into());
        }
    }

    // Re-read so the response reflects the persisted row (and any concurrent
    // status change from the ingest path).
    let updated = s
        .streams
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("stream {id}")))?;
    Ok(Json(serde_json::to_value(updated).map_err(AeroError::from)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_title_rejects_empty_and_whitespace() {
        assert!(validate_title("").is_err());
        assert!(validate_title("   ").is_err());
        assert!(validate_title("\t\n ").is_err());
    }

    #[test]
    fn validate_title_trims_and_accepts() {
        assert_eq!(validate_title("  My Stream  ").expect("ok"), "My Stream");
        assert_eq!(validate_title("hello").expect("ok"), "hello");
    }

    #[test]
    fn validate_title_rejects_over_long() {
        let long: String = "a".repeat(MAX_TITLE_LEN + 1);
        assert!(validate_title(&long).is_err());
        let max: String = "b".repeat(MAX_TITLE_LEN);
        assert_eq!(validate_title(&max).expect("ok").chars().count(), MAX_TITLE_LEN);
    }

    #[test]
    fn validate_title_counts_chars_not_bytes() {
        // Multi-byte chars count once each, so a string of MAX multi-byte chars
        // is accepted even though its byte length far exceeds the cap.
        let multibyte: String = "界".repeat(MAX_TITLE_LEN);
        assert!(multibyte.len() > MAX_TITLE_LEN, "precondition: byte len exceeds cap");
        assert!(validate_title(&multibyte).is_ok());
        let over: String = "界".repeat(MAX_TITLE_LEN + 1);
        assert!(validate_title(&over).is_err());
    }
}
