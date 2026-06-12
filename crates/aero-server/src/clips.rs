//! Live-stream clips — viewer-marked shareable highlight ranges.
//!
//! A viewer marks a timestamped `[start_secs, end_secs]` range of a live stream /
//! VOD to share; playback reuses the stream's existing HLS playlist with a
//! client-side seek to that range — no media is processed here (mirroring
//! [`crate::vod`], which references the same playlist). Thin handlers over
//! [`ClipRepo`](aero_storage::ClipRepo).
//!
//! Any authenticated viewer may create a clip on an existing stream (clips are a
//! public sharing primitive, like the danmaku/gift routes in [`crate::routes`],
//! which gate on auth but not room membership); the source stream must exist
//! (`404` otherwise). The `[start, end]` range is validated at the edge by the
//! pure [`validate_range`] (`0 <= start < end` and `end - start <= MAX_CLIP_SECS`),
//! rejected `400`. Deleting is creator-scoped at the SQL layer (a non-creator's or
//! unknown id resolves to `false` ⇒ `404`). Mounted via [`routes`] and `.merge`d
//! into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{ClipId, Error as AeroError};
use aero_storage::ClipRepo;
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// Maximum clip length in whole seconds (10 minutes). A clip is a highlight, not a
/// re-broadcast, so the range is bounded — keeping both the stored bounds and the
/// client-side seek window sane.
const MAX_CLIP_SECS: i64 = 600;

/// All clip routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/streams/:id/clips",
            post(create_clip).get(list_clips),
        )
        .route("/api/clips/:cid", get(get_clip).delete(delete_clip))
        .route("/api/clips/:cid/share", post(share_clip))
}

/// Public clip route (no auth required) — mounted separately (no auth extractor).
pub fn public_routes() -> Router<AppState> {
    Router::new().route("/clips/:slug", get(get_clip_by_slug))
}

/// Build a [`ClipRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> ClipRepo {
    ClipRepo::new(s.pg.clone())
}

fn parse_stream(s: &str) -> Result<ulid::Ulid, AeroError> {
    ulid::Ulid::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("stream id: {e}")))
}

fn parse_clip(s: &str) -> Result<ClipId, AeroError> {
    ClipId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("clip id: {e}")))
}

/// Validate a clip's `[start, end]` range: `0 <= start < end` and
/// `end - start <= MAX_CLIP_SECS`. Pure, so the rule is unit-tested offline
/// (Postgres absent in CI). On success returns the bounds narrowed to `i32` for
/// the `INTEGER` columns; the upper-bound check guarantees they fit.
///
/// # Errors
/// [`AeroError::Invalid`] when the range is negative, empty/inverted, or longer
/// than [`MAX_CLIP_SECS`].
fn validate_range(start: i64, end: i64) -> Result<(i32, i32), AeroError> {
    if start < 0 {
        return Err(AeroError::Invalid("start_secs must be >= 0".into()));
    }
    if end <= start {
        return Err(AeroError::Invalid("end_secs must be greater than start_secs".into()));
    }
    if end - start > MAX_CLIP_SECS {
        return Err(AeroError::Invalid(format!(
            "clip too long: max {MAX_CLIP_SECS} seconds"
        )));
    }
    // Both bounds are in `0..=start+MAX_CLIP_SECS`; the only way to exceed i32 is a
    // huge `start`, which the cast guards against (never panics, maps to Invalid).
    let start = i32::try_from(start).map_err(|_| AeroError::Invalid("start_secs too large".into()))?;
    let end = i32::try_from(end).map_err(|_| AeroError::Invalid("end_secs too large".into()))?;
    Ok((start, end))
}

#[derive(Deserialize)]
struct CreateClipReq {
    /// Human-readable title for the clip.
    title: String,
    /// Inclusive start offset into the stream, in whole seconds.
    start_secs: i64,
    /// Exclusive end offset into the stream, in whole seconds (`> start_secs`).
    end_secs: i64,
}

