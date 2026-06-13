//! Advanced cross-room search — Slack-style query operators.
//!
//! The structured counterpart to [`crate::search`]: the caller's query may carry
//! `from:@<id>`, `in:<roomid>`, `before:<msgid>`, and `after:<msgid>` operators
//! alongside free text. The operators are parsed out
//! ([`parse_search_query`](aero_storage::parse_search_query)) and AND-ed into the
//! SAME membership-scoped cross-room search as [`crate::search`]
//! ([`AdvancedSearchRepo`](aero_storage::AdvancedSearchRepo), whose
//! `JOIN room_members` is the security boundary) — so a hit can never surface a
//! room the caller isn't in, and there is no post-filter.
//!
//! Purely additive: a thin handler over [`AdvancedSearchRepo`]; no existing repo
//! or handler is touched. Mounted via [`routes`] and `.merge`d into the main
//! router, mirroring [`crate::search`].

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, WorkspaceId};
use aero_storage::{parse_search_query, AdvancedSearchRepo, SearchCursor};
use axum::{extract::State, routing::post, Json, Router};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// The advanced cross-room search route, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/search/advanced", post(search_advanced))
}

/// Default number of hits returned when the caller omits `limit` (mirrors
/// [`crate::search`]). The repo additionally clamps the value to `[1, 100]`.
const DEFAULT_LIMIT: i64 = 20;

/// The legacy / default workspace (the all-zero `WorkspaceId`) a single-tenant
/// caller searches when `workspace_id` is absent — mirrors
/// `crate::routes::resolve_workspace_id` / [`crate::search`].
const DEFAULT_WORKSPACE_ID: WorkspaceId = WorkspaceId(ulid::Ulid(0));

#[derive(Deserialize)]
struct AdvancedSearchReq {
    /// The raw query, free text plus optional `from:`/`in:`/`before:`/`after:`.
    query: String,
    /// Optional tenant scope; absent ⇒ [`DEFAULT_WORKSPACE_ID`].
    #[serde(default)]
    workspace_id: Option<String>,
    /// Optional page size; absent ⇒ [`DEFAULT_LIMIT`], clamped to `[1, 100]`.
    #[serde(default)]
    limit: Option<i64>,
    /// Optional keyset cursor from a previous response's `next_cursor`; absent ⇒
    /// first page. A malformed cursor is treated as absent (first page).
    #[serde(default)]
    cursor: Option<String>,
}

/// `POST /api/search/advanced` — full-text search across every room the caller
/// belongs to, with Slack-style operators AND-ed on. The repository's
/// `JOIN room_members` is the security boundary; no room the caller isn't in can
/// appear. Echoes the recognized operators back under `parsed`.
async fn search_advanced(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<AdvancedSearchReq>,
) -> ApiResult<Json<serde_json::Value>> {
    if req.query.trim().is_empty() {
        return Err(AeroError::Invalid("empty query".into()).into());
    }
    let workspace = match req.workspace_id.as_deref() {
        Some(raw) => WorkspaceId::from_str(raw.trim())
            .map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))?,
        None => DEFAULT_WORKSPACE_ID,
    };
    let limit = req.limit.unwrap_or(DEFAULT_LIMIT);
    let parsed = parse_search_query(&req.query);
    // A malformed cursor decodes to None — treat as the first page rather than error.
    let after = req.cursor.as_deref().and_then(SearchCursor::decode);

    let repo = AdvancedSearchRepo::new(s.pg.clone());
    let total = repo
        .count(auth.participant_id, workspace, &parsed)
        .await
        .map_err(AeroError::from)?;
    let (hits, next) = repo
        .search_page(auth.participant_id, workspace, &parsed, limit, after)
        .await
        .map_err(AeroError::from)?;

    // "Did you mean…" — only worth surfacing on the first page of an under-
    // performing single-token query (multi-word typo correction is noisy and
    // word_similarity works per-word). Best-effort: a suggestion lookup failure
    // never fails the search.
    let term = parsed.terms.trim();
    let suggestions = if after.is_none() && total < 3 && !term.is_empty() && !term.contains(' ') {
        repo.suggest_terms(auth.participant_id, workspace, term, 5)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    Ok(Json(serde_json::json!({
        "query": req.query,
        "parsed": {
            "from": parsed.from,
            "in": parsed.in_room,
            "before": parsed.before,
            "after": parsed.after,
        },
        "total_count": total,
        "next_cursor": next.map(|c| c.encode()),
        "suggestions": suggestions,
        "results": hits.into_iter().map(|h| {
            serde_json::json!({
                "score": h.score,
                "message": h.message,
            })
        }).collect::<Vec<_>>(),
    })))
}
