//! Durable user custom status + presence preference repository.
//!
//! Backs `migrations/0022_user_status.sql`. A participant sets a custom status —
//! an emoji shorthand (`:palm_tree:`), free text ("On vacation"), an optional
//! auto-expiry — and a coarse [`Presence`] preference (active/away). Others read
//! it on a profile / member list. One row per participant (keyed on
//! `participant_id`), so [`set`](UserStatusRepo::set) is an idempotent upsert and
//! [`clear`](UserStatusRepo::clear) deletes the row.
//!
//! This is the DURABLE counterpart to the ephemeral Redis
//! [`PresenceStore`](crate::PresenceStore), which tracks whether a client is
//! currently connected to a room (online heartbeat). The two are kept separate:
//! this stores the user's *chosen* status, that stores *live* connectivity.
//!
//! ## Expiry semantics
//! `expires_at` applies only to the CUSTOM status (emoji + text). When it has
//! passed, a read reports the custom status as CLEARED — `emoji`/`text` become
//! `None` — while the participant's [`Presence`] preference is KEPT (a user who
//! set "away until 5pm" stays away after 5pm; only the decoration drops). The
//! row is not deleted on read; it is normalized on the next [`set`] /
//! [`clear`]. `expires_at == None` means the custom status never expires.
//!
//! Purely additive: a NEW [`UserStatusRepo`]; no existing repo is touched. The
//! pure [`is_expired`] decision is unit-tested without a database.

use aero_common::{ParticipantId, Presence, UserStatus};
use sqlx::PgPool;
use time::OffsetDateTime;

/// Whether a custom status with the given `expires_at` is expired as of `now`.
///
/// `None` (no expiry) is never expired. Otherwise expired once `now` has reached
/// or passed `expires_at`. Pure, so the boundary behaviour is unit-tested offline.
#[must_use]
pub fn is_expired(expires_at: Option<OffsetDateTime>, now: OffsetDateTime) -> bool {
    matches!(expires_at, Some(at) if now >= at)
}

/// Row shape returned by the status queries (column order matches the SELECTs).
type StatusRow = (
    uuid::Uuid,
    Option<String>,
    Option<String>,
    String,
    Option<OffsetDateTime>,
    OffsetDateTime,
);

/// Build a [`UserStatus`] from a raw row, applying the [`is_expired`] clearing
/// rule (expired custom status ⇒ `emoji`/`text` dropped, `presence` kept).
fn row_to_status(row: StatusRow, now: OffsetDateTime) -> UserStatus {
    let (participant_id, emoji, text, presence, expires_at, updated_at) = row;
    let expired = is_expired(expires_at, now);
    UserStatus {
        participant_id: ParticipantId::from_uuid(participant_id),
        emoji: if expired { None } else { emoji },
        text: if expired { None } else { text },
        presence: Presence::from_str_lenient(&presence),
        // Once expired, the custom-status expiry has fired; surface it as cleared.
        expires_at: if expired { None } else { expires_at },
        updated_at,
    }
}

#[derive(Clone)]
pub struct UserStatusRepo {
    pool: PgPool,
}

