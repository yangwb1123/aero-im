//! "Remind me about this message" — message-anchored reminders.
//!
//! Slack's "Remind me about this" on a message: the caller picks a relative time
//! and, at that instant, a reminder note (a card linking back to the message plus
//! a short snippet) is delivered into the message's room. This adds NO table, NO
//! new id and NO storage change — it composes existing pieces:
//!
//! * resolve + authorize the message exactly like [`crate::translate`]
//!   (`messages.get` → 404, then [`assert_room_access`] → 403),
//! * parse the relative delay with the same parser `/remind` uses
//!   ([`crate::commands::parse_reminder_delay`] — `10m`/`2h`/`30s`/`1d`, optional
//!   leading `in`; it already caps at one year and rejects garbage → 400),
//! * stage a self-authored reminder note in the room through the durable
//!   [`ScheduledRepo`](aero_storage::ScheduledRepo), so the existing
//!   [`run_scheduled_dispatcher`](crate::scheduled::run_scheduled_dispatcher)
//!   replays it through [`ImService::send_message`] at the due time — the reminder
//!   lands as an ordinary room message, like every scheduled message and `/remind`.
//!
//! Mounted via [`routes`] and `.merge`d into the gateway router, mirroring
//! [`crate::translate`].
//!
//! [`assert_room_access`]: aero_im_core::ImService::assert_room_access
//! [`ImService::send_message`]: aero_im_core::ImService::send_message

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Block, Error as AeroError, MessageId};
use aero_storage::ScheduledRepo;
use axum::{
    extract::{Path, State},
    routing::post,
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// Max length of the message-text snippet embedded in a reminder, in characters
/// (truncated on a char boundary so multibyte text never panics).
const SNIPPET_CHARS: usize = 140;

/// Mount the message-reminder routes, folded into the gateway router by
/// [`crate::routes::build`].
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/messages/:id/remind", post(remind_about_message))
        .route("/api/messages/:id/reminders", axum::routing::get(list_reminders))
}

/// First `SNIPPET_CHARS` characters of `text`, truncated on a char boundary so
/// multibyte input never panics. Trailing whitespace from the cut is trimmed.
#[must_use]
fn snippet(text: &str) -> String {
    let trimmed = text.trim();
    match trimmed.char_indices().nth(SNIPPET_CHARS) {
        Some((byte_idx, _)) => trimmed[..byte_idx].trim_end().to_string(),
        None => trimmed.to_string(),
    }
}

#[derive(Deserialize)]
struct RemindReq {
    /// Relative delay (`10m`, `2h`, `30s`, `1d`, optional leading `in`). Accepts
    /// either `in` (the Slack-y field name) or `when`; `in` wins if both are sent.
    #[serde(rename = "in", default)]
    in_: Option<String>,
    #[serde(default)]
    when: Option<String>,
}

