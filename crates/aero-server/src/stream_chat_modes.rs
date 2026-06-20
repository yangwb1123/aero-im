//! Live-stream chat-modes HTTP API + posting-path enforcement (Twitch-style).
//!
//! Additive layer over the new [`aero_storage::StreamChatSettingsRepo`]. The
//! stream *owner* (`stream.owner_id == auth.participant_id`) configures three
//! chat restrictions for their stream's danmaku chat:
//!
//!   * **slow mode** — a minimum number of seconds between two posts from the
//!     same viewer;
//!   * **follower-only** — only viewers following the creator may post;
//!   * **subscriber-only** — only active creator-subscribers may post.
//!
//! The HTTP surface here is just owner read/write of those settings
//! (`GET`/`PUT /api/streams/:id/chat-settings`). *Enforcement* of the modes on
//! the posting path lives in [`enforce_chat_modes`], which both the REST
//! `stream_chat_post` handler and the WS `StreamChat` frame call (after the
//! existing ban check) to reject a violating post with 403 before the line is
//! accepted/broadcast.
//!
//! Slow mode needs a per-(stream, viewer) last-post timestamp. Rather than widen
//! [`AppState`], we keep a process-static [`DashMap`] keyed by
//! `(stream, ParticipantId)`. This is best-effort rate limiting (not a security
//! boundary — follower/subscriber gates are authoritative against Postgres), so
//! a per-process map is the correct, least-invasive choice; in a multi-node
//! deployment each node enforces its own window, which only ever *relaxes* slow
//! mode, never lets a banned/ungated viewer through. Nothing here mutates
//! existing modules' state.

use std::str::FromStr;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use aero_common::{Error as AeroError, ParticipantId, Result as AeroResult};
use aero_storage::{
    StreamChatSettings, StreamChatSettingsRepo, StreamFollowRepo, StreamRepo, SubscriptionRepo,
};
use axum::{
    extract::{Path, State},
    routing::get,
    Json, Router,
};
use dashmap::DashMap;
use serde::Deserialize;
use ulid::Ulid;

use aero_auth::AuthUser;

use crate::error::ApiResult;
use crate::state::AppState;

/// Upper bound on the slow-mode window a stream owner can set (24h). Keeps a
/// fat-fingered value from effectively freezing chat forever and bounds the
/// stored `int`.
const MAX_SLOW_MODE_SECS: i32 = 86_400;

/// Process-static per-(stream, viewer) last-post instants, used only for
/// slow-mode enforcement. Best-effort, per-process (see module docs). Keyed by
/// the stream id and the posting viewer; the value is the `Instant` of that
/// viewer's most recent accepted post.
static LAST_POST: LazyLock<DashMap<(Ulid, ParticipantId), Instant>> =
    LazyLock::new(DashMap::new);

/// Mount the chat-modes routes. Folded into the main router by
/// [`crate::routes::build`]; kept separate so the chat-modes surface lives next
/// to its own storage repo, additively over the live/danmaku path.
pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/api/streams/:id/chat-settings",
        get(get_settings).put(put_settings),
    )
}

fn parse_stream_id(s: &str) -> AeroResult<Ulid> {
    Ulid::from_str(s).map_err(|e| AeroError::Invalid(format!("stream id: {e}")))
}

fn settings_repo(s: &AppState) -> StreamChatSettingsRepo {
    StreamChatSettingsRepo::new(s.participants.pool().clone())
}

/// Resolve a stream and assert the caller owns it. Returns the stream id on
/// success; `NotFound`/`Forbidden` otherwise. Mirrors `stream_mod::require_owner`.
async fn require_owner(s: &AppState, stream_str: &str, caller: ParticipantId) -> AeroResult<Ulid> {
    let stream_id = parse_stream_id(stream_str)?;
    let stream = StreamRepo::new(s.participants.pool().clone())
        .get(stream_id)
        .await?
        .ok_or_else(|| AeroError::NotFound(format!("stream {stream_id}")))?;
    if stream.owner_id != caller {
        return Err(AeroError::Forbidden(
            "only the stream owner may configure its chat modes".into(),
        ));
    }
    Ok(stream_id)
}

