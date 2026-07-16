//! Cross-room (workspace-wide) message search — the global counterpart to the
//! per-room `POST /api/rooms/:id/search` in [`crate::routes`].
//!
//! Searches every room the caller belongs to (Slack-style global search). The
//! membership scoping is enforced in SQL by
//! [`MessageRepo::search_all_rooms`](aero_storage::MessageRepo::search_all_rooms)
//! (a `JOIN room_members`), so results can never leak a room the caller isn't in
//! — there is no post-filter to get wrong. Mounted via [`routes`] and `.merge`d
//! into the gateway router, mirroring [`crate::collab`].

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, WorkspaceId};
use axum::{extract::State, routing::post, Json, Router};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// The cross-room search route, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/search", post(search_all))
}

/// Default number of hits when the client doesn't specify a `limit`.
const DEFAULT_SEARCH_LIMIT: i64 = 20;

/// Hard ceiling on hits per search request. Cross-room search runs FTS/trigram
/// scans, so an unbounded `limit` lets a single request materialise an
/// arbitrarily large result set — memory + JSON-serialization pressure, and a
/// cheap amplification/DoS vector against the shared DB pool. Every request is
/// clamped to this bound regardless of what the client asks for.
const MAX_SEARCH_LIMIT: i64 = 100;

/// Clamp a client-supplied limit into `[1, MAX_SEARCH_LIMIT]`, defaulting a
/// missing value to [`DEFAULT_SEARCH_LIMIT`].
///
/// A non-positive value (`0` or negative) floors up to `1` rather than erroring:
/// search is best-effort and a degenerate limit should still return a
/// deterministic, bounded result rather than an unbounded scan or an
/// accidentally-empty one.
fn clamp_search_limit(requested: Option<i64>) -> i64 {
    requested.unwrap_or(DEFAULT_SEARCH_LIMIT).clamp(1, MAX_SEARCH_LIMIT)
}

#[derive(Deserialize)]
struct SearchAllReq {
    query: String,
    #[serde(default)]
    limit: Option<i64>,
    /// Optional tenant scope. When present, only the caller's rooms in that
    /// workspace are searched (`search_all_rooms_in_workspace`); absent ⇒ every
    /// room the caller belongs to, across tenants.
    #[serde(default)]
    workspace_id: Option<String>,
}

/// `POST /api/search` — full-text/trigram search across all rooms the caller is
/// a member of. The repository's `JOIN room_members` is the security boundary;
/// no room the caller isn't in can appear in the results.
async fn search_all(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<SearchAllReq>,
) -> ApiResult<Json<serde_json::Value>> {
    if req.query.trim().is_empty() {
        return Err(AeroError::Invalid("empty query".into()).into());
    }
    let limit = clamp_search_limit(req.limit);

    // ROADMAP 方向四: cross-room search is the heaviest read in the gateway (FTS +
    // trigram scans with a `JOIN room_members`) and the most replica-friendly —
    // mild replication lag (a message searchable a few ms late) is acceptable, and
    // it is NOT a read-your-writes path (unlike `list_since` reconnect backfill,
    // which must stay on the primary). So route it to the READ pool: the replica
    // when one is configured, else the primary unchanged (`pg_read == pg`). The
    // `JOIN room_members` security boundary is identical on either pool.
    let messages = aero_storage::MessageRepo::new(s.pg_read.clone());
    let hits = match req.workspace_id.as_deref() {
        Some(raw) => {
            let ws = WorkspaceId::from_str(raw.trim())
                .map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))?;
            messages
                .search_all_rooms_in_workspace(auth.participant_id, ws, &req.query, limit)
                .await
                .map_err(AeroError::from)?
        }
        None => messages
            .search_all_rooms(auth.participant_id, &req.query, limit)
            .await
            .map_err(AeroError::from)?,
    };

    Ok(Json(serde_json::json!({
        "query": req.query,
        "results": hits.into_iter().map(|h| {
            serde_json::json!({
                "score": h.score,
                "headline": h.headline,
                "message": h.message,
            })
        }).collect::<Vec<_>>(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_limit_defaults_when_absent() {
        assert_eq!(clamp_search_limit(None), DEFAULT_SEARCH_LIMIT);
    }

    #[test]
    fn search_limit_clamps_to_ceiling() {
        assert_eq!(clamp_search_limit(Some(1_000_000)), MAX_SEARCH_LIMIT);
        assert_eq!(clamp_search_limit(Some(MAX_SEARCH_LIMIT + 1)), MAX_SEARCH_LIMIT);
    }

    #[test]
    fn search_limit_floors_non_positive_to_one() {
        // A degenerate limit must never reach SQL as 0 or a negative bind.
        assert_eq!(clamp_search_limit(Some(0)), 1);
        assert_eq!(clamp_search_limit(Some(-5)), 1);
        assert_eq!(clamp_search_limit(Some(i64::MIN)), 1);
    }

    #[test]
    fn search_limit_passes_through_in_range() {
        assert_eq!(clamp_search_limit(Some(1)), 1);
        assert_eq!(clamp_search_limit(Some(50)), 50);
        assert_eq!(clamp_search_limit(Some(MAX_SEARCH_LIMIT)), MAX_SEARCH_LIMIT);
    }
}
