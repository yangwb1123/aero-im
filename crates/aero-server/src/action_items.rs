//! AI action-item extraction — pull the tasks / to-dos out of a channel.
//!
//! An AI-native convenience over the existing RAG path: instead of asking a free
//! form question (`POST /api/ai/ask`), the caller asks the server to mine a
//! channel for everything actionable — action items, tasks, decisions, and
//! to-dos — and gets back a concise markdown bullet list with the responsible
//! person where stated. It reuses the SAME retrieval-augmented
//! [`AiBackend::answer_question`](crate::state::AiBackend) call `POST /api/ai/ask`
//! uses (a fixed extraction prompt over the room's top-`k` relevant messages), so
//! there is no `AiBackend` trait change and the returned `citations` point back at
//! the source messages.
//!
//! Degrades exactly like `POST /api/ai/ask`: `502` when no AI backend is wired
//! ([`AppState::ai`] is `None`), and when a backend *is* wired but no LLM key is
//! configured it falls back to the backend's heuristic answer. Purely additive: a
//! thin handler over existing [`AppState`] state; no existing repo or service is
//! touched. Mounted via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, RoomId};
use aero_storage::task::{
    ActionItemBatchError, MAX_ACTION_ITEM_BATCH_KEY_LEN, MAX_ACTION_ITEM_BATCH_SIZE,
};
use aero_storage::TaskRepo;
use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    routing::post,
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All action-item routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/rooms/:id/action-items", post(action_items))
}

/// Max length of a persisted task title; longer parsed titles are truncated to
/// this (the [`TaskRepo`] column is `text` but we keep titles sane). Matches the
/// task module's `MAX_TITLE_LEN`.
const MAX_TASK_TITLE_LEN: usize = 512;

/// A bounded parse of the model's markdown list.
#[derive(Debug, PartialEq, Eq)]
struct ParsedActionItems {
    titles: Vec<String>,
    truncated: bool,
}

/// Parse the AI extraction's markdown bullet list into individual task titles.
///
/// Pure (no I/O), so it is unit-tested offline. Accepts the markdown shapes the
/// extraction prompt produces: lines starting with `-`, `*`, `•`, or an ordered
/// `1.` / `1)` marker. The leading marker (and any leading checkbox `[ ]` / `[x]`)
/// is stripped, surrounding whitespace trimmed, blank lines and the exact "No
/// action items found." sentinel dropped, and each title clamped to
/// [`MAX_TASK_TITLE_LEN`] characters. Non-bullet prose lines are ignored so a
/// chatty model preamble does not become a task. Order is preserved, and no more
/// than [`MAX_ACTION_ITEM_BATCH_SIZE`] valid items are retained.
#[must_use]
fn parse_action_items(markdown: &str) -> ParsedActionItems {
    let mut titles = Vec::new();
    let mut truncated = false;
    for raw in markdown.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let Some(body) = strip_bullet_marker(line) else {
            continue; // not a bullet — ignore prose / headings.
        };
        let title = strip_checkbox(body).trim();
        if title.is_empty() {
            continue;
        }
        // Drop the "nothing actionable" sentinel however it is bulleted/cased.
        if title.eq_ignore_ascii_case("no action items found")
            || title.eq_ignore_ascii_case("no action items found.")
        {
            continue;
        }
        if titles.len() == MAX_ACTION_ITEM_BATCH_SIZE {
            truncated = true;
            break;
        }
        titles.push(clamp_title(title));
    }
    ParsedActionItems { titles, truncated }
}

/// Strip a leading list marker (`-`, `*`, `•`, or an ordered `N.` / `N)`),
/// returning the remaining body, or `None` when the line is not a bullet. The
/// marker must be followed by whitespace so a stray hyphen mid-word is not treated
/// as a bullet.
fn strip_bullet_marker(line: &str) -> Option<&str> {
    for marker in ['-', '*', '•'] {
        if let Some(rest) = line.strip_prefix(marker) {
            if rest.starts_with(char::is_whitespace) || rest.is_empty() {
                return Some(rest.trim_start());
            }
        }
    }
    // Ordered list: leading digits then `.` or `)`.
    let digits: String = line.chars().take_while(char::is_ascii_digit).collect();
    if !digits.is_empty() {
        let after = &line[digits.len()..];
        if let Some(rest) = after.strip_prefix('.').or_else(|| after.strip_prefix(')')) {
            if rest.starts_with(char::is_whitespace) || rest.is_empty() {
                return Some(rest.trim_start());
            }
        }
    }
    None
}

