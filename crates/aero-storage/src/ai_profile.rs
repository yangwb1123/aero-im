//! Persistent cross-room AI user-profile repository (持久跨房 AI 用户画像).
//!
//! Backs `migrations/0145_ai_persona.sql`. One optional row per participant
//! (keyed on `participant_id`), holding a durable, cross-room view the AI
//! answer / recommendation paths can read to personalise a reply:
//!
//!   * `topics` — a small set of recurring topics (JSON array of strings),
//!   * `preferences` — stated preference hints (JSON object),
//!   * `summary` — a short human-readable digest of the participant.
//!
//! ## Privacy posture (隐私第一)
//!
//! This is a PRIVACY-SENSITIVE store, so the whole feature is gated:
//!
//!   * **Opt-in** — nothing is ever written unless the operator sets
//!     `AERO_AI_CROSS_ROOM_PROFILE` (default OFF). The extraction entrypoint in
//!     `aero-ai` short-circuits otherwise, so the table stays empty on a fresh
//!     deploy. This repo only persists what that gated entrypoint hands it.
//!   * **GDPR-erasable** — the row is keyed by `participant_id` and is DELETEd
//!     explicitly by `ParticipantRepo::delete_participant`. The FK is
//!     `ON DELETE CASCADE`, but erasure TOMBSTONES the participant (an UPDATE),
//!     so the cascade never fires — the explicit DELETE is load-bearing. This
//!     [`AiProfileRepo::delete`] is exactly that hook (and is also callable on
//!     its own for a targeted profile reset).
//!   * **Transparent** — every field is plain readable data (not an opaque
//!     vector), so the subject can be shown exactly what is stored about them.
//!
//! Purely additive: a NEW [`AiProfileRepo`]; no existing repo is touched.

use aero_common::{ParticipantId, WorkspaceId};
use serde_json::Value;
use sqlx::PgPool;
use time::OffsetDateTime;

/// A durable cross-room AI profile for one participant.
///
/// `topics` is a JSON array of short topic strings; `preferences` a JSON object
/// of free-form key/value hints; `summary` a short human-readable digest. All
/// three are plain readable data (transparency) and default empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiProfile {
    pub participant_id: ParticipantId,
    pub workspace_id: Option<WorkspaceId>,
    pub topics: Value,
    pub preferences: Value,
    pub summary: String,
    pub updated_at: OffsetDateTime,
}

/// Row shape returned by the profile queries (column order matches the SELECTs).
type ProfileRow = (uuid::Uuid, Option<uuid::Uuid>, Value, Value, String, OffsetDateTime);

fn row_to_profile(row: ProfileRow) -> AiProfile {
    let (participant_id, workspace_id, topics, preferences, summary, updated_at) = row;
    AiProfile {
        participant_id: ParticipantId::from_uuid(participant_id),
        workspace_id: workspace_id.map(WorkspaceId::from_uuid),
        topics,
        preferences,
        summary,
        updated_at,
    }
}

#[derive(Clone)]
pub struct AiProfileRepo {
    pool: PgPool,
}

impl AiProfileRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Upsert `participant`'s profile. Idempotent and keyed on the participant:
    /// re-running overwrites the previous profile and bumps `updated_at`.
    ///
    /// `topics` should be a JSON array, `preferences` a JSON object — both are
    /// stored as-is (callers normalise before persisting). Returns the stored
    /// profile.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the upsert (e.g. the participant FK).
    pub async fn upsert(
        &self,
        participant: ParticipantId,
        workspace: Option<WorkspaceId>,
        topics: &Value,
        preferences: &Value,
        summary: &str,
    ) -> Result<AiProfile, sqlx::Error> {
        let row = sqlx::query_as::<_, ProfileRow>(
            r"INSERT INTO participant_ai_profiles
                  (participant_id, workspace_id, topics, preferences, summary, updated_at)
               VALUES ($1, $2, $3, $4, $5, now())
               ON CONFLICT (participant_id)
               DO UPDATE SET workspace_id = EXCLUDED.workspace_id,
                             topics       = EXCLUDED.topics,
                             preferences  = EXCLUDED.preferences,
                             summary      = EXCLUDED.summary,
                             updated_at   = now()
               RETURNING participant_id, workspace_id, topics, preferences, summary, updated_at",
        )
        .bind(participant.to_uuid())
        .bind(workspace.map(|w| w.to_uuid()))
        .bind(topics)
        .bind(preferences)
        .bind(summary)
        .fetch_one(&self.pool)
        .await?;
        Ok(row_to_profile(row))
    }

    /// `participant`'s profile, or `None` when none has been extracted (the
    /// default state — the feature is opt-in, so most participants have no row).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(
        &self,
        participant: ParticipantId,
    ) -> Result<Option<AiProfile>, sqlx::Error> {
        let row = sqlx::query_as::<_, ProfileRow>(
            r"SELECT participant_id, workspace_id, topics, preferences, summary, updated_at
               FROM participant_ai_profiles
               WHERE participant_id = $1",
        )
        .bind(participant.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_profile))
    }

    /// Delete `participant`'s profile (GDPR right-to-erasure). Returns `true`
    /// when a row was removed; idempotent (deleting an absent profile is a no-op
    /// `false`).
    ///
    /// This is the explicit erasure hook wired into
    /// `ParticipantRepo::delete_participant`: the FK cascade does NOT fire on
    /// erasure (which tombstones the participant via UPDATE rather than
    /// hard-deleting it), so this DELETE is what actually removes the profile.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn delete(&self, participant: ParticipantId) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM participant_ai_profiles WHERE participant_id = $1")
            .bind(participant.to_uuid())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored ai_profile_
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::{ParticipantId, WorkspaceId};

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
            .bind(format!("ai-profile-participant-{participant}"))
            .execute(p)
            .await
            .expect("insert participant");
        participant
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn ai_profile_upsert_get_delete_roundtrip() {
        let p = pool();
        let repo = AiProfileRepo::new(p.clone());
        let participant = fixture(&p).await;
        let ws = WorkspaceId::new();

        assert!(repo.get(participant).await.unwrap().is_none(), "no profile initially");

        let topics = serde_json::json!(["rust", "postgres"]);
        let prefs = serde_json::json!({ "tone": "concise" });
        let set = repo
            .upsert(participant, Some(ws), &topics, &prefs, "Engineer who asks about rust + pg.")
            .await
            .unwrap();
        assert_eq!(set.topics, topics);
        assert_eq!(set.preferences, prefs);
        assert_eq!(set.workspace_id, Some(ws));

        let got = repo.get(participant).await.unwrap().expect("profile present");
        assert_eq!(got.topics, topics);
        assert_eq!(got.summary, "Engineer who asks about rust + pg.");

        // Re-upsert is an idempotent overwrite.
        let topics2 = serde_json::json!(["oncall"]);
        let reset = repo
            .upsert(participant, None, &topics2, &serde_json::json!({}), "Updated.")
            .await
            .unwrap();
        assert_eq!(reset.topics, topics2);
        assert!(reset.workspace_id.is_none(), "workspace overwritten to NULL");
        assert_eq!(reset.summary, "Updated.");

        assert!(repo.delete(participant).await.unwrap(), "delete removed it");
        assert!(!repo.delete(participant).await.unwrap(), "second delete is a no-op");
        assert!(repo.get(participant).await.unwrap().is_none(), "deleted");
    }
}
