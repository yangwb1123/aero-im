//! Atomic first-party account registration.
//!
//! The public registration journey creates more than a credential: the new
//! participant must belong to the default workspace and its curated channels,
//! and the refresh token returned to the client must already exist in the active
//! session inventory. This repository commits those rows together so a failure
//! never leaves an email occupied by a half-registered account.

use aero_common::{Participant, ParticipantId, ParticipantKind, SessionId, WorkspaceId};
use sqlx::PgPool;

use crate::audit_governance::AuditGovernanceOutboxRepo;
use crate::AuditRepo;

/// Governance pair spec for a registration (B5-1 auth slice). When `Some`,
/// [`RegistrationRepo::create`] REPLACES the legacy audit-only append with the
/// fail-open pair writer (exactly 1 audit row + 1 outbox row, same tx — never
/// double-audited). `detail` is `{}` (D-N1: PII reduction — the email leaves
/// the audit trail on the pair path; no in-repo consumers of the register
/// audit detail exist).
#[derive(Debug, Clone)]
pub struct NewRegistrationAudit {
    /// Local token: `auth.register`.
    pub action: &'static str,
    /// New participant id (the audit target).
    pub target: String,
    /// Envelope `payload` (must be a JSON object — the pair writer's contract
    /// pre-check rejects non-objects).
    pub detail: serde_json::Value,
    /// Outbound token: `admin.auth.register`.
    pub outbound_action: &'static str,
}

#[derive(Debug, Clone)]
pub struct NewRegistration {
    pub participant_id: ParticipantId,
    pub email: String,
    pub display_name: String,
    pub password_hash: String,
    pub workspace_id: WorkspaceId,
    pub session_id: SessionId,
    pub refresh_token_hash: String,
    pub user_agent: Option<String>,
    /// Governance pair spec — `None` (SSO/OIDC JIT etc.) keeps the legacy
    /// audit-only `auth.register` append (D-N4); `Some` (first-party
    /// `register_enrolled`) writes the 1:1 pair.
    pub auth_audit: Option<NewRegistrationAudit>,
}

#[derive(Clone)]
#[must_use]
pub struct RegistrationRepo {
    pool: PgPool,
}

