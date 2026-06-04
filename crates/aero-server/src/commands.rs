//! Slash-commands framework: `/me`, `/shrug`, `/giphy`, `/remind` (built-ins).
//!
//! A message whose text begins with `/` is a *command*, not chat content. To keep
//! the hot send path (`ImService::send_message`) untouched, the client posts such
//! input to a dedicated endpoint here when the composer's first char is `/`; the
//! command is parsed, an effect is chosen, and — for the common case — a normal
//! message is sent back into the room through the SAME service call every other
//! message uses (so membership, persistence, broadcast and mention notifications
//! all hold). Non-`/` text never reaches this module.
//!
//! ## Pure core, thin shell
//!
//! The whole parse → dispatch decision is DB-free and exhaustively unit-tested:
//!
//! * [`parse_command`] splits `/name rest` (or rejects non-commands),
//! * [`render_command`] maps `(name, rest, author)` to a [`CommandOutcome`],
//! * [`builtin_help`] / [`builtin_command_names`] drive the client autocomplete.
//!
//! The two async handlers ([`run_command`], [`list_commands`]) are then a shell:
//! resolve the room + caller display name, call the pure function, and apply the
//! outcome.
//!
//! ## `/remind` is wired to the durable scheduler
//!
//! `/remind <when> <text>` parses the relative time ([`parse_reminder_delay`] —
//! `10m`, `2h`, `30s`, `1d`, with an optional leading `in`), computes an absolute
//! delivery instant, and persists a self-authored reminder note into the room via
//! [`ScheduledRepo`](aero_storage::ScheduledRepo). The existing
//! [`run_scheduled_dispatcher`](crate::scheduled::run_scheduled_dispatcher) then
//! delivers it at the due time through the same [`ImService::send_message`] path
//! every message takes — so the reminder lands as a normal room message. The pure
//! layer is unchanged: [`render_command`] still yields a [`CommandOutcome::Remind`]
//! `{ when_raw, text }`; only the async handler arm resolves it against the clock
//! and hands it to the scheduler.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Block, Error as AeroError, ParticipantId, RoomId};
use aero_storage::ScheduledRepo;
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

// ---------------------------------------------------------------- Router

/// All slash-command routes, ready to `.merge` into the gateway router (mirrors
/// [`crate::workspaces`]).
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/commands", get(list_commands))
        .route("/api/rooms/:id/command", post(run_command))
}

// ------------------------------------------------------ Pure parse + dispatch

/// The effect a parsed slash-command resolves to. Pure data: the async handler
/// decides how to apply each arm (post a message, "schedule" a reminder, reply
/// ephemerally, or surface an error).
///
/// Intentionally not `PartialEq`: it carries `Vec<Block>`, and `Block` (in
/// `aero-common`) does not implement `PartialEq` — tests pattern-match the
/// outcome and compare the projected `content` / `payload` instead.
#[derive(Debug, Clone)]
pub enum CommandOutcome {
    /// Post these blocks to the room as a normal message (the common case).
    Post(Vec<Block>),
    /// A reminder request: deliver `text` at the relative time named by `when_raw`
    /// (e.g. `10m`). The async handler resolves `when_raw` against the clock via
    /// [`parse_reminder_delay`] and persists it through
    /// [`ScheduledRepo`](aero_storage::ScheduledRepo) for later delivery.
    Remind { when_raw: String, text: String },
    /// Reply only to the caller, without posting to the room.
    Ephemeral(String),
    /// The command was malformed or unknown; surfaces as `400`.
    Error(String),
}

/// Split a typed message into `(command_name, rest)` when it is a slash-command.
///
/// `/me hugs the bot` → `Some(("me", "hugs the bot"))`. The leading `/` is
/// required and stripped; the name is the first whitespace-delimited token
/// (lower-cased so `/ME` == `/me`); `rest` is everything after the first run of
/// whitespace, trimmed. Returns `None` for input that is not a command: empty, not
/// starting with `/`, a bare `/`, or only-slash-then-spaces (`/   `). A `//`
/// escapes to a literal message and is therefore NOT a command either.
#[must_use]
pub fn parse_command(input: &str) -> Option<(String, String)> {
    let s = input.trim_start();
    let body = s.strip_prefix('/')?;
    // `//...` is a deliberately-escaped literal slash, not a command.
    if body.starts_with('/') {
        return None;
    }
    let mut parts = body.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or("");
    if name.is_empty() {
        return None;
    }
    let rest = parts.next().unwrap_or("").trim().to_string();
    Some((name.to_ascii_lowercase(), rest))
}

