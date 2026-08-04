//! Persistent cross-room AI user-profile repository (持久跨房 AI 用户画像).
//!
//! Backs `migrations/0145_ai_persona.sql` plus the tenant-boundary correction in
//! `0173_ai_profiles_workspace_scope.sql`. The tenant-aware table is named
//! `participant_ai_profiles_scoped`; the legacy relation name is a deliberately
//! empty rolling-upgrade view so a pre-0173 binary fails closed instead of
//! reading an arbitrary tenant. One optional row per `(participant, workspace)`
//! holds a durable, cross-room view the AI answer /
//! recommendation paths can read to personalise a reply:
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
//!   * **GDPR-erasable** — rows are keyed by `participant_id` and are deleted
//!     explicitly by `ParticipantRepo::delete_participant`. The FK is
//!     `ON DELETE CASCADE`, but erasure TOMBSTONES the participant (an UPDATE),
//!     so the cascade never fires — the explicit DELETE is load-bearing. This
//!     [`AiProfileRepo::delete`] removes every workspace profile for the subject.
//!   * **Tenant-isolated** — workspace is part of the primary key and every read
//!     binds it; a profile derived in one tenant can never enter another tenant's
//!     prompt. The workspace FK cascades workspace deletion.
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
    pub workspace_id: WorkspaceId,
    pub topics: Value,
    pub preferences: Value,
    pub summary: String,
    pub updated_at: OffsetDateTime,
}

/// Row shape returned by the profile queries (column order matches the SELECTs).
type ProfileRow = (uuid::Uuid, uuid::Uuid, Value, Value, String, OffsetDateTime);

