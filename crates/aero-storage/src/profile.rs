//! Custom user-profile repository (per-participant side table).
//!
//! Backs `migrations/0035_participant_profiles.sql`. A participant fills in
//! optional profile metadata about themselves — title, pronouns, timezone,
//! phone, and a free-form status text. The data lives in a SIDE table keyed by
//! `participant_id`, so the core `participants` row (and its `update_me`
//! handler) is never touched: the profile row is created lazily on first
//! [`upsert`](ProfileRepo::upsert) and every field is nullable.
//!
//! Purely additive: a NEW [`ProfileRepo`]; no existing repo is touched. The
//! [`Profile`] model lives here (and is re-exported from the crate root) rather
//! than in `aero-common`, since it is a storage-layer projection — mirroring
//! [`SavedSearch`](crate::SavedSearch) and [`Draft`](crate::Draft).

use aero_common::ParticipantId;
use serde::Serialize;
use sqlx::PgPool;

/// One participant's custom profile fields.
///
/// A storage-layer projection of a `participant_profiles` row. Every field but
/// the id is optional, since the participant fills them in piecemeal.
/// `Serialize` so a handler can hand the row straight back as JSON; `updated_at`
/// renders as RFC 3339.
#[derive(Debug, Clone, Serialize)]
pub struct Profile {
    /// The participant the profile belongs to (and is keyed by).
    pub participant_id: ParticipantId,
    /// Job title / role (e.g. "Staff Engineer"), if set.
    pub title: Option<String>,
    /// Preferred pronouns (e.g. "she/her"), if set.
    pub pronouns: Option<String>,
    /// IANA timezone name (e.g. `America/New_York`), if set.
    pub timezone: Option<String>,
    /// Contact phone number, if set.
    pub phone: Option<String>,
    /// Free-form status text (e.g. "Out of office"), if set.
    pub status_text: Option<String>,
    /// Emoji shorthand for the custom status (e.g. ":palm_tree:"), if set.
    /// Added by migration 0121.
    pub status_emoji: Option<String>,
    /// Optional auto-expiry for the custom status. `None` means never expires.
    /// Added by migration 0121.
    #[serde(with = "time::serde::rfc3339::option")]
    pub status_expires_at: Option<time::OffsetDateTime>,
    /// When the profile was last upserted (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: time::OffsetDateTime,
}

/// The columns a [`Profile`] is built from, in select order. Shared by every
/// query so the row decoding stays in one place.
const COLUMNS: &str = "participant_id, title, pronouns, timezone, phone, status_text, \
                        status_emoji, status_expires_at, updated_at";

type Row = (
    uuid::Uuid,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<time::OffsetDateTime>,
    time::OffsetDateTime,
);

fn row_to_model(r: Row) -> Profile {
    let (participant_id, title, pronouns, timezone, phone, status_text,
         status_emoji, status_expires_at, updated_at) = r;
    Profile {
        participant_id: ParticipantId::from_uuid(participant_id),
        title,
        pronouns,
        timezone,
        phone,
        status_text,
        status_emoji,
        status_expires_at,
        updated_at,
    }
}

/// Repository over the `participant_profiles` table (per-participant custom
/// profile fields).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`ProfileRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct ProfileRepo {
    pool: PgPool,
}