/// `POST /api/streams/:id/clips` — mark a `[start_secs, end_secs]` range of this
/// stream as a shareable clip (creator = caller). The stream must exist (`404`);
/// a blank title or an invalid range is rejected `400`. Returns the created row.
async fn create_clip(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(stream_str): Path<String>,
    Json(req): Json<CreateClipReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream = parse_stream(&stream_str)?;
    // The source stream must exist — a clip with no stream could never play back.
    if s.streams.get(stream).await.map_err(AeroError::from)?.is_none() {
        return Err(AeroError::NotFound(format!("stream {stream}")).into());
    }

    let title = req.title.trim();
    if title.is_empty() {
        return Err(AeroError::Invalid("title must not be empty".into()).into());
    }
    if title.len() > 256 {
        return Err(AeroError::Invalid("title too long".into()).into());
    }
    let (start, end) = validate_range(req.start_secs, req.end_secs)?;

    let id = repo(&s)
        .create(stream, auth.participant_id, title, start, end)
        .await
        .map_err(AeroError::from)?;
    // Re-read so the response carries the full, canonical row (created_at).
    let row = repo(&s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("clip".into()))?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

/// `GET /api/streams/:id/clips` — this stream's clips, newest first. Any
/// authenticated viewer may list them.
async fn list_clips(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(stream_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream = parse_stream(&stream_str)?;
    let clips = repo(&s)
        .list_for_stream(stream)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(clips).map_err(AeroError::from)?))
}

/// `GET /api/clips/:cid` — fetch a single clip by id. `404` if it does not exist.
async fn get_clip(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_clip(&id_str)?;
    let clip = repo(&s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("clip {id}")))?;
    Ok(Json(serde_json::to_value(clip).map_err(AeroError::from)?))
}

/// `DELETE /api/clips/:cid` — delete one of the caller's own clips. Creator-scoped:
/// a `404` if it isn't the caller's clip (someone else's or unknown).
async fn delete_clip(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_clip(&id_str)?;
    let removed = repo(&s)
        .delete(id, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    if !removed {
        return Err(AeroError::NotFound(format!("clip {id}")).into());
    }
    Ok(Json(serde_json::json!({ "deleted": true })))
}

/// `POST /api/clips/:cid/share` — generate (or return existing) a shareable URL
/// for a clip. The caller must be the clip's creator. Returns
/// `{share_url, slug}` where `share_url` is `/clips/{slug}`.
async fn share_clip(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_clip(&id_str)?;
    // Verify the clip exists and the caller is its creator.
    let clip = repo(&s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("clip {id}")))?;
    if clip.creator_id != auth.participant_id {
        return Err(AeroError::Forbidden("only the clip creator may generate a share URL".into()).into());
    }

    // Generate a slug if none exists yet; if one already exists, re-read.
    let slug = match repo(&s)
        .generate_share_slug(id)
        .await
        .map_err(AeroError::from)?
    {
        Some(s) => s,
        None => {
            // Slug already existed — re-read the clip to get it.
            repo(&s)
                .get(id)
                .await
                .map_err(AeroError::from)?
                .and_then(|c| c.share_slug)
                .ok_or_else(|| AeroError::Internal(anyhow::anyhow!("share_slug missing after generate")))?
        }
    };

    let share_url = format!("/clips/{slug}");
    Ok(Json(serde_json::json!({ "share_url": share_url, "slug": slug })))
}

/// `GET /clips/:slug` — public, no auth required. Returns clip metadata by
/// its share slug. Increments the clip's view count on each successful fetch.
async fn get_clip_by_slug(
    State(s): State<AppState>,
    Path(slug): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let clip = repo(&s)
        .get_by_slug(slug.trim())
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("clip with slug {slug}")))?;
    // Best-effort view count increment — do not fail the fetch if the update errors.
    if let Err(e) = repo(&s).increment_view_count(clip.id).await {
        tracing::warn!(error = ?e, clip = %clip.id, "failed to increment clip view count");
    }
    Ok(Json(serde_json::to_value(clip).map_err(AeroError::from)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_range_accepts_valid_window() {
        assert_eq!(validate_range(0, 1).unwrap(), (0, 1));
        assert_eq!(validate_range(30, 75).unwrap(), (30, 75));
        // Exactly the max length is allowed.
        let end = 10 + MAX_CLIP_SECS;
        assert_eq!(
            validate_range(10, end).unwrap(),
            (10, i32::try_from(end).unwrap())
        );
    }

    #[test]
    fn validate_range_rejects_negative_start() {
        assert!(validate_range(-1, 10).is_err());
    }

    #[test]
    fn validate_range_rejects_empty_or_inverted() {
        assert!(validate_range(10, 10).is_err(), "empty range");
        assert!(validate_range(20, 5).is_err(), "inverted range");
    }

    #[test]
    fn validate_range_rejects_too_long() {
        assert!(validate_range(0, MAX_CLIP_SECS + 1).is_err());
    }
}
