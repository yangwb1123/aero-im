//! Live-stream chat-modes repository (slow mode / follower-only / subscriber-only).
//!
//! Backs `migrations/0071_stream_chat_settings.sql`. A stream owner configures
//! Twitch-style chat restrictions for their stream's danmaku chat: a slow-mode
//! window, follower-only posting, and subscriber-only posting. There is at most
//! one row per stream (PK = `stream_id`, FK to `streams` cascading on delete);
//! an absent row means *all defaults* (no restrictions), so a stream that never
//! configured chat modes behaves exactly as before — [`StreamChatSettingsRepo::get`]
//! returns `None`, which the caller treats as [`StreamChatSettings::default`].
//!
//! Enforcement of the restrictions on the *posting* path lives next to the
//! danmaku handlers (`stream_chat_post` in `aero-server`'s `routes` and the
//! `StreamChat` WS frame), which load the settings and reject a post that
//! violates them with 403 before the line is accepted/broadcast.
//!
//! Stream ids are [`Ulid`]s stored as UUID (same as the `streams` table), bound
//! via `uuid::Uuid::from_u128(ulid.0)` to match [`StreamRepo`](crate::StreamRepo).
//! Purely additive: a NEW [`StreamChatSettingsRepo`]; no existing repo is touched.

use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use ulid::Ulid;
use uuid::Uuid;

/// The chat-mode restrictions configured for a stream's danmaku chat.
///
/// A storage-layer projection of a `stream_chat_settings` row. [`Default`] is
/// the unrestricted baseline returned when no row exists (slow mode off,
/// everyone may post). `Serialize` so a handler can hand it straight back as
/// JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct StreamChatSettings {
    /// Minimum seconds between two posts from the same viewer (`0` ⇒ disabled).
    pub slow_mode_secs: i32,
    /// Only viewers following the creator may post.
    pub follower_only: bool,
    /// Only active creator-subscribers may post.
    pub subscriber_only: bool,
}

impl Default for StreamChatSettings {
    /// The unrestricted baseline: no slow mode, no follower/subscriber gate.
    fn default() -> Self {
        Self {
            slow_mode_secs: 0,
            follower_only: false,
            subscriber_only: false,
        }
    }
}

/// Whether posting now would violate a slow-mode window of `secs` seconds.
///
/// `secs <= 0` disables slow mode (never a violation). With slow mode on, a
/// post is a violation only when the viewer's `last_post` is known AND less than
/// `secs` seconds in the past relative to `now`; a viewer who has not posted
/// before (`None`) is always allowed. The boundary is inclusive on the allow
/// side: a post exactly `secs` seconds after the last is permitted.
///
/// Pure (no DB / no clock of its own), so it unit-tests hermetically.
#[must_use]
pub fn slow_mode_violation(
    last_post: Option<OffsetDateTime>,
    now: OffsetDateTime,
    secs: i32,
) -> bool {
    if secs <= 0 {
        return false;
    }
    match last_post {
        None => false,
        Some(last) => {
            let window = time::Duration::seconds(i64::from(secs));
            // Violation iff the next allowed instant (last + window) is still in
            // the future: the elapsed gap is strictly less than the window.
            now < last + window
        }
    }
}

/// Repository over the `stream_chat_settings` table (per-stream chat modes).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`StreamChatSettingsRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct StreamChatSettingsRepo {
    pool: PgPool,
}