/// Strip a leading markdown checkbox (`[ ]`, `[x]`, `[X]`) from a bullet body.
fn strip_checkbox(body: &str) -> &str {
    for token in ["[ ]", "[x]", "[X]", "[]"] {
        if let Some(rest) = body.strip_prefix(token) {
            return rest.trim_start();
        }
    }
    body
}

/// Clamp a title to [`MAX_TASK_TITLE_LEN`] characters (char-safe, with an ellipsis).
fn clamp_title(s: &str) -> String {
    if s.chars().count() <= MAX_TASK_TITLE_LEN {
        return s.to_string();
    }
    let cut: String = s.chars().take(MAX_TASK_TITLE_LEN - 1).collect();
    format!("{cut}…")
}

/// Default number of relevant messages retrieved when the caller omits `k`.
const DEFAULT_K: usize = 20;
/// Hard ceiling on the retrieval window, mirroring the search/RAG page caps.
const MAX_K: usize = 50;

/// The fixed extraction prompt fed to the RAG backend. Phrased so the model
/// returns a self-contained markdown bullet list (or an exact sentinel when the
/// channel holds nothing actionable).
const EXTRACT_PROMPT: &str = "Extract every action item, task, decision, and to-do mentioned in this channel as a concise markdown bullet list, each with the responsible person if stated. If there are none, reply exactly: No action items found.";

fn action_item_usage_fingerprint(room: RoomId, k: usize, persist: bool) -> String {
    if persist {
        // The persistence key identifies the whole durable operation. A retry
        // that changes `k` must replay the first provider result rather than
        // paying for a second answer which the batch receipt would discard.
        format!("action_items:persist:{room}")
    } else {
        format!("action_items:{room}:{k}")
    }
}

/// Clamp a requested retrieval window into `[1, MAX_K]`, defaulting to
/// [`DEFAULT_K`] when absent. Pure, so the cap/floor is unit-tested offline
/// (Postgres / AI backend absent in CI).
#[must_use]
fn action_item_k(requested: Option<usize>) -> usize {
    requested.unwrap_or(DEFAULT_K).clamp(1, MAX_K)
}

/// Require and normalize the persistence idempotency key without changing the
/// legacy extraction-only request contract.
fn persistence_idempotency_key(
    headers: &HeaderMap,
    persist: bool,
) -> Result<Option<&str>, AeroError> {
    if !persist {
        return Ok(None);
    }
    let raw = headers
        .get("idempotency-key")
        .ok_or_else(|| {
            AeroError::Invalid("Idempotency-Key header is required when persist=true".into())
        })?
        .to_str()
        .map_err(|_| AeroError::Invalid("Idempotency-Key must be valid ASCII".into()))?;
    let key = raw.trim();
    if key.is_empty() {
        return Err(AeroError::Invalid(
            "Idempotency-Key must not be empty".into(),
        ));
    }
    if key.len() > MAX_ACTION_ITEM_BATCH_KEY_LEN {
        return Err(AeroError::Invalid(format!(
            "Idempotency-Key must be at most {MAX_ACTION_ITEM_BATCH_KEY_LEN} bytes"
        )));
    }
    Ok(Some(key))
}

fn map_batch_error(error: ActionItemBatchError) -> AeroError {
    match error {
        ActionItemBatchError::ActorNotMember => {
            AeroError::Forbidden("action-item room access was revoked".into())
        }
        ActionItemBatchError::InvalidIdempotencyKey => {
            AeroError::Invalid("invalid Idempotency-Key".into())
        }
        ActionItemBatchError::TooManyItems => AeroError::Invalid(format!(
            "at most {MAX_ACTION_ITEM_BATCH_SIZE} action items may be persisted"
        )),
        ActionItemBatchError::Database(error) => AeroError::from(error),
    }
}

fn map_action_item_ai_error(error: &str) -> AeroError {
    if error.contains("provider reservation is already active") {
        return AeroError::Conflict(
            "an action-item request with this Idempotency-Key is still in progress; retry it"
                .into(),
        );
    }
    AeroError::Upstream(format!("ai: {error}"))
}

/// Parse a `RoomId` from a path segment, mapping a decode failure to `400`.
fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

#[derive(Deserialize)]
struct ActionItemsReq {
    /// How many relevant messages to retrieve for the extraction. Absent ⇒
    /// [`DEFAULT_K`]; clamped into `[1, MAX_K]`.
    #[serde(default)]
    k: Option<usize>,
}

#[derive(Deserialize)]
struct ActionItemsQuery {
    /// When `true`, the extracted bullet list is ALSO parsed into individual
    /// durable [`Task`](aero_storage::Task)s (created in this room via the existing
    /// [`TaskRepo`]), and their ids are returned. Absent/`false` ⇒ the legacy
    /// markdown-only response, unchanged.
    #[serde(default)]
    persist: bool,
}