fn row_to_profile(row: ProfileRow) -> AiProfile {
    let (participant_id, workspace_id, topics, preferences, summary, updated_at) = row;
    AiProfile {
        participant_id: ParticipantId::from_uuid(participant_id),
        workspace_id: WorkspaceId::from_uuid(workspace_id),
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

    /// Upsert `participant`'s profile inside `workspace`. Idempotent and keyed on
    /// both ids: re-running overwrites only that tenant's profile and bumps
    /// `updated_at`.
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
        workspace: WorkspaceId,
        topics: &Value,
        preferences: &Value,
        summary: &str,
    ) -> Result<AiProfile, sqlx::Error> {
        let row = sqlx::query_as::<_, ProfileRow>(
            r"INSERT INTO participant_ai_profiles_scoped
                  (participant_id, workspace_id, topics, preferences, summary, updated_at)
               VALUES ($1, $2, $3, $4, $5, now())
               ON CONFLICT (participant_id, workspace_id)
               DO UPDATE SET topics       = EXCLUDED.topics,
                             preferences  = EXCLUDED.preferences,
                             summary      = EXCLUDED.summary,
                             updated_at   = now()
               RETURNING participant_id, workspace_id, topics, preferences, summary, updated_at",
        )
        .bind(participant.to_uuid())
        .bind(workspace.to_uuid())
        .bind(topics)
        .bind(preferences)
        .bind(summary)
        .fetch_one(&self.pool)
        .await?;
        Ok(row_to_profile(row))
    }

    /// `participant`'s profile for `workspace`, or `None` when none has been
    /// extracted or the participant no longer has effective workspace access.
    /// Both keys are mandatory so callers cannot accidentally reuse a profile
    /// across tenants. The access joins are deliberately repeated here as a
    /// defence-in-depth read gate: callers cannot resurrect personalization for
    /// a deleted/deactivated account or one locked out by mandatory 2FA merely
    /// by bypassing the HTTP route.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
    ) -> Result<Option<AiProfile>, sqlx::Error> {
        let row = sqlx::query_as::<_, ProfileRow>(
            r"SELECT profile.participant_id, profile.workspace_id, profile.topics,
                     profile.preferences, profile.summary, profile.updated_at
                FROM participant_ai_profiles_scoped profile
                JOIN workspace_members membership
                  ON membership.workspace_id = profile.workspace_id
                 AND membership.participant_id = profile.participant_id
                JOIN workspaces workspace
                  ON workspace.id = profile.workspace_id
                JOIN participants participant
                  ON participant.id = profile.participant_id
                 AND participant.deleted_at IS NULL
                LEFT JOIN workspace_deactivations deactivated
                  ON deactivated.workspace_id = profile.workspace_id
                 AND deactivated.participant_id = profile.participant_id
                LEFT JOIN totp_secrets totp
                  ON totp.participant_id = profile.participant_id
               WHERE profile.participant_id = $1
                 AND profile.workspace_id = $2
                 AND deactivated.participant_id IS NULL
                 AND (
                     participant.kind <> 'human'
                     OR NOT workspace.require_2fa
                     OR COALESCE(totp.activated, false)
                 )",
        )
        .bind(participant.to_uuid())
        .bind(workspace.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_profile))
    }

    /// Delete every profile for `participant` (GDPR right-to-erasure). Returns
    /// `true` when at least one row was removed; idempotent.
    ///
    /// This is the explicit erasure hook wired into
    /// `ParticipantRepo::delete_participant`: the FK cascade does NOT fire on
    /// erasure (which tombstones the participant via UPDATE rather than
    /// hard-deleting it), so this DELETE is what actually removes the profile.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn delete(&self, participant: ParticipantId) -> Result<bool, sqlx::Error> {
        let result =
            sqlx::query("DELETE FROM participant_ai_profiles_scoped WHERE participant_id = $1")
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

    // Create a throwaway participant and two workspaces so both FKs are
    // satisfied and tenant isolation can be exercised.
    async fn fixture(p: &PgPool) -> (ParticipantId, WorkspaceId, WorkspaceId) {
        let participant = ParticipantId::new();
        let owner = ParticipantId::new();
        for (id, label) in [(participant, "participant"), (owner, "owner")] {
            sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
                .bind(id.to_uuid())
                .bind(format!("ai-profile-{label}-{id}"))
                .execute(p)
                .await
                .expect("insert participant");
        }
        let first = WorkspaceId::new();
        let second = WorkspaceId::new();
        for (workspace, suffix) in [(first, "first"), (second, "second")] {
            let mut tx = p.begin().await.expect("begin workspace fixture");
            sqlx::query("INSERT INTO workspaces (id, name, slug, created_by) VALUES ($1,$2,$3,$4)")
                .bind(workspace.to_uuid())
                .bind(format!("AI profile {suffix}"))
                .bind(format!("ai-profile-{suffix}-{workspace}"))
                .bind(participant.to_uuid())
                .execute(&mut *tx)
                .await
                .expect("insert workspace");
            sqlx::query(
                "INSERT INTO workspace_members (workspace_id, participant_id, role)
                 VALUES ($1, $2, 'owner'), ($1, $3, 'member')",
            )
            .bind(workspace.to_uuid())
            .bind(owner.to_uuid())
            .bind(participant.to_uuid())
            .execute(&mut *tx)
            .await
            .expect("insert workspace membership");
            tx.commit().await.expect("commit workspace fixture");
        }
        (participant, first, second)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn ai_profile_upsert_get_delete_roundtrip() {
        let p = pool();
        let repo = AiProfileRepo::new(p.clone());
        let (participant, ws, other_ws) = fixture(&p).await;

        assert!(
            repo.get(participant, ws).await.unwrap().is_none(),
            "no profile initially"
        );

        let topics = serde_json::json!(["rust", "postgres"]);
        let prefs = serde_json::json!({ "tone": "concise" });
        let set = repo
            .upsert(
                participant,
                ws,
                &topics,
                &prefs,
                "Engineer who asks about rust + pg.",
            )
            .await
            .unwrap();
        assert_eq!(set.topics, topics);
        assert_eq!(set.preferences, prefs);
        assert_eq!(set.workspace_id, ws);

        let got = repo
            .get(participant, ws)
            .await
            .unwrap()
            .expect("profile present");
        assert_eq!(got.topics, topics);
        assert_eq!(got.summary, "Engineer who asks about rust + pg.");
        assert!(
            repo.get(participant, other_ws).await.unwrap().is_none(),
            "a different workspace cannot read this profile"
        );

        let other = repo
            .upsert(
                participant,
                other_ws,
                &serde_json::json!(["finance"]),
                &serde_json::json!({}),
                "Other tenant.",
            )
            .await
            .unwrap();
        assert_eq!(other.workspace_id, other_ws);
        assert_eq!(
            repo.get(participant, ws)
                .await
                .unwrap()
                .expect("first tenant profile retained")
                .summary,
            "Engineer who asks about rust + pg."
        );
        sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(other_ws.to_uuid())
            .execute(&p)
            .await
            .unwrap();
        assert!(
            repo.get(participant, other_ws).await.unwrap().is_none(),
            "workspace deletion cascades only that tenant's derived profile"
        );

        // Re-upsert is an idempotent overwrite within the selected workspace.
        let topics2 = serde_json::json!(["oncall"]);
        let reset = repo
            .upsert(
                participant,
                ws,
                &topics2,
                &serde_json::json!({}),
                "Updated.",
            )
            .await
            .unwrap();
        assert_eq!(reset.topics, topics2);
        assert_eq!(reset.workspace_id, ws);
        assert_eq!(reset.summary, "Updated.");

        assert!(repo.delete(participant).await.unwrap(), "delete removed it");
        assert!(
            !repo.delete(participant).await.unwrap(),
            "second delete is a no-op"
        );
        assert!(
            repo.get(participant, ws).await.unwrap().is_none(),
            "first tenant profile deleted"
        );
        assert!(
            repo.get(participant, other_ws).await.unwrap().is_none(),
            "second tenant profile deleted"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn ai_profile_legacy_relation_fails_closed_during_rolling_upgrade() {
        let p = pool();
        let repo = AiProfileRepo::new(p.clone());
        let (participant, workspace, other_workspace) = fixture(&p).await;
        repo.upsert(
            participant,
            workspace,
            &serde_json::json!(["tenant-a"]),
            &serde_json::json!({}),
            "Tenant A.",
        )
        .await
        .unwrap();
        repo.upsert(
            participant,
            other_workspace,
            &serde_json::json!(["tenant-b"]),
            &serde_json::json!({}),
            "Tenant B.",
        )
        .await
        .unwrap();

        let legacy_rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM participant_ai_profiles WHERE participant_id = $1",
        )
        .bind(participant.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(
            legacy_rows, 0,
            "a pre-tenant binary must see no arbitrary workspace profile"
        );

        let legacy_upsert = sqlx::query(
            r"INSERT INTO participant_ai_profiles
                  (participant_id, workspace_id, topics, preferences, summary, updated_at)
               VALUES ($1, $2, '[]'::jsonb, '{}'::jsonb, 'legacy write', now())
               ON CONFLICT (participant_id)
               DO UPDATE SET summary = EXCLUDED.summary",
        )
        .bind(participant.to_uuid())
        .bind(workspace.to_uuid())
        .execute(&p)
        .await
        .expect_err("an old single-key upsert must fail closed");
        let legacy_database_error = legacy_upsert
            .as_database_error()
            .expect("legacy write is rejected by PostgreSQL");
        let legacy_sqlstate = legacy_database_error.code();
        assert!(
            matches!(
                legacy_sqlstate.as_deref(),
                Some("0A000" | "42P10" | "44000")
            ),
            "legacy write returns an observable, non-transient schema error: \
             SQLSTATE={legacy_sqlstate:?}, message={}",
            legacy_database_error.message()
        );

        let scoped_rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM participant_ai_profiles_scoped WHERE participant_id = $1",
        )
        .bind(participant.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(
            scoped_rows, 2,
            "the rejected old write leaves both tenant rows intact"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn ai_profile_read_requires_effective_workspace_access() {
        let p = pool();
        let repo = AiProfileRepo::new(p.clone());
        let (participant, workspace, _other_workspace) = fixture(&p).await;
        repo.upsert(
            participant,
            workspace,
            &serde_json::json!(["tenant-bound"]),
            &serde_json::json!({}),
            "Must remain behind effective access.",
        )
        .await
        .unwrap();
        assert!(
            repo.get(participant, workspace).await.unwrap().is_some(),
            "active member may read their profile"
        );

        sqlx::query(
            "INSERT INTO workspace_deactivations
                 (workspace_id, participant_id, deactivated_by)
             VALUES ($1, $2, $2)",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .execute(&p)
        .await
        .unwrap();
        assert!(
            repo.get(participant, workspace).await.unwrap().is_none(),
            "deactivation hides a durable profile even when the row remains"
        );
        sqlx::query(
            "DELETE FROM workspace_deactivations
              WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .execute(&p)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO totp_secrets
                 (participant_id, secret, activated, activated_at)
             SELECT participant_id, 'ai-profile-workspace-owner', true, now()
               FROM workspace_members
              WHERE workspace_id = $1 AND role = 'owner'",
        )
        .bind(workspace.to_uuid())
        .execute(&p)
        .await
        .unwrap();
        sqlx::query("UPDATE workspaces SET require_2fa = true WHERE id = $1")
            .bind(workspace.to_uuid())
            .execute(&p)
            .await
            .unwrap();
        assert!(
            repo.get(participant, workspace).await.unwrap().is_none(),
            "mandatory 2FA hides the profile from an unenrolled member"
        );
        sqlx::query(
            "INSERT INTO totp_secrets
                 (participant_id, secret, activated, activated_at)
             VALUES ($1, 'ai-profile-effective-access', true, now())",
        )
        .bind(participant.to_uuid())
        .execute(&p)
        .await
        .unwrap();
        assert!(
            repo.get(participant, workspace).await.unwrap().is_some(),
            "activated 2FA restores the defensive profile read"
        );

        sqlx::query("UPDATE participants SET deleted_at = now() WHERE id = $1")
            .bind(participant.to_uuid())
            .execute(&p)
            .await
            .unwrap();
        let retained: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM participant_ai_profiles_scoped WHERE participant_id = $1",
        )
        .bind(participant.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(
            retained, 0,
            "the database tombstone hook protects erasure requests served by an old pod"
        );
        assert!(
            repo.get(participant, workspace).await.unwrap().is_none(),
            "a tombstoned account has no remaining profile"
        );
    }
}
