//! Advanced cross-room search — Slack-style query operators.
//!
//! The structured counterpart to [`crate::search`]: the caller's query may carry
//! `from:@<id>`, `in:<roomid>`, `before:<msgid>`, `after:<msgid>`,
//! `since:<timestamp>`, and `until:<timestamp>` operators alongside free text.
//! The operators are parsed out
//! ([`parse_search_query`](aero_storage::parse_search_query)) and AND-ed into the
//! SAME effective-access-scoped cross-room search as [`crate::search`]
//! ([`AdvancedSearchRepo`](aero_storage::AdvancedSearchRepo), whose effective
//! room/workspace/account/deactivation/2FA joins are the security boundary) —
//! so a stale room edge can never surface content.
//!
//! Purely additive: a thin handler over [`AdvancedSearchRepo`]; no existing repo
//! or handler is touched. Mounted via [`routes`] and `.merge`d into the main
//! router, mirroring [`crate::search`].

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, MessageId, WorkspaceId};
use aero_storage::search_feedback::normalize_search_query;
use aero_storage::{parse_search_query, AdvancedSearchRepo, SearchCursor, SearchFeedbackRepo};
use axum::{extract::State, routing::post, Json, Router};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// The advanced cross-room search routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/search/advanced", post(search_advanced))
        .route("/api/search/click", post(search_click))
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
    /// The raw query, free text plus optional structured operators.
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
    /// When true, also compute a faceted breakdown (top rooms + senders by hit
    /// count) under `facets`. Opt-in: it runs two extra GROUP BY queries, so a
    /// plain paged search doesn't pay for it. Default false.
    #[serde(default)]
    facets: bool,
}

/// How many buckets each facet dimension returns.
const FACET_TOP: i64 = 10;

/// `POST /api/search/advanced` — full-text search across every room the caller
/// may currently access, with Slack-style operators AND-ed on. The repository's
/// effective-access joins are the security boundary. Echoes the recognized
/// operators back under `parsed`.
async fn search_advanced(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<AdvancedSearchReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let normalized_query = normalize_search_query(&req.query)?;
    let workspace = match req.workspace_id.as_deref() {
        Some(raw) => WorkspaceId::from_str(raw.trim())
            .map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))?,
        None => DEFAULT_WORKSPACE_ID,
    };
    let limit = req.limit.unwrap_or(DEFAULT_LIMIT);
    let parsed = parse_search_query(&normalized_query);
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

    // Faceted drill-down (opt-in, first page only — facet counts are over the
    // whole result set, so they don't change page to page).
    let facets = if req.facets && after.is_none() {
        Some(
            repo.facets(auth.participant_id, workspace, &parsed, FACET_TOP)
                .await
                .map_err(AeroError::from)?,
        )
    } else {
        None
    };

    let result_ids = hits.iter().map(|hit| hit.message.id).collect::<Vec<_>>();
    let impression = SearchFeedbackRepo::new(s.pg.clone())
        // Persist the complete normalized request, not only its free-text terms.
        // Structured operators materially change which ranked set was shown and
        // therefore belong to the feedback proof.
        .create_impression(
            auth.participant_id,
            workspace,
            &normalized_query,
            &result_ids,
        )
        .await?;

    Ok(Json(serde_json::json!({
        "query": normalized_query,
        "impression_id": impression.id,
        "parsed": {
            "from": parsed.from,
            "in": parsed.in_room,
            "before": parsed.before,
            "after": parsed.after,
        },
        "total_count": total,
        "next_cursor": next.map(|c| c.encode()),
        "suggestions": suggestions,
        "facets": facets,
        "results": hits.into_iter().map(|h| {
            serde_json::json!({
                "score": h.score,
                "headline": h.headline,
                "message": h.message,
            })
        }).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchClickReq {
    /// Server-issued proof returned by `/api/search/advanced`.
    impression_id: String,
    /// The message id of the clicked result.
    result_id: String,
}

/// `POST /api/search/click` — record a search-result click-through, the data
/// foundation for relevance analytics / learning-to-rank (ROADMAP5 方向三 P2).
/// The client submits only the server-issued impression and chosen result. The
/// repository locks and consumes that proof, derives normalized query/rank from
/// its ordered snapshot, and rechecks current access/canonical workspace before
/// commit.
///
/// Repeating the same click is deterministic and does not append a second
/// analytics event. A different second result, expired proof, wrong owner,
/// non-snapshot result, or revoked access is rejected.
async fn search_click(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<SearchClickReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let impression_id = uuid::Uuid::parse_str(req.impression_id.trim())
        .map_err(|e| AeroError::Invalid(format!("impression_id: {e}")))?;
    let result_id = MessageId::from_str(req.result_id.trim())
        .map_err(|e| AeroError::Invalid(format!("result_id: {e}")))?;

    let receipt = SearchFeedbackRepo::new(s.pg.clone())
        .record_impression_click(auth.participant_id, impression_id, result_id)
        .await?;

    Ok(Json(serde_json::json!({
        "recorded": true,
        "impression_id": receipt.impression_id,
        "result_id": receipt.result_id,
        "rank": receipt.result_rank,
    })))
}

#[cfg(test)]
mod tests {
    use super::SearchClickReq;

    #[test]
    fn click_request_accepts_only_server_proof_and_result() {
        let impression = uuid::Uuid::new_v4();
        let result = aero_common::MessageId::new();
        let parsed: SearchClickReq = serde_json::from_value(serde_json::json!({
            "impression_id": impression,
            "result_id": result,
        }))
        .unwrap();
        assert_eq!(parsed.impression_id, impression.to_string());
        assert_eq!(parsed.result_id, result.to_string());

        assert!(
            serde_json::from_value::<SearchClickReq>(serde_json::json!({
                "impression_id": impression,
                "result_id": result,
                "query": "forged",
                "rank": 99,
            }))
            .is_err(),
            "legacy client-supplied relevance attributes are rejected"
        );
    }
}