impl ProfileRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create or replace `participant`'s profile, stamping `updated_at = now()`.
    /// Every field is optional; a `None` clears the column. The caller is
    /// responsible for per-field length validation.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the upsert.
    pub async fn upsert(
        &self,
        participant: ParticipantId,
        title: Option<&str>,
        pronouns: Option<&str>,
        timezone: Option<&str>,
        phone: Option<&str>,
        status_text: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO participant_profiles
                  (participant_id, title, pronouns, timezone, phone, status_text)
               VALUES ($1, $2, $3, $4, $5, $6)
               ON CONFLICT (participant_id) DO UPDATE SET
                   title       = EXCLUDED.title,
                   pronouns    = EXCLUDED.pronouns,
                   timezone    = EXCLUDED.timezone,
                   phone       = EXCLUDED.phone,
                   status_text = EXCLUDED.status_text,
                   updated_at  = now()",
        )
        .bind(participant.to_uuid())
        .bind(title)
        .bind(pronouns)
        .bind(timezone)
        .bind(phone)
        .bind(status_text)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Update only the core profile fields (title, pronouns, timezone, phone,
    /// status_text) without touching the emoji / expires_at status columns that
    /// [`set_status`](Self::set_status) manages. Preserves the new columns on
    /// conflict so a `PUT /api/me/profile` call does not accidentally wipe a
    /// status set via `PATCH /api/me/profile/status`.
    pub async fn upsert_core(
        &self,
        participant: ParticipantId,
        title: Option<&str>,
        pronouns: Option<&str>,
        timezone: Option<&str>,
        phone: Option<&str>,
        status_text: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        // Identical SQL to `upsert` — both only touch the columns in the INSERT
        // list; the new status_emoji / status_expires_at columns are left at
        // their current values by the ON CONFLICT DO UPDATE because they are not
        // in EXCLUDED.
        self.upsert(participant, title, pronouns, timezone, phone, status_text).await
    }

    /// Update the custom status fields (`status_text`, `status_emoji`,
    /// `status_expires_at`) on `participant`'s profile row, creating it if it
    /// does not yet exist. Touches only the three status columns so the other
    /// profile fields (title, pronouns, etc.) are preserved.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the upsert.
    pub async fn set_status(
        &self,
        participant: ParticipantId,
        text: Option<&str>,
        emoji: Option<&str>,
        expires_at: Option<time::OffsetDateTime>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO participant_profiles
                  (participant_id, status_text, status_emoji, status_expires_at)
               VALUES ($1, $2, $3, $4)
               ON CONFLICT (participant_id) DO UPDATE SET
                   status_text       = EXCLUDED.status_text,
                   status_emoji      = EXCLUDED.status_emoji,
                   status_expires_at = EXCLUDED.status_expires_at,
                   updated_at        = now()",
        )
        .bind(participant.to_uuid())
        .bind(text)
        .bind(emoji)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetch `participant`'s profile, or `None` if they have never set one.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(&self, participant: ParticipantId) -> Result<Option<Profile>, sqlx::Error> {
        let sql =
            format!("SELECT {COLUMNS} FROM participant_profiles WHERE participant_id = $1");
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(participant.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored profile
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a throwaway participant so the test is self-contained.
    async fn participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("profile-owner-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn profile_upsert_get_then_overwrite() {
        let p = pool();
        let repo = ProfileRepo::new(p.clone());
        let owner = participant(&p).await;

        // No profile yet.
        assert!(
            repo.get(owner).await.unwrap().is_none(),
            "no profile before first upsert"
        );

        // First upsert → get returns the fields.
        repo.upsert(
            owner,
            Some("Staff Engineer"),
            Some("she/her"),
            Some("America/New_York"),
            Some("+1-555-0100"),
            Some("Building things"),
        )
        .await
        .unwrap();
        let got = repo.get(owner).await.unwrap().expect("profile present");
        assert_eq!(got.participant_id, owner);
        assert_eq!(got.title.as_deref(), Some("Staff Engineer"));
        assert_eq!(got.pronouns.as_deref(), Some("she/her"));
        assert_eq!(got.timezone.as_deref(), Some("America/New_York"));
        assert_eq!(got.phone.as_deref(), Some("+1-555-0100"));
        assert_eq!(got.status_text.as_deref(), Some("Building things"));

        // Second upsert overwrites — including clearing a field to NULL.
        repo.upsert(owner, Some("Principal Engineer"), None, None, None, None)
            .await
            .unwrap();
        let got = repo.get(owner).await.unwrap().expect("profile present");
        assert_eq!(got.title.as_deref(), Some("Principal Engineer"));
        assert_eq!(got.pronouns, None, "second upsert clears pronouns");
        assert_eq!(got.timezone, None);
        assert_eq!(got.phone, None);
        assert_eq!(got.status_text, None);

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM participant_profiles WHERE participant_id = $1")
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