impl StreamChatSettingsRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Fetch the chat-mode settings for `stream`, or `None` if none are
    /// configured (the caller treats `None` as [`StreamChatSettings::default`]).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(&self, stream: Ulid) -> Result<Option<StreamChatSettings>, sqlx::Error> {
        let row = sqlx::query_as::<_, (i32, bool, bool)>(
            r"SELECT slow_mode_secs, follower_only, subscriber_only
               FROM stream_chat_settings
              WHERE stream_id = $1",
        )
        .bind(Uuid::from_u128(stream.0))
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(slow_mode_secs, follower_only, subscriber_only)| StreamChatSettings {
            slow_mode_secs,
            follower_only,
            subscriber_only,
        }))
    }

    /// Set (upsert) the chat-mode settings for `stream`, refreshing `updated_at`.
    /// The caller is responsible for verifying owner identity and clamping
    /// `slow_mode_secs` to a sane non-negative range.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the upsert.
    pub async fn set(
        &self,
        stream: Ulid,
        slow_mode_secs: i32,
        follower_only: bool,
        subscriber_only: bool,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO stream_chat_settings
                  (stream_id, slow_mode_secs, follower_only, subscriber_only, updated_at)
               VALUES ($1, $2, $3, $4, now())
               ON CONFLICT (stream_id)
               DO UPDATE SET slow_mode_secs  = EXCLUDED.slow_mode_secs,
                             follower_only   = EXCLUDED.follower_only,
                             subscriber_only = EXCLUDED.subscriber_only,
                             updated_at      = now()",
        )
        .bind(Uuid::from_u128(stream.0))
        .bind(slow_mode_secs)
        .bind(follower_only)
        .bind(subscriber_only)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    #[test]
    fn default_is_unrestricted() {
        let d = StreamChatSettings::default();
        assert_eq!(d.slow_mode_secs, 0);
        assert!(!d.follower_only);
        assert!(!d.subscriber_only);
    }

    #[test]
    fn slow_mode_disabled_never_violates() {
        let now = OffsetDateTime::now_utc();
        // secs <= 0 disables slow mode regardless of last_post.
        assert!(!slow_mode_violation(Some(now), now, 0));
        assert!(!slow_mode_violation(Some(now), now, -5));
        assert!(!slow_mode_violation(None, now, 0));
    }

    #[test]
    fn slow_mode_first_post_allowed() {
        let now = OffsetDateTime::now_utc();
        // No prior post ⇒ never a violation even with slow mode on.
        assert!(!slow_mode_violation(None, now, 30));
    }

    #[test]
    fn slow_mode_within_window_violates() {
        let now = OffsetDateTime::now_utc();
        // Posted 5s ago, 30s window ⇒ still inside the window ⇒ violation.
        assert!(slow_mode_violation(Some(now - Duration::seconds(5)), now, 30));
        // Posted right now ⇒ violation.
        assert!(slow_mode_violation(Some(now), now, 30));
    }

    #[test]
    fn slow_mode_after_window_allowed() {
        let now = OffsetDateTime::now_utc();
        // Posted 31s ago, 30s window ⇒ window elapsed ⇒ allowed.
        assert!(!slow_mode_violation(Some(now - Duration::seconds(31)), now, 30));
    }

    #[test]
    fn slow_mode_boundary_is_inclusive_on_allow() {
        let now = OffsetDateTime::now_utc();
        // Exactly `secs` seconds elapsed ⇒ next allowed instant == now ⇒ NOT a
        // violation (`now < last + window` is false at equality).
        assert!(!slow_mode_violation(Some(now - Duration::seconds(30)), now, 30));
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored stream_chat_settings
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::ParticipantId;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a throwaway stream (and its owner) so the test is self-contained.
    /// `stream_chat_settings.stream_id` has a real FK to `streams(id)`, so a row
    /// must exist before we can upsert settings against it.
    async fn mk_stream(p: &PgPool) -> Ulid {
        let owner = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(owner.to_uuid())
            .bind(format!("chat-settings-owner-{owner}"))
            .execute(p)
            .await
            .expect("insert participant");

        let stream = Ulid::new();
        sqlx::query(
            r"INSERT INTO streams (id, owner_id, title, stream_key, status, protocol, created_at)
               VALUES ($1, $2, $3, $4, 'idle', 'rtmp', now())",
        )
        .bind(Uuid::from_u128(stream.0))
        .bind(owner.to_uuid())
        .bind("chat-settings-stream")
        .bind(format!("key-{stream}"))
        .execute(p)
        .await
        .expect("insert stream");
        stream
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn stream_chat_settings_get_defaults_and_upsert() {
        let p = pool();
        let repo = StreamChatSettingsRepo::new(p.clone());
        let stream = mk_stream(&p).await;

        // No row yet ⇒ get returns None (caller treats as default).
        assert!(
            repo.get(stream).await.unwrap().is_none(),
            "unconfigured stream has no settings row"
        );

        // First set inserts.
        repo.set(stream, 30, true, false).await.unwrap();
        let got = repo.get(stream).await.unwrap().expect("present after set");
        assert_eq!(got.slow_mode_secs, 30);
        assert!(got.follower_only);
        assert!(!got.subscriber_only);

        // Second set upserts the same row (still one row, new values).
        repo.set(stream, 0, false, true).await.unwrap();
        let got = repo.get(stream).await.unwrap().expect("present after re-set");
        assert_eq!(got.slow_mode_secs, 0);
        assert!(!got.follower_only);
        assert!(got.subscriber_only);

        // Cleanup (cascades the settings row via the FK).
        sqlx::query("DELETE FROM streams WHERE id = $1")
            .bind(Uuid::from_u128(stream.0))
            .execute(&p)
            .await
            .ok();
    }
}