/// Map a parsed command to its [`CommandOutcome`]. Pure and total: every known
/// built-in is handled and anything else is [`CommandOutcome::Error`].
/// `author_display` is the caller's display name, woven into emote-style output.
#[must_use]
pub fn render_command(name: &str, rest: &str, author_display: &str) -> CommandOutcome {
    let rest = rest.trim();
    match name {
        // Emote / action line: "* Alice hugs the bot". Requires some text.
        "me" => {
            if rest.is_empty() {
                CommandOutcome::Error("usage: /me <action>".into())
            } else {
                CommandOutcome::Post(vec![Block::text(format!("* {author_display} {rest}"))])
            }
        }
        // Shrug, with optional leading text. Trim so a bare `/shrug` has no space.
        "shrug" => {
            let line = format!("{rest} ¯\\_(ツ)_/¯");
            CommandOutcome::Post(vec![Block::text(line.trim().to_string())])
        }
        // Stub GIF card — a real GIPHY fetch is a documented seam (the client, or a
        // later server enrichment, swaps the stub for a chosen GIF url).
        "giphy" => {
            if rest.is_empty() {
                CommandOutcome::Error("usage: /giphy <query>".into())
            } else {
                CommandOutcome::Post(vec![Block::Card {
                    schema: "giphy".into(),
                    payload: serde_json::json!({ "query": rest }),
                }])
            }
        }
        // Reminder: split off the leading "<when>" token; the remainder is the note.
        "remind" => {
            let (when_raw, text) = split_remind(rest);
            if when_raw.is_empty() || text.is_empty() {
                CommandOutcome::Error(
                    "usage: /remind <when> <text> (e.g. /remind 10m stand up)".into(),
                )
            } else {
                CommandOutcome::Remind { when_raw, text }
            }
        }
        other => CommandOutcome::Error(format!("unknown command: /{other}")),
    }
}

/// Split `/remind`'s argument into `(when, text)`. An optional `in` prefix
/// (`in 10m ...`) is folded into the time phrase so `/remind in 2h ship it` parses
/// naturally; otherwise the first token is the when and the rest is the note.
fn split_remind(rest: &str) -> (String, String) {
    let rest = rest.trim();
    if let Some(after_in) = rest.strip_prefix("in ").or_else(|| rest.strip_prefix("IN ")) {
        let mut parts = after_in.trim_start().splitn(2, char::is_whitespace);
        let when = parts.next().unwrap_or("").trim();
        let text = parts.next().unwrap_or("").trim();
        return (format!("in {when}"), text.to_string());
    }
    let mut parts = rest.splitn(2, char::is_whitespace);
    let when = parts.next().unwrap_or("").trim();
    let text = parts.next().unwrap_or("").trim();
    (when.to_string(), text.to_string())
}

/// Upper bound on a reminder delay: one year. A request further out is rejected so
/// a fat-fingered unit can't stage a near-permanent row.
const MAX_REMINDER_SECS: u64 = 365 * 86_400;

/// Parse a `/remind` relative time phrase into a concrete [`Duration`].
///
/// Accepts `<positive-integer><unit>` (an optional leading `in` and surrounding
/// whitespace are tolerated, case-insensitively): e.g. `10m`, `2h`, `30s`, `1d`,
/// `in 90m`, `2 hours`. Units: `s`/`sec`/`second(s)`, `m`/`min`/`minute(s)`,
/// `h`/`hr`/`hour(s)`, `d`/`day(s)`. Returns `None` for anything it can't read as a
/// strictly-positive delay within [`MAX_REMINDER_SECS`] — including a bare number
/// (no unit), zero, an unknown unit, or overflow. Pure, so it unit-tests without a
/// clock or database.
///
/// [`Duration`]: std::time::Duration
#[must_use]
pub fn parse_reminder_delay(when_raw: &str) -> Option<std::time::Duration> {
    let lower = when_raw.trim().to_ascii_lowercase();
    // An optional leading `in ` ("in 2h") is folded away before parsing.
    let body = lower.strip_prefix("in ").map_or(lower.as_str(), str::trim);
    if body.is_empty() {
        return None;
    }
    // Split the leading digit run from the trailing unit. `find` returning `None`
    // means the whole thing is digits — i.e. no unit — which we reject.
    let split = body.find(|c: char| !c.is_ascii_digit())?;
    if split == 0 {
        return None; // no leading number (e.g. "soon")
    }
    let (num_str, unit) = body.split_at(split);
    let n: u64 = num_str.parse().ok()?;
    if n == 0 {
        return None;
    }
    let secs = match unit.trim() {
        "s" | "sec" | "secs" | "second" | "seconds" => n,
        "m" | "min" | "mins" | "minute" | "minutes" => n.checked_mul(60)?,
        "h" | "hr" | "hrs" | "hour" | "hours" => n.checked_mul(3_600)?,
        "d" | "day" | "days" => n.checked_mul(86_400)?,
        _ => return None,
    };
    if secs > MAX_REMINDER_SECS {
        return None;
    }
    Some(std::time::Duration::from_secs(secs))
}