impl RegistrationRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Commit the account, tenant enrollment, default-channel memberships, and
    /// initial refresh-session inventory in one transaction.
    pub async fn create(&self, new: NewRegistration) -> Result<Participant, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let created_at = time::OffsetDateTime::now_utc();
        let participant_id = new.participant_id.to_uuid();

        sqlx::query(
            r"INSERT INTO participants
                  (id, kind, display_name, avatar_url, created_by, created_at)
              VALUES ($1, 'human', $2, NULL, NULL, $3)",
        )
        .bind(participant_id)
        .bind(&new.display_name)
        .bind(created_at)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r"INSERT INTO credentials
                  (participant_id, email, password_hash, created_at)
              VALUES ($1, $2::citext, $3, $4)",
        )
        .bind(participant_id)
        .bind(new.email.trim())
        .bind(&new.password_hash)
        .bind(created_at)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r"INSERT INTO workspace_members
                  (workspace_id, participant_id, role, joined_at)
              VALUES ($1, $2, 'member', $3)",
        )
        .bind(new.workspace_id.to_uuid())
        .bind(participant_id)
        .bind(created_at)
        .execute(&mut *tx)
        .await?;

        // Snapshot the curated default-channel set inside the transaction. A
        // concurrently-added default is handled by the normal member-enrollment
        // workflow; every channel visible here is joined atomically with signup.
        sqlx::query(
            r"INSERT INTO room_members (room_id, participant_id, role, joined_at)
              SELECT defaults.room_id, $2, 'member', $3
                FROM workspace_default_channels defaults
                JOIN rooms r
                  ON r.id = defaults.room_id
                 AND r.workspace_id = defaults.workspace_id
                 AND r.kind = 'channel'
               WHERE defaults.workspace_id = $1
              ON CONFLICT (room_id, participant_id) DO NOTHING",
        )
        .bind(new.workspace_id.to_uuid())
        .bind(participant_id)
        .bind(created_at)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r"INSERT INTO auth_sessions
                  (id, participant_id, token_hash, user_agent, created_at, last_seen_at)
              VALUES ($1, $2, $3, $4, $5, $5)",
        )
        .bind(new.session_id.to_uuid())
        .bind(participant_id)
        .bind(&new.refresh_token_hash)
        .bind(new.user_agent.as_deref())
        .bind(created_at)
        .execute(&mut *tx)
        .await?;

        // Security-event audit (auth slice direction): the account-creation
        // event rides the SAME transaction — same-fate. When `auth_audit` is
        // `Some`, the governance PAIR writer replaces the legacy append:
        // exactly 1 audit row + 1 outbox row (SAVEPOINT fail-open — `Ok(None)`
        // continues silently, `Err` propagates). When `None` (SSO/OIDC JIT,
        // db_tests), the legacy audit-only append stays (D-N4).
        match new.auth_audit {
            Some(audit) => {
                let _ = AuditGovernanceOutboxRepo::append_pair_in_tx_fail_open(
                    &mut tx,
                    new.workspace_id,
                    Some(new.participant_id),
                    audit.action,
                    Some(&audit.target),
                    audit.detail,
                    audit.outbound_action,
                )
                .await?; // Ok(None) = fail-open skip (DLQ row in-tx)
            }
            None => {
                AuditRepo::append_in_tx(
                    &mut tx,
                    new.workspace_id,
                    Some(new.participant_id),
                    "auth.register",
                    Some(&new.participant_id.to_string()),
                    serde_json::json!({
                        "email": new.email.trim(),
                        "user_agent": new.user_agent,
                    }),
                )
                .await?;
            }
        }

        tx.commit().await?;
        Ok(Participant {
            id: new.participant_id,
            kind: ParticipantKind::Human,
            display_name: new.display_name,
            avatar_url: None,
            created_by: None,
            created_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_repo_is_clone() {
        fn assert_clone<T: Clone>() {}
        assert_clone::<RegistrationRepo>();
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::{DmRepo, RoomRepo, WorkspaceRepo};
    use aero_common::RoomKind;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("valid DATABASE_URL")
    }

    #[tokio::test]
    #[ignore = "requires live Postgres with migrations"]
    async fn missing_workspace_rolls_back_every_registration_row() {
        let pool = pool();
        let repo = RegistrationRepo::new(pool.clone());
        let participant = ParticipantId::new();
        let session = SessionId::new();
        let email = format!("atomic-register-{participant}@example.test");
        let result = repo
            .create(NewRegistration {
                participant_id: participant,
                email: email.clone(),
                display_name: "Atomic Register".into(),
                password_hash: "test-hash".into(),
                workspace_id: WorkspaceId::new(),
                session_id: session,
                refresh_token_hash: format!("refresh-{session}"),
                user_agent: Some("registration-test".into()),
                auth_audit: None,
            })
            .await;
        assert!(
            result.is_err(),
            "foreign-key failure must abort registration"
        );

        let participant_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM participants WHERE id = $1")
                .bind(participant.to_uuid())
                .fetch_one(&pool)
                .await
                .unwrap();
        let credential_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM credentials WHERE email = $1::citext")
                .bind(&email)
                .fetch_one(&pool)
                .await
                .unwrap();
        let session_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM auth_sessions WHERE id = $1")
                .bind(session.to_uuid())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            (participant_count, credential_count, session_count),
            (0, 0, 0)
        );
        // Same-fate audit (AC1 negative): the forced FK failure must also leave
        // zero `auth.register` audit rows for the attempted id — no orphan
        // audit without an account. Scoped by actor (the shared-DB suite runs
        // other registration tests that legitimately emit register rows).
        let audit_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM audit_events WHERE action = 'auth.register' AND actor_id = $1",
        )
        .bind(participant.to_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            audit_count, 0,
            "a failed registration must not leave an orphan auth.register audit row"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres with migrations"]
    async fn successful_registration_audits_auth_register_once() {
        let pool = pool();
        let repo = RegistrationRepo::new(pool.clone());
        let participant = ParticipantId::new();
        let session = SessionId::new();
        let email = format!("audited-register-{participant}@example.test");
        repo.create(NewRegistration {
            participant_id: participant,
            email: email.clone(),
            display_name: "Audited Register".into(),
            password_hash: "test-hash".into(),
            workspace_id: WorkspaceId::nil(),
            session_id: session,
            refresh_token_hash: format!("refresh-{session}"),
            user_agent: Some("registration-test".into()),
            auth_audit: None,
        })
        .await
        .expect("registration commits");

        // Exactly one `auth.register` row, workspace/actor/target/detail per R1.
        let row: (String, String, String, String, Option<String>, String) = sqlx::query_as(
            "SELECT workspace_id::text, actor_id::text, target, detail->>'email', \
                    detail->>'user_agent', action
               FROM audit_events
              WHERE action = 'auth.register' AND actor_id = $1",
        )
        .bind(participant.to_uuid())
        .fetch_one(&pool)
        .await
        .expect("exactly one auth.register row");
        assert_eq!(
            row.0,
            WorkspaceId::nil().to_uuid().to_string(),
            "workspace = the registration workspace (nil UUID)"
        );
        assert_eq!(
            row.1,
            participant.to_uuid().to_string(),
            "actor = the new participant"
        );
        assert_eq!(row.2, participant.to_string(), "target = the new participant");
        assert_eq!(row.3, email, "detail.email = the registration email");
        assert_eq!(row.4.as_deref(), Some("registration-test"), "detail.user_agent");
        assert_eq!(row.5, "auth.register");
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM audit_events WHERE action = 'auth.register' AND actor_id = $1",
        )
        .bind(participant.to_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(total, 1, "exactly one audit row per registration");

        // Cleanup (FK NO ACTION, 0007): audit rows reference the participant.
        sqlx::query("DELETE FROM audit_events WHERE actor_id = $1")
            .bind(participant.to_uuid())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(participant.to_uuid())
            .execute(&pool)
            .await
            .unwrap();
    }

    async fn participant(pool: &PgPool, label: &str) -> ParticipantId {
        let participant = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(participant.to_uuid())
            .bind(format!("registration-kind-{label}-{participant}"))
            .execute(pool)
            .await
            .unwrap();
        participant
    }

    #[tokio::test]
    #[ignore = "requires live Postgres with migrations"]
    async fn historical_non_channel_defaults_cannot_enroll_a_registration() {
        let pool = pool();
        let owner = participant(&pool, "owner").await;
        let peer = participant(&pool, "peer").await;
        let workspaces = WorkspaceRepo::new(pool.clone());
        let workspace = workspaces
            .create(
                "Registration kind containment".into(),
                format!("registration-kind-{owner}"),
                owner,
            )
            .await
            .unwrap()
            .id;
        workspaces
            .add_member(workspace, peer, aero_common::WorkspaceRole::Member)
            .await
            .unwrap();
        let group = RoomRepo::new(pool.clone())
            .create_in_workspace(
                workspace,
                RoomKind::Group,
                Some("dirty default group".into()),
                owner,
            )
            .await
            .unwrap()
            .id;
        let direct = DmRepo::new(pool.clone())
            .find_or_create_in_workspace(workspace, owner, peer)
            .await
            .unwrap()
            .id;
        for room in [group, direct] {
            sqlx::query(
                r"INSERT INTO workspace_default_channels (workspace_id, room_id)
                   VALUES ($1, $2)",
            )
            .bind(workspace.to_uuid())
            .bind(room.to_uuid())
            .execute(&pool)
            .await
            .unwrap();
        }

        let participant = ParticipantId::new();
        let session = SessionId::new();
        RegistrationRepo::new(pool.clone())
            .create(NewRegistration {
                participant_id: participant,
                email: format!("registration-kind-{participant}@example.test"),
                display_name: "Kind-contained registration".into(),
                password_hash: "test-hash".into(),
                workspace_id: workspace,
                session_id: session,
                refresh_token_hash: format!("refresh-{session}"),
                user_agent: None,
                auth_audit: None,
            })
            .await
            .unwrap();

        for room in [group, direct] {
            let joined: bool = sqlx::query_scalar(
                r"SELECT EXISTS(
                       SELECT 1 FROM room_members
                        WHERE room_id = $1 AND participant_id = $2
                   )",
            )
            .bind(room.to_uuid())
            .bind(participant.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(!joined, "registration must not join non-channel default");
        }
    }
}