/// `POST /api/rooms/:id/action-items[?persist=true]` — extract the channel's action
/// items, tasks, decisions, and to-dos as a markdown bullet list. Requires room
/// access. Returns `{ "action_items": <markdown>, "citations": [<message id>, …] }`.
///
/// When `persist=true`, the markdown is additionally parsed into individual task
/// titles and CREATED as one atomic durable-task batch. It requires a non-empty
/// `Idempotency-Key` of at most 128 bytes; retries in the same caller/room scope
/// return the original task ids. The response also carries `"task_ids"` and
/// `"truncated"`. The non-persist path is unchanged. `502` when no AI backend is
/// configured; degrades to the backend's heuristic answer when a backend is
/// wired but no LLM key is set (exactly like `POST /api/ai/ask`).
async fn action_items(
    State(s): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    Path(room_str): Path<String>,
    Query(q): Query<ActionItemsQuery>,
    Json(req): Json<ActionItemsReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    // Tenant guard (workspace + room membership) before invoking the AI backend
    // over the room's messages — defense-in-depth on RAG output.
    s.im.assert_room_access(auth.participant_id, room).await?;
    let persistence_key = persistence_idempotency_key(&headers, q.persist)?;

    let k = action_item_k(req.k);
    // Degrade exactly like `POST /api/ai/ask`: `502` when no AI backend is wired;
    // a wired backend falls back to its heuristic answer when no LLM key is set.
    let ai =
        s.ai.as_ref()
            .ok_or_else(|| AeroError::Upstream("AI not configured".into()))?;
    let usage_context = crate::ai_usage::room_request_usage_context(
        &s,
        &headers,
        auth.participant_id,
        room,
        &action_item_usage_fingerprint(room, k, q.persist),
    )
    .await?;
    let answer = ai
        .answer_question_with_usage_context(room, EXTRACT_PROMPT, k, usage_context)
        .await
        .map_err(|e| map_action_item_ai_error(&e))?;

    let mut body = serde_json::json!({
        "action_items": answer.answer,
        "citations": answer.citations,
    });

    if q.persist {
        let parsed = parse_action_items(&answer.answer);
        let task_ids = TaskRepo::new(s.pg.clone())
            .create_action_item_batch(
                room,
                auth.participant_id,
                persistence_key.expect("persist=true requires a validated key"),
                &parsed.titles,
            )
            .await
            .map_err(map_batch_error)?;
        body["task_ids"] = serde_json::to_value(task_ids).map_err(AeroError::from)?;
        body["truncated"] = serde_json::Value::Bool(parsed.truncated);
    }

    Ok(Json(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure window-sizing logic: an absent `k` defaults to [`DEFAULT_K`]; a present
    /// value passes through until it hits the cap, then saturates at [`MAX_K`], and
    /// a zero floors to `1`.
    #[test]
    fn action_item_k_clamps_into_bounds() {
        assert_eq!(action_item_k(None), DEFAULT_K);
        assert_eq!(action_item_k(Some(0)), 1);
        assert_eq!(action_item_k(Some(1)), 1);
        assert_eq!(action_item_k(Some(20)), 20);
        assert_eq!(action_item_k(Some(50)), MAX_K);
        assert_eq!(action_item_k(Some(10_000)), MAX_K);
    }

    #[test]
    fn persisted_usage_fingerprint_is_stable_across_retry_input_changes() {
        let room = RoomId::new();
        assert_eq!(
            action_item_usage_fingerprint(room, 1, true),
            action_item_usage_fingerprint(room, MAX_K, true)
        );
        assert_ne!(
            action_item_usage_fingerprint(room, 1, false),
            action_item_usage_fingerprint(room, MAX_K, false)
        );
        assert_ne!(
            action_item_usage_fingerprint(room, 1, true),
            action_item_usage_fingerprint(RoomId::new(), 1, true)
        );
    }

    #[test]
    fn persisted_retry_stabilizes_both_paid_operations_across_k_changes() {
        let room = RoomId::new();
        let actor = aero_common::ParticipantId::new();
        let workspace = Some(uuid::Uuid::new_v4());
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "same-batch".parse().unwrap());

        let first = crate::ai_usage::request_usage_context(
            &headers,
            actor,
            workspace,
            &action_item_usage_fingerprint(room, 1, true),
        );
        let changed_k = crate::ai_usage::request_usage_context(
            &headers,
            actor,
            workspace,
            &action_item_usage_fingerprint(room, MAX_K, true),
        );
        for operation in ["voyage_answer_query", "anthropic_answer"] {
            assert_eq!(
                first.operation_id(operation),
                changed_k.operation_id(operation),
                "same persisted key must reuse the {operation} reservation"
            );
        }

        headers.insert("idempotency-key", "different-batch".parse().unwrap());
        let different_key = crate::ai_usage::request_usage_context(
            &headers,
            actor,
            workspace,
            &action_item_usage_fingerprint(room, 1, true),
        );
        assert_ne!(first.root_id, different_key.root_id);

        headers.insert("idempotency-key", "same-batch".parse().unwrap());
        let non_persist_changed_k = crate::ai_usage::request_usage_context(
            &headers,
            actor,
            workspace,
            &action_item_usage_fingerprint(room, MAX_K, false),
        );
        assert_ne!(first.root_id, non_persist_changed_k.root_id);
    }

    #[test]
    fn concurrent_paid_reservation_maps_to_retryable_idempotency_conflict() {
        let error = map_action_item_ai_error(
            "storage: AI usage accounting: provider reservation is already active \
             (usage_id=00000000-0000-0000-0000-000000000000, kind=answer)",
        );
        assert!(matches!(error, AeroError::Conflict(message) if message.contains("retry")));
        assert!(matches!(
            map_action_item_ai_error("provider timed out"),
            AeroError::Upstream(_)
        ));
    }

    #[test]
    fn parse_action_items_handles_each_bullet_shape() {
        let md = "\
- Ship the release
* Write the changelog
• Notify the team
1. Update docs
2) Close the ticket";
        let items = parse_action_items(md);
        assert_eq!(
            items.titles,
            vec![
                "Ship the release",
                "Write the changelog",
                "Notify the team",
                "Update docs",
                "Close the ticket",
            ]
        );
    }

    #[test]
    fn parse_action_items_strips_checkboxes_and_skips_prose() {
        let md = "\
Here are the action items:

- [ ] Draft the proposal
- [x] Review PR #42

That's all.";
        let items = parse_action_items(md);
        // Prose preamble + trailing line ignored; checkboxes stripped.
        assert_eq!(items.titles, vec!["Draft the proposal", "Review PR #42"]);
    }

    #[test]
    fn parse_action_items_drops_sentinel_and_blanks() {
        assert!(parse_action_items("No action items found.")
            .titles
            .is_empty());
        assert!(parse_action_items("- No action items found")
            .titles
            .is_empty());
        assert!(parse_action_items("\n\n   \n").titles.is_empty());
        assert!(parse_action_items("").titles.is_empty());
    }

    #[test]
    fn parse_action_items_ignores_mid_word_hyphen() {
        // A non-bullet prose line with a hyphen is not a task.
        assert!(parse_action_items("re-review the design later")
            .titles
            .is_empty());
        // A real bullet survives.
        assert_eq!(
            parse_action_items("- re-review the design").titles,
            vec!["re-review the design"]
        );
    }

    #[test]
    fn clamp_title_truncates_long_titles() {
        let long = "汉".repeat(MAX_TASK_TITLE_LEN + 50);
        let clamped = clamp_title(&long);
        assert_eq!(clamped.chars().count(), MAX_TASK_TITLE_LEN);
        assert!(clamped.ends_with('…'));
        // A short title is returned unchanged.
        assert_eq!(clamp_title("short"), "short");
    }

    #[test]
    fn parse_action_items_caps_model_output_and_marks_truncation() {
        let markdown = (0..(MAX_ACTION_ITEM_BATCH_SIZE + 7))
            .map(|index| format!("- Task {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let parsed = parse_action_items(&markdown);
        assert_eq!(parsed.titles.len(), MAX_ACTION_ITEM_BATCH_SIZE);
        assert!(parsed.truncated);

        let exact = (0..MAX_ACTION_ITEM_BATCH_SIZE)
            .map(|index| format!("- Exact {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!parse_action_items(&exact).truncated);
    }

    #[test]
    fn persistence_requires_a_bounded_non_empty_idempotency_key() {
        let empty = HeaderMap::new();
        assert!(persistence_idempotency_key(&empty, true).is_err());
        assert_eq!(persistence_idempotency_key(&empty, false).unwrap(), None);

        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", " batch-42 ".parse().unwrap());
        assert_eq!(
            persistence_idempotency_key(&headers, true).unwrap(),
            Some("batch-42")
        );

        let too_long = "x".repeat(MAX_ACTION_ITEM_BATCH_KEY_LEN + 1);
        headers.insert("idempotency-key", too_long.parse().unwrap());
        assert!(persistence_idempotency_key(&headers, true).is_err());
        // Extraction-only requests deliberately ignore even malformed/oversized
        // persistence headers.
        assert_eq!(persistence_idempotency_key(&headers, false).unwrap(), None);
    }
}