/// `POST /api/messages/:id/remind` — set a reminder about a specific message.
///
/// Resolves the message (404 if unknown), authorizes the caller against the
/// message's room (`assert_room_access` → 403 for a non-member / cross-tenant),
/// parses the relative delay via [`crate::commands::parse_reminder_delay`] (400 if
/// unparseable or too far out), and stages a self-authored reminder note in the
/// room via [`ScheduledRepo`](aero_storage::ScheduledRepo). The note is a
/// `message_reminder` card (linking back to the message + a snippet) followed by a
/// human-readable line. Returns the new reminder id and the resolved delivery time.
async fn remind_about_message(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<RemindReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let mid =
        MessageId::from_str(&id_str).map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;

    let msg = s
        .messages
        .get(mid)
        .await?
        .ok_or_else(|| AeroError::NotFound(format!("message {mid}")))?;
    // Tenant + room-membership guard before exposing the message's content.
    s.im.assert_room_access(auth.participant_id, msg.room_id).await?;

    let when_raw = req
        .in_
        .as_deref()
        .or(req.when.as_deref())
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .ok_or_else(|| AeroError::Invalid("missing reminder time ('in' or 'when')".into()))?;

    // Same parser `/remind` uses: caps at one year, rejects garbage → 400.
    let delay = crate::commands::parse_reminder_delay(when_raw).ok_or_else(|| {
        AeroError::Invalid(format!(
            "couldn't read the time '{when_raw}'; try 10m, 2h, 30s, 1d"
        ))
    })?;
    let deliver_at = time::OffsetDateTime::now_utc()
        + time::Duration::seconds(i64::try_from(delay.as_secs()).unwrap_or(i64::MAX));

    let snip = snippet(&msg.searchable_text());
    let blocks = vec![
        Block::Card {
            schema: "message_reminder".into(),
            payload: serde_json::json!({
                "message_id": mid,
                "snippet": snip,
                "by": auth.participant_id,
            }),
        },
        Block::text(format!("⏰ Reminder about a message: {snip}")),
    ];

    let reminder_id = ScheduledRepo::new(s.pg.clone())
        .create(msg.room_id, auth.participant_id, &blocks, None, deliver_at)
        .await?;

    Ok(Json(serde_json::json!({
        "scheduled": true,
        "reminder_id": reminder_id,
        "deliver_at": deliver_at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
        "message_id": mid,
    })))
}

/// `GET /api/messages/:id/reminders` — the caller's own still-pending reminders in
/// the message's room (soonest first). Resolves + authorizes the message exactly
/// like [`remind_about_message`], then reuses
/// [`ScheduledRepo::list_pending_for_sender`] scoped to that room. Sender-scoped, so
/// one user never sees another's reminders; the listing is room-wide (it includes
/// reminders anchored to other messages in the same room, plus plain `/remind`s and
/// "send later" drafts — there is no per-message filter in the shared scheduler).
async fn list_reminders(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let mid =
        MessageId::from_str(&id_str).map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    let msg = s
        .messages
        .get(mid)
        .await?
        .ok_or_else(|| AeroError::NotFound(format!("message {mid}")))?;
    s.im.assert_room_access(auth.participant_id, msg.room_id).await?;

    let pending = ScheduledRepo::new(s.pg.clone())
        .list_pending_for_sender(auth.participant_id, Some(msg.room_id))
        .await?;
    Ok(Json(serde_json::to_value(pending).map_err(AeroError::from)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_keeps_short_text_verbatim() {
        assert_eq!(snippet("hello world"), "hello world");
        // Surrounding whitespace is trimmed.
        assert_eq!(snippet("  hi  "), "hi");
    }

    #[test]
    fn snippet_empty_is_empty() {
        assert_eq!(snippet(""), "");
        assert_eq!(snippet("    "), "");
    }

    #[test]
    fn snippet_truncates_long_ascii_to_cap() {
        let long = "a".repeat(SNIPPET_CHARS + 50);
        let s = snippet(&long);
        assert_eq!(s.chars().count(), SNIPPET_CHARS);
        assert!(long.starts_with(&s));
    }

    #[test]
    fn snippet_truncates_on_char_boundary_for_multibyte() {
        // Each `é` is 2 bytes and each emoji is 4 bytes; a naive byte slice at
        // SNIPPET_CHARS would split a char and panic. We must cut on a boundary.
        let multibyte = "é".repeat(SNIPPET_CHARS + 10);
        let s = snippet(&multibyte);
        assert_eq!(s.chars().count(), SNIPPET_CHARS);
        // Round-trips as valid UTF-8 (no split char), and is a real prefix.
        assert!(multibyte.starts_with(&s));

        let emoji = "🎉".repeat(SNIPPET_CHARS + 10);
        let s = snippet(&emoji);
        assert_eq!(s.chars().count(), SNIPPET_CHARS);
        assert!(emoji.starts_with(&s));
    }

    #[test]
    fn snippet_at_exact_cap_is_unchanged() {
        let exact = "x".repeat(SNIPPET_CHARS);
        assert_eq!(snippet(&exact), exact);
    }
}