/// `GET /api/streams/:id/chat-settings` — owner reads the stream's chat modes
/// (defaults when none are configured).
async fn get_settings(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<StreamChatSettings>> {
    let stream_id = require_owner(&s, &id_str, auth.participant_id).await?;
    let settings = settings_repo(&s)
        .get(stream_id)
        .await?
        .unwrap_or_default();
    Ok(Json(settings))
}

#[derive(Deserialize)]
struct ChatSettingsReq {
    #[serde(default)]
    slow_mode_secs: i32,
    #[serde(default)]
    follower_only: bool,
    #[serde(default)]
    subscriber_only: bool,
}

/// `PUT /api/streams/:id/chat-settings` — owner sets the stream's chat modes.
async fn put_settings(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<ChatSettingsReq>,
) -> ApiResult<Json<StreamChatSettings>> {
    let stream_id = require_owner(&s, &id_str, auth.participant_id).await?;
    if req.slow_mode_secs < 0 {
        return Err(AeroError::Invalid("slow_mode_secs must be non-negative".into()).into());
    }
    if req.slow_mode_secs > MAX_SLOW_MODE_SECS {
        return Err(AeroError::Invalid(format!(
            "slow_mode_secs must be at most {MAX_SLOW_MODE_SECS}"
        ))
        .into());
    }
    settings_repo(&s)
        .set(
            stream_id,
            req.slow_mode_secs,
            req.follower_only,
            req.subscriber_only,
        )
        .await?;
    Ok(Json(StreamChatSettings {
        slow_mode_secs: req.slow_mode_secs,
        follower_only: req.follower_only,
        subscriber_only: req.subscriber_only,
    }))
}

/// Enforce a stream's chat modes for `sender` posting to `stream`, called by
/// BOTH the REST `stream_chat_post` handler and the WS `StreamChat` frame after
/// the existing ban check, before the line is accepted/broadcast.
///
/// On success the viewer's slow-mode clock is advanced (so the *next* post is
/// rate-limited). Returns:
///   * `Forbidden("followers only ...")` when follower-only is on and `sender`
///     does not follow the creator;
///   * `Forbidden("subscribers only ...")` when subscriber-only is on and
///     `sender` has no active subscription to the creator;
///   * `Forbidden("slow mode ...")` when the slow-mode window has not elapsed
///     since `sender`'s last post.
///
/// The stream owner is exempt from all three restrictions (a creator can always
/// post in their own chat). An unconfigured stream (no settings row) is the
/// unrestricted baseline and always passes cheaply.
///
/// # Errors
/// Propagates any [`sqlx::Error`] (as [`AeroError`]) from the lookups, or one of
/// the `Forbidden` variants above on a violation.
pub async fn enforce_chat_modes(
    state: &AppState,
    stream: Ulid,
    sender: ParticipantId,
) -> AeroResult<()> {
    let pool = state.participants.pool().clone();
    let settings = StreamChatSettingsRepo::new(pool.clone())
        .get(stream)
        .await?
        .unwrap_or_default();

    // Cheap exit: an unconfigured/unrestricted stream skips all the work.
    if settings == StreamChatSettings::default() {
        return Ok(());
    }

    // The owner is exempt from their own chat's restrictions. Resolve once and
    // reuse the creator id for the follower/subscriber checks below.
    let creator = StreamRepo::new(pool.clone())
        .get(stream)
        .await?
        .ok_or_else(|| AeroError::NotFound(format!("stream {stream}")))?
        .owner_id;
    if sender == creator {
        return Ok(());
    }

    if settings.follower_only
        && !StreamFollowRepo::new(pool.clone())
            .is_following(sender, creator)
            .await?
    {
        return Err(AeroError::Forbidden(
            "followers only: follow the creator to chat".into(),
        ));
    }

    if settings.subscriber_only
        && !SubscriptionRepo::new(pool.clone())
            .is_subscribed(creator, sender)
            .await?
    {
        return Err(AeroError::Forbidden(
            "subscribers only: subscribe to the creator to chat".into(),
        ));
    }

    if settings.slow_mode_secs > 0 {
        slow_mode_check_and_record(stream, sender, settings.slow_mode_secs)?;
    }

    Ok(())
}

/// Slow-mode gate against the process-static [`LAST_POST`] map: reject if the
/// viewer's last post in this stream was less than `secs` seconds ago; otherwise
/// record `now` and allow. Split out so the timestamp bookkeeping stays in one
/// place. `secs > 0` is the caller's precondition.
fn slow_mode_check_and_record(
    stream: Ulid,
    sender: ParticipantId,
    secs: i32,
) -> AeroResult<()> {
    let key = (stream, sender);
    let now = Instant::now();
    let window = std::time::Duration::from_secs(u64::from(secs.unsigned_abs()));
    if let Some(last) = LAST_POST.get(&key) {
        if now.duration_since(*last) < window {
            return Err(AeroError::Forbidden(format!(
                "slow mode: wait {secs}s between messages"
            )));
        }
    }
    LAST_POST.insert(key, now);
    Ok(())
}

/// Evict slow-mode bookkeeping entries older than `idle_after` from the
/// process-static [`LAST_POST`] map, returning how many were removed.
///
/// `LAST_POST` is keyed by `(stream, viewer)` and gains one never-overwritten
/// entry per distinct viewer that ever posts under slow mode, so without a sweep
/// it grows unbounded over the process lifetime (it is NOT in `AppState`, so the
/// gateway's rate-limiter sweep loop cannot reach it). An entry only gates a post
/// while it is younger than the slow-mode window (≤ [`MAX_SLOW_MODE_SECS`]); once
/// older, a missing key behaves identically (both ⇒ allowed), so dropping it
/// cannot change any future decision. Call with `idle_after >= MAX_SLOW_MODE_SECS`
/// so a still-relevant entry is never evicted (fail-safe toward enforcement,
/// mirroring [`crate::rate_limit`]'s `sweep_idle`).
pub fn sweep_idle(now: Instant, idle_after: Duration) -> usize {
    // Count evictions INSIDE retain rather than diffing len() before/after: this
    // map is a shared static, so a concurrent post could insert between two len()
    // reads and make the difference underflow. This counts exactly what this call
    // dropped, regardless of concurrent inserts.
    let mut removed = 0usize;
    LAST_POST.retain(|_, last| {
        let keep = now.saturating_duration_since(*last) < idle_after;
        removed += usize::from(!keep);
        keep
    });
    removed
}

/// The smallest `idle_after` that can never evict an entry still able to gate a
/// post: the maximum configurable slow-mode window. Exposed so the sweep loop
/// passes a provably-safe threshold.
pub const MIN_SAFE_SWEEP_IDLE: Duration = Duration::from_secs(MAX_SLOW_MODE_SECS as u64);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_mode_first_post_records_and_allows() {
        // A fresh (stream, sender) key has no prior post ⇒ allowed, and the post
        // is then recorded so an immediate retry is rejected.
        let stream = Ulid::new();
        let sender = ParticipantId::new();
        assert!(slow_mode_check_and_record(stream, sender, 30).is_ok());
        // Immediate second post within the window is rejected.
        assert!(slow_mode_check_and_record(stream, sender, 30).is_err());
    }

    #[test]
    fn slow_mode_independent_per_stream_and_sender() {
        // Distinct keys don't interfere: posting as sender A in stream S does not
        // rate-limit sender B, nor the same sender in a different stream.
        let s1 = Ulid::new();
        let s2 = Ulid::new();
        let a = ParticipantId::new();
        let b = ParticipantId::new();
        assert!(slow_mode_check_and_record(s1, a, 30).is_ok());
        assert!(
            slow_mode_check_and_record(s1, b, 30).is_ok(),
            "a different sender is independent"
        );
        assert!(
            slow_mode_check_and_record(s2, a, 30).is_ok(),
            "the same sender in a different stream is independent"
        );
    }

    #[test]
    fn sweep_idle_evicts_stale_but_keeps_active_entries() {
        // Unique keys so this never collides with the other tests sharing the
        // process-static map. A 60s threshold evicts only entries older than 60s,
        // so concurrent tests' freshly-recorded entries (≈0s old) always survive.
        let stale_key = (Ulid::new(), ParticipantId::new());
        let active_key = (Ulid::new(), ParticipantId::new());
        let now = Instant::now();
        let stale = now
            .checked_sub(Duration::from_secs(120))
            .expect("instant 120s in the past is representable");
        LAST_POST.insert(stale_key, stale);
        LAST_POST.insert(active_key, now);

        let removed = sweep_idle(Instant::now(), Duration::from_secs(60));

        assert!(removed >= 1, "the stale entry must be evicted");
        assert!(
            LAST_POST.get(&stale_key).is_none(),
            "an entry older than idle_after is dropped (a missing key == allowed)"
        );
        assert!(
            LAST_POST.get(&active_key).is_some(),
            "an entry within the window must survive so it can still gate posts"
        );
        LAST_POST.remove(&active_key); // don't leak our own key into other tests
    }
}