/// Built-in command names + a one-line help string each. Single source of truth
/// for both the `/api/commands` autocomplete payload and the names list. Pure.
#[must_use]
pub fn builtin_help() -> Vec<(&'static str, &'static str)> {
    vec![
        ("me", "/me <action> — post an emote/action line (\"* you wave\")"),
        ("shrug", "/shrug [text] — append ¯\\_(ツ)_/¯ to your message"),
        ("giphy", "/giphy <query> — drop a GIF card for <query>"),
        ("remind", "/remind <when> <text> — schedule a reminder (e.g. 10m, 2h, in 30s)"),
    ]
}

/// Just the built-in command names (derived from [`builtin_help`]). Pure.
#[must_use]
pub fn builtin_command_names() -> Vec<&'static str> {
    builtin_help().into_iter().map(|(name, _)| name).collect()
}

// ----------------------------------------------------------------- Handlers

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

/// Resolve a participant's display name, falling back to their id string when the
/// lookup misses or errors (so a command never fails just to render a name).
async fn display_name_of(s: &AppState, who: ParticipantId) -> String {
    match s.participants.get(who).await {
        Ok(Some(p)) => p.display_name,
        _ => who.to_string(),
    }
}

/// `GET /api/commands` — list the built-in commands (name + one-line help) so the
/// client can render an autocomplete menu. Auth-gated; no room scope.
async fn list_commands(
    State(_s): State<AppState>,
    _auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let commands: Vec<serde_json::Value> = builtin_help()
        .into_iter()
        .map(|(name, help)| serde_json::json!({ "name": name, "help": help }))
        .collect();
    Ok(Json(serde_json::json!({ "commands": commands })))
}

#[derive(Deserialize)]
struct RunCommandReq {
    /// The raw composer text, e.g. `"/me waves"`. Must start with `/`.
    text: String,
}

/// `POST /api/rooms/:id/command` — parse one slash-command typed in a room and
/// apply its effect. Requires room access. Non-command input is a `400`.
///
/// * [`CommandOutcome::Post`] → sent through [`ImService::send_message`] (the same
///   path normal messages take); the persisted message is returned.
/// * [`CommandOutcome::Ephemeral`] → returned as `{ "ephemeral": msg }`, not posted.
/// * [`CommandOutcome::Error`] → `400 Invalid`.
/// * [`CommandOutcome::Remind`] → schedules a self-authored reminder note for later
///   delivery via [`ScheduledRepo`](aero_storage::ScheduledRepo); returns the new
///   reminder id and the resolved delivery time. A time it can't parse is a `400`.
///
/// [`ImService::send_message`]: aero_im_core::ImService::send_message
async fn run_command(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<RunCommandReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;

    let (name, rest) = parse_command(&req.text)
        .ok_or_else(|| AeroError::Invalid("not a command".into()))?;
    let author = display_name_of(&s, auth.participant_id).await;

    match render_command(&name, &rest, &author) {
        CommandOutcome::Post(blocks) => {
            let msg = s.im.send_message(auth.participant_id, room, blocks, None).await?;
            Ok(Json(serde_json::to_value(msg).map_err(AeroError::from)?))
        }
        CommandOutcome::Ephemeral(msg) => Ok(Json(serde_json::json!({ "ephemeral": msg }))),
        CommandOutcome::Error(e) => Err(AeroError::Invalid(e).into()),
        CommandOutcome::Remind { when_raw, text } => {
            // Resolve the relative time against the clock, then stage a self-authored
            // reminder note in the room via the durable scheduler. The background
            // `run_scheduled_dispatcher` replays it through `send_message` at the due
            // time, so the reminder arrives as an ordinary room message.
            let delay = parse_reminder_delay(&when_raw).ok_or_else(|| {
                AeroError::Invalid(format!(
                    "couldn't read the time '{when_raw}'; try 10m, 2h, 30s, 1d \
                     (e.g. /remind 10m stand up)"
                ))
            })?;
            let deliver_at = time::OffsetDateTime::now_utc()
                + time::Duration::seconds(i64::try_from(delay.as_secs()).unwrap_or(i64::MAX));
            let blocks = vec![Block::text(format!("⏰ Reminder: {text}"))];
            let id = ScheduledRepo::new(s.pg.clone())
                .create(room, auth.participant_id, &blocks, None, deliver_at)
                .await?;
            Ok(Json(serde_json::json!({
                "scheduled": true,
                "reminder_id": id,
                "deliver_at": deliver_at
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default(),
                "text": text,
            })))
        }
    }
}