impl UserStatusRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Set (upsert) `participant`'s status. Idempotent and keyed on the
    /// participant: re-setting overwrites the previous status. `emoji`/`text`
    /// are the optional custom-status decoration; `presence` is the coarse
    /// preference token (see [`Presence::as_str`]); `expires_at` is the optional
    /// custom-status auto-expiry. Returns the stored status (post-expiry
    /// normalization, computed against the current time).
    pub async fn set(
        &self,
        participant: ParticipantId,
        emoji: Option<&str>,
        text: Option<&str>,
        presence: &str,
        expires_at: Option<OffsetDateTime>,
    ) -> Result<UserStatus, sqlx::Error> {
        // Normalize the presence token through the lenient parser so only the
        // canonical lowercase form ever lands in the column.
        let presence_token = Presence::from_str_lenient(presence).as_str();
        let row = sqlx::query_as::<_, StatusRow>(
            r"INSERT INTO user_status (participant_id, emoji, text, presence, expires_at, updated_at)
               VALUES ($1, $2, $3, $4, $5, now())
               ON CONFLICT (participant_id)
               DO UPDATE SET emoji      = EXCLUDED.emoji,
                             text       = EXCLUDED.text,
                             presence   = EXCLUDED.presence,
                             expires_at = EXCLUDED.expires_at,
                             updated_at = now()
               RETURNING participant_id, emoji, text, presence, expires_at, updated_at",
        )
        .bind(participant.to_uuid())
        .bind(emoji)
        .bind(text)
        .bind(presence_token)
        .bind(expires_at)
        .fetch_one(&self.pool)
        .await?;
        Ok(row_to_status(row, OffsetDateTime::now_utc()))
    }

    /// `participant`'s current status, or `None` when they have never set one.
    /// An expired custom status is reported CLEARED (emoji/text `None`) with the
    /// presence preference kept — see the module-level expiry semantics.
    pub async fn get(&self, participant: ParticipantId) -> Result<Option<UserStatus>, sqlx::Error> {
        let row = sqlx::query_as::<_, StatusRow>(
            r"SELECT participant_id, emoji, text, presence, expires_at, updated_at
               FROM user_status
               WHERE participant_id = $1",
        )
        .bind(participant.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        let now = OffsetDateTime::now_utc();
        Ok(row.map(|r| row_to_status(r, now)))
    }

    /// Batch-fetch statuses for many participants (for rendering a member list in
    /// one round-trip). Participants without a status row are simply absent from
    /// the result; expired custom statuses are reported cleared, as in [`get`].
    /// Returns at most one entry per input id.
    pub async fn get_many(
        &self,
        participants: &[ParticipantId],
    ) -> Result<Vec<UserStatus>, sqlx::Error> {
        if participants.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<uuid::Uuid> = participants.iter().map(ParticipantId::to_uuid).collect();
        let rows = sqlx::query_as::<_, StatusRow>(
            r"SELECT participant_id, emoji, text, presence, expires_at, updated_at
               FROM user_status
               WHERE participant_id = ANY($1)",
        )
        .bind(&ids)
        .fetch_all(&self.pool)
        .await?;
        let now = OffsetDateTime::now_utc();
        Ok(rows.into_iter().map(|r| row_to_status(r, now)).collect())
    }

    /// Clear `participant`'s status entirely (deletes the row). Returns `true`
    /// when a status was removed; idempotent (clearing a never-set status is a
    /// no-op `false`).
    pub async fn clear(&self, participant: ParticipantId) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(r"DELETE FROM user_status WHERE participant_id = $1")
            .bind(participant.to_uuid())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    #[test]
    fn is_expired_boundaries() {
        let now = OffsetDateTime::UNIX_EPOCH;
        // No expiry is never expired.
        assert!(!is_expired(None, now));
        // Future expiry: not yet expired.
        assert!(!is_expired(Some(now + Duration::seconds(1)), now));
        // Exactly at the boundary counts as expired (>=).
        assert!(is_expired(Some(now), now));
        // Past expiry: expired.
        assert!(is_expired(Some(now - Duration::seconds(1)), now));
    }

    #[test]
    fn row_to_status_clears_expired_custom_status_but_keeps_presence() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let pid = ParticipantId::new();
        let row: StatusRow = (
            pid.to_uuid(),
            Some(":palm_tree:".into()),
            Some("On vacation".into()),
            "away".into(),
            Some(now - Duration::seconds(1)), // already expired
            now,
        );
        let status = row_to_status(row, now);
        assert!(status.emoji.is_none(), "expired emoji cleared");
        assert!(status.text.is_none(), "expired text cleared");
        assert!(status.expires_at.is_none(), "expiry surfaced as cleared");
        assert_eq!(status.presence, Presence::Away, "presence preference kept");
    }

    #[test]
    fn row_to_status_keeps_unexpired_custom_status() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let pid = ParticipantId::new();
        let row: StatusRow = (
            pid.to_uuid(),
            Some(":coffee:".into()),
            Some("Coffee break".into()),
            "active".into(),
            Some(now + Duration::hours(1)), // not yet expired
            now,
        );
        let status = row_to_status(row, now);
        assert_eq!(status.emoji.as_deref(), Some(":coffee:"));
        assert_eq!(status.text.as_deref(), Some("Coffee break"));
        assert_eq!(status.presence, Presence::Active);
        assert!(status.expires_at.is_some());
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored user_status_
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

    // Create a throwaway participant so the FK is satisfied.
    async fn fixture(p: &PgPool) -> ParticipantId {
        let participant = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(participant.to_uuid())
            .bind(format!("status-participant-{participant}"))
            .execute(p)
            .await
            .expect("insert participant");
        participant
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn user_status_set_get_clear_roundtrip() {
        let p = pool();
        let repo = UserStatusRepo::new(p.clone());
        let participant = fixture(&p).await;

        assert!(
            repo.get(participant).await.unwrap().is_none(),
            "no status initially"
        );

        let set = repo
            .set(
                participant,
                Some(":palm_tree:"),
                Some("On vacation"),
                "away",
                None,
            )
            .await
            .unwrap();
        assert_eq!(set.emoji.as_deref(), Some(":palm_tree:"));
        assert_eq!(set.presence, Presence::Away);

        let got = repo
            .get(participant)
            .await
            .unwrap()
            .expect("status present");
        assert_eq!(got.emoji.as_deref(), Some(":palm_tree:"));
        assert_eq!(got.text.as_deref(), Some("On vacation"));
        assert_eq!(got.presence, Presence::Away);

        // Re-set is an idempotent upsert (overwrites + normalizes the presence token).
        let reset = repo
            .set(participant, Some(":coffee:"), None, "bogus-token", None)
            .await
            .unwrap();
        assert_eq!(reset.emoji.as_deref(), Some(":coffee:"));
        assert!(reset.text.is_none(), "text overwritten to NULL");
        assert_eq!(
            reset.presence,
            Presence::Active,
            "unknown token normalized to active"
        );

        // get_many sees the row.
        let many = repo.get_many(&[participant]).await.unwrap();
        assert_eq!(many.len(), 1);
        assert_eq!(many[0].participant_id, participant);

        assert!(repo.clear(participant).await.unwrap(), "clear removed it");
        assert!(
            !repo.clear(participant).await.unwrap(),
            "second clear is a no-op"
        );
        assert!(repo.get(participant).await.unwrap().is_none(), "cleared");
        assert!(repo.get_many(&[participant]).await.unwrap().is_empty());
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn user_status_expired_custom_status_reported_cleared() {
        let p = pool();
        let repo = UserStatusRepo::new(p.clone());
        let participant = fixture(&p).await;

        // Set a status that already expired (expiry in the past), away preference.
        let past = OffsetDateTime::now_utc() - time::Duration::hours(1);
        repo.set(
            participant,
            Some(":palm_tree:"),
            Some("On vacation"),
            "away",
            Some(past),
        )
        .await
        .unwrap();

        // Read reports the custom status cleared, presence preference kept.
        let got = repo.get(participant).await.unwrap().expect("row present");
        assert!(got.emoji.is_none(), "expired emoji cleared on read");
        assert!(got.text.is_none(), "expired text cleared on read");
        assert!(got.expires_at.is_none(), "expiry surfaced as cleared");
        assert_eq!(
            got.presence,
            Presence::Away,
            "presence preference kept past expiry"
        );

        // get_many applies the same clearing.
        let many = repo.get_many(&[participant]).await.unwrap();
        assert_eq!(many.len(), 1);
        assert!(many[0].emoji.is_none());
        assert_eq!(many[0].presence, Presence::Away);
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn user_status_get_many_empty_input_is_empty() {
        let p = pool();
        let repo = UserStatusRepo::new(p);
        assert!(repo.get_many(&[]).await.unwrap().is_empty());
    }
}
