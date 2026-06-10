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
use aero_storage::TaskRepo;
use axum::{
    extract::{Path, Query, State},
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

/// Parse the AI extraction's markdown bullet list into individual task titles.
///
/// Pure (no I/O), so it is unit-tested offline. Accepts the markdown shapes the
/// extraction prompt produces: lines starting with `-`, `*`, `•`, or an ordered
/// `1.` / `1)` marker. The leading marker (and any leading checkbox `[ ]` / `[x]`)
/// is stripped, surrounding whitespace trimmed, blank lines and the exact "No
/// action items found." sentinel dropped, and each title clamped to
/// [`MAX_TASK_TITLE_LEN`] characters. Non-bullet prose lines are ignored so a
/// chatty model preamble does not become a task. Order is preserved.
#[must_use]
fn parse_action_items(markdown: &str) -> Vec<String> {
    let mut titles = Vec::new();
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
        titles.push(clamp_title(title));
    }
    titles
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

/// Clamp a requested retrieval window into `[1, MAX_K]`, defaulting to
/// [`DEFAULT_K`] when absent. Pure, so the cap/floor is unit-tested offline
/// (Postgres / AI backend absent in CI).
#[must_use]
fn action_item_k(requested: Option<usize>) -> usize {
    requested.unwrap_or(DEFAULT_K).clamp(1, MAX_K)
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
/// titles and CREATED as durable [`Task`](aero_storage::Task)s in this room (via the
/// existing [`TaskRepo`], status `open`, creator = caller); the response then also
/// carries `"task_ids": [<task id>, …]`. The non-persist path is byte-for-byte
/// unchanged. `502` when no AI backend is configured; degrades to the backend's
/// heuristic answer when a backend is wired but no LLM key is set (exactly like
/// `POST /api/ai/ask`).
async fn action_items(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Query(q): Query<ActionItemsQuery>,
    Json(req): Json<ActionItemsReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    // Tenant guard (workspace + room membership) before invoking the AI backend
    // over the room's messages — defense-in-depth on RAG output.
    s.im.assert_room_access(auth.participant_id, room).await?;

    let k = action_item_k(req.k);
    // Degrade exactly like `POST /api/ai/ask`: `502` when no AI backend is wired;
    // a wired backend falls back to its heuristic answer when no LLM key is set.
    let ai = s
        .ai
        .as_ref()
        .ok_or_else(|| AeroError::Upstream("AI not configured".into()))?;
    let answer = ai
        .answer_question(room, EXTRACT_PROMPT, k)
        .await
        .map_err(|e| AeroError::Upstream(format!("ai: {e}")))?;

    let mut body = serde_json::json!({
        "action_items": answer.answer,
        "citations": answer.citations,
    });

    if q.persist {
        // Parse the bullet list into individual titles and create a durable task
        // per title (status `open`, creator = caller, no assignee / due date).
        let tasks = TaskRepo::new(s.pg.clone());
        let mut task_ids = Vec::new();
        for title in parse_action_items(&answer.answer) {
            match tasks
                .create(room, auth.participant_id, &title, None, None, None)
                .await
            {
                Ok(id) => task_ids.push(id),
                Err(e) => {
                    // Best-effort: a single insert failure is logged and skipped so
                    // the rest of the items still persist.
                    tracing::warn!(error = ?e, %room, "action-items persist: task create failed");
                }
            }
        }
        body["task_ids"] = serde_json::to_value(task_ids).map_err(AeroError::from)?;
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
    fn parse_action_items_handles_each_bullet_shape() {
        let md = "\
- Ship the release
* Write the changelog
• Notify the team
1. Update docs
2) Close the ticket";
        let items = parse_action_items(md);
        assert_eq!(
            items,
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
        assert_eq!(items, vec!["Draft the proposal", "Review PR #42"]);
    }

    #[test]
    fn parse_action_items_drops_sentinel_and_blanks() {
        assert!(parse_action_items("No action items found.").is_empty());
        assert!(parse_action_items("- No action items found").is_empty());
        assert!(parse_action_items("\n\n   \n").is_empty());
        assert!(parse_action_items("").is_empty());
    }

    #[test]
    fn parse_action_items_ignores_mid_word_hyphen() {
        // A non-bullet prose line with a hyphen is not a task.
        assert!(parse_action_items("re-review the design later").is_empty());
        // A real bullet survives.
        assert_eq!(parse_action_items("- re-review the design"), vec!["re-review the design"]);
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
}