// --------------------------------------------------------------------- Tests

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_command ----

    #[test]
    fn parse_command_splits_name_and_rest() {
        assert_eq!(
            parse_command("/me hugs"),
            Some(("me".into(), "hugs".into()))
        );
        assert_eq!(
            parse_command("/giphy cat party"),
            Some(("giphy".into(), "cat party".into()))
        );
    }

    #[test]
    fn parse_command_lowercases_name_and_trims_rest() {
        assert_eq!(
            parse_command("/ME   hugs the bot  "),
            Some(("me".into(), "hugs the bot".into()))
        );
        // Leading whitespace before the slash is tolerated.
        assert_eq!(parse_command("   /shrug"), Some(("shrug".into(), String::new())));
    }

    #[test]
    fn parse_command_name_only_has_empty_rest() {
        assert_eq!(parse_command("/shrug"), Some(("shrug".into(), String::new())));
    }

    #[test]
    fn parse_command_rejects_non_commands() {
        assert_eq!(parse_command("hello"), None);
        assert_eq!(parse_command(""), None);
        assert_eq!(parse_command("   "), None);
        assert_eq!(parse_command("not /a command"), None);
    }

    #[test]
    fn parse_command_rejects_bare_and_escaped_slash() {
        // A lone `/` is not a command.
        assert_eq!(parse_command("/"), None);
        // Slash followed by only whitespace is not a command.
        assert_eq!(parse_command("/   "), None);
        assert_eq!(parse_command("   /  "), None);
        // `//` is an escaped literal slash, not a command.
        assert_eq!(parse_command("//me"), None);
    }

    // ---- render_command: /me ----

    fn post_text(o: &CommandOutcome) -> &str {
        match o {
            CommandOutcome::Post(blocks) => match blocks.as_slice() {
                [Block::Text { content, .. }] => content,
                _ => panic!("expected a single text block, got {blocks:?}"),
            },
            _ => panic!("expected Post, got {o:?}"),
        }
    }

    #[test]
    fn me_renders_action_line() {
        let o = render_command("me", "waves at everyone", "Alice");
        assert_eq!(post_text(&o), "* Alice waves at everyone");
    }

    #[test]
    fn me_without_text_is_error() {
        assert!(matches!(render_command("me", "", "Alice"), CommandOutcome::Error(_)));
        assert!(matches!(render_command("me", "   ", "Alice"), CommandOutcome::Error(_)));
    }

    // ---- render_command: /shrug ----

    #[test]
    fn shrug_without_text_is_just_the_emoji() {
        let o = render_command("shrug", "", "Bob");
        assert_eq!(post_text(&o), "¯\\_(ツ)_/¯");
    }

    #[test]
    fn shrug_with_text_prepends_it() {
        let o = render_command("shrug", "who knows", "Bob");
        assert_eq!(post_text(&o), "who knows ¯\\_(ツ)_/¯");
    }

    #[test]
    fn shrug_trims_surrounding_space() {
        // Even with padded input the bare form has no leading space.
        let o = render_command("shrug", "   ", "Bob");
        assert_eq!(post_text(&o), "¯\\_(ツ)_/¯");
    }

    // ---- render_command: /giphy ----

    #[test]
    fn giphy_renders_card_with_query() {
        match render_command("giphy", "dancing cat", "Carol") {
            CommandOutcome::Post(blocks) => match blocks.as_slice() {
                [Block::Card { schema, payload }] => {
                    assert_eq!(schema, "giphy");
                    assert_eq!(payload, &serde_json::json!({ "query": "dancing cat" }));
                }
                _ => panic!("expected one giphy card, got {blocks:?}"),
            },
            other => panic!("expected Post, got {other:?}"),
        }
    }

    #[test]
    fn giphy_without_query_is_error() {
        assert!(matches!(render_command("giphy", "", "Carol"), CommandOutcome::Error(_)));
    }

    // ---- render_command: /remind ----

    #[test]
    fn remind_splits_when_and_text() {
        match render_command("remind", "10m stand up", "Dee") {
            CommandOutcome::Remind { when_raw, text } => {
                assert_eq!(when_raw, "10m");
                assert_eq!(text, "stand up");
            }
            other => panic!("expected Remind, got {other:?}"),
        }
    }

    #[test]
    fn remind_supports_in_prefix() {
        match render_command("remind", "in 2h ship the release", "Dee") {
            CommandOutcome::Remind { when_raw, text } => {
                assert_eq!(when_raw, "in 2h");
                assert_eq!(text, "ship the release");
            }
            other => panic!("expected Remind, got {other:?}"),
        }
    }

    #[test]
    fn remind_without_text_or_when_is_error() {
        assert!(matches!(render_command("remind", "", "Dee"), CommandOutcome::Error(_)));
        // Only a "when", no note.
        assert!(matches!(render_command("remind", "10m", "Dee"), CommandOutcome::Error(_)));
    }

    // ---- parse_reminder_delay ----

    fn secs(when: &str) -> Option<u64> {
        parse_reminder_delay(when).map(|d| d.as_secs())
    }

    #[test]
    fn reminder_delay_parses_each_unit() {
        assert_eq!(secs("30s"), Some(30));
        assert_eq!(secs("10m"), Some(600));
        assert_eq!(secs("2h"), Some(7_200));
        assert_eq!(secs("1d"), Some(86_400));
    }

    #[test]
    fn reminder_delay_tolerates_in_prefix_case_and_spacing() {
        assert_eq!(secs("in 2h"), Some(7_200));
        assert_eq!(secs("IN 90m"), Some(5_400));
        assert_eq!(secs("  45M  "), Some(2_700));
        // Long-form units and an internal space.
        assert_eq!(secs("2 hours"), Some(7_200));
        assert_eq!(secs("15 minutes"), Some(900));
    }

    #[test]
    fn reminder_delay_rejects_garbage_zero_and_bare_numbers() {
        assert_eq!(secs(""), None);
        assert_eq!(secs("soon"), None);
        assert_eq!(secs("10"), None); // no unit
        assert_eq!(secs("0m"), None); // zero is not a delay
        assert_eq!(secs("5y"), None); // unknown unit
        assert_eq!(secs("m10"), None); // unit before number
    }

    #[test]
    fn reminder_delay_caps_at_one_year() {
        // 365d is allowed; 366d is over the ceiling.
        assert_eq!(secs("365d"), Some(MAX_REMINDER_SECS));
        assert_eq!(secs("366d"), None);
        // Multiplication overflow is rejected (None), not a panic.
        assert_eq!(secs("99999999999999999999d"), None);
    }

    // ---- render_command: unknown ----

    #[test]
    fn unknown_command_is_error_with_name() {
        match render_command("foo", "anything", "Eve") {
            CommandOutcome::Error(e) => assert_eq!(e, "unknown command: /foo"),
            other => panic!("expected Error, got {other:?}"),
        }
        assert!(matches!(render_command("", "", "Eve"), CommandOutcome::Error(_)));
    }

    // ---- help / names ----

    #[test]
    fn builtin_help_and_names_are_consistent() {
        let help = builtin_help();
        let names = builtin_command_names();
        assert_eq!(help.len(), names.len());
        for ((name, text), n) in help.iter().zip(names.iter()) {
            assert_eq!(name, n);
            assert!(!text.is_empty(), "help for /{name} is empty");
            // Every help line documents its own command name.
            assert!(text.contains(name), "help for /{name} should mention it: {text}");
        }
    }

    #[test]
    fn every_documented_command_renders_a_non_error_for_valid_input() {
        // The help list must not advertise a command `render_command` rejects.
        for name in builtin_command_names() {
            let rest = match name {
                "remind" => "10m do the thing",
                _ => "some args",
            };
            assert!(
                !matches!(render_command(name, rest, "User"), CommandOutcome::Error(_)),
                "documented command /{name} unexpectedly errored on valid input"
            );
        }
    }
}
