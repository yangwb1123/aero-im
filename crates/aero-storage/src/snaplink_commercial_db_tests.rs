use aero_common::{Block, MessageId, ParticipantId, RoomId, WorkspaceId};

use crate::{
    AuditRepo, MessageRepo, NewMessage, ProjectionOutcome, SnaplinkBindingSpec,
    SnaplinkCommercialRepo, SnaplinkDeliveryDestination, SnaplinkEntitlementProjection,
    SnaplinkLimitProjection,
};

fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must select a migrated disposable database");
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect_lazy(&url)
        .expect("valid DATABASE_URL")
}

#[tokio::test]
#[ignore = "requires a migrated disposable PostgreSQL database"]
async fn message_quota_and_snaplink_outboxes_are_transactional() {
    let fixture = Fixture::new().await;
    let inserted = fixture.assert_quota_boundary().await;
    fixture.assert_outbox_payloads(inserted).await;
}

struct Fixture {
    pool: sqlx::PgPool,
    repo: SnaplinkCommercialRepo,
    workspace: WorkspaceId,
    source: String,
    actor: ParticipantId,
    room: RoomId,
    historical: MessageId,
    now: time::OffsetDateTime,
}

impl Fixture {
    async fn new() -> Self {
        let pool = pool();
        let repo = SnaplinkCommercialRepo::new(pool.clone());
        let workspace = WorkspaceId::new();
        let source = format!("aero-im-test-{}", uuid::Uuid::new_v4().simple());
        let actor = create_participant(&pool).await;
        create_workspace(&pool, workspace, actor).await;
        let room = create_room(&pool, workspace, actor).await;
        let mut historical_message = new_message(room, actor, "accepted before activation");
        historical_message.metadata = serde_json::json!({
            "source": "integration",
            "installation_id": uuid::Uuid::new_v4(),
        });
        let historical = MessageRepo::new(pool.clone())
            .insert(historical_message)
            .await
            .expect("pre-activation message")
            .id;
        let workspaces = list_workspaces(&pool).await;
        repo.configure_enabled(&desired_bindings(&workspaces, workspace, &source))
            .await
            .expect("activate complete desired state");
        assert_eq!(repo.reconcile_usage(10).await.unwrap(), 1);
        let now = time::OffsetDateTime::now_utc();
        project_entitlements(&repo, &workspaces, workspace, now).await;
        assert!(repo.ready().await.unwrap());
        Self {
            pool,
            repo,
            workspace,
            source,
            actor,
            room,
            historical,
            now,
        }
    }

    async fn assert_quota_boundary(&self) -> MessageId {
        let messages = MessageRepo::new(self.pool.clone());
        let inserted = messages
            .insert(new_message(
                self.room,
                self.actor,
                "counted after activation",
            ))
            .await
            .expect("second cumulative message at hard limit");
        let rejected = messages
            .insert(new_message(self.room, self.actor, "over quota"))
            .await
            .expect_err("third cumulative message must exceed hard limit");
        assert_eq!(
            rejected
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::constraint),
            Some("snaplink_quota_exceeded")
        );
        assert!(messages.get(self.historical).await.unwrap().is_some());
        assert!(messages.get(inserted.id).await.unwrap().is_some());
        inserted.id
    }

    async fn assert_outbox_payloads(&self, inserted: MessageId) {
        AuditRepo::new(self.pool.clone())
            .append(
                self.workspace,
                Some(self.actor),
                "commercial.test",
                Some(&inserted.to_string()),
                serde_json::json!({
                    "safe": true,
                    "token_hash_prefix": "must-not-leave-the-service",
                }),
            )
            .await
            .expect("local and governance audit enqueue");
        let claims = self
            .repo
            .claim_due(
                self.now + time::Duration::minutes(1),
                std::time::Duration::from_secs(30),
                10,
            )
            .await
            .expect("claim usage and audit");
        verify_claims(&claims, &self.source);
        self.assert_destination_rotation_fence(&claims).await;
    }

    async fn assert_destination_rotation_fence(&self, claims: &[crate::SnaplinkDeliveryClaim]) {
        let audit_client = format!("rotated-audit-client-{}", uuid::Uuid::new_v4().simple());
        let blocked = sqlx::query(
            "UPDATE snaplink_commercial_bindings SET audit_client_id = $2, revision = revision + 1 WHERE workspace_id = $1",
        )
        .bind(self.workspace.to_uuid())
        .bind(&audit_client)
        .execute(&self.pool)
        .await
        .expect_err("pending audit delivery must fence Audit client rotation");
        assert_eq!(
            blocked
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::constraint),
            Some("snaplink_binding_audit_rotation_pending")
        );
        for claim in claims {
            assert!(self.repo.mark_delivered(claim).await.unwrap());
        }
        sqlx::query(
            "UPDATE snaplink_commercial_bindings SET audit_client_id = $2, revision = revision + 1 WHERE workspace_id = $1",
        )
        .bind(self.workspace.to_uuid())
        .bind(audit_client)
        .execute(&self.pool)
        .await
        .expect("drained Audit client rotation");
    }
}

fn verify_claims(claims: &[crate::SnaplinkDeliveryClaim], source: &str) {
    assert_eq!(claims.len(), 3);
    let usage = claims
        .iter()
        .filter(|claim| claim.destination == SnaplinkDeliveryDestination::Usage)
        .collect::<Vec<_>>();
    assert_eq!(usage.len(), 2);
    for claim in &usage {
        assert_eq!(claim.payload["dimension"], "messages_per_month");
        assert_eq!(claim.payload["id"], claim.idempotency_key);
        assert!(claim.payload.get("tenant_id").is_none());
    }
    let audit = claims
        .iter()
        .find(|claim| claim.destination == SnaplinkDeliveryDestination::Audit)
        .expect("audit claim");
    assert_eq!(audit.payload["source_system"], source);
    assert_eq!(audit.payload["event_type"], "aero.im.security");
    assert_eq!(audit.payload["schema_id"], "aero.im.security");
    assert_eq!(audit.payload["schema_version"], 1);
    assert_eq!(audit.payload["payload"]["safe"], true);
    assert!(audit.payload["payload"].get("token_hash_prefix").is_none());
    assert!(audit.payload.get("tenant_id").is_none());
    assert!(usage
        .iter()
        .all(|claim| claim.client_id.starts_with("billing-client-")));
    assert!(audit.client_id.starts_with("audit-client-"));
}

async fn create_participant(pool: &sqlx::PgPool) -> ParticipantId {
    let actor = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(actor.to_uuid())
        .bind(format!("commercial-{actor}"))
        .execute(pool)
        .await
        .expect("participant");
    actor
}

async fn list_workspaces(pool: &sqlx::PgPool) -> Vec<WorkspaceId> {
    sqlx::query_scalar::<_, uuid::Uuid>("SELECT id FROM workspaces")
        .fetch_all(pool)
        .await
        .expect("list seed workspaces")
        .into_iter()
        .map(WorkspaceId::from_uuid)
        .collect()
}

fn desired_bindings(
    workspaces: &[WorkspaceId],
    target: WorkspaceId,
    target_source: &str,
) -> Vec<SnaplinkBindingSpec> {
    workspaces
        .iter()
        .map(|workspace| SnaplinkBindingSpec {
            workspace_id: *workspace,
            tenant_id: format!("tenant-{workspace}"),
            billing_client_id: format!("billing-client-{workspace}"),
            audit_client_id: format!("audit-client-{workspace}"),
            source_system: if *workspace == target {
                target_source.to_owned()
            } else {
                format!("aero-im-test-{}", workspace.to_uuid().simple())
            },
            revision: 1,
            enabled: true,
        })
        .collect()
}

async fn project_entitlements(
    repo: &SnaplinkCommercialRepo,
    workspaces: &[WorkspaceId],
    target: WorkspaceId,
    now: time::OffsetDateTime,
) {
    for workspace in workspaces {
        let hard = if *workspace == target { 2 } else { i64::MAX };
        assert_eq!(
            repo.project_entitlement(&entitlement(*workspace, now, hard))
                .await
                .unwrap(),
            ProjectionOutcome::Applied
        );
    }
}

fn entitlement(
    workspace_id: WorkspaceId,
    now: time::OffsetDateTime,
    hard: i64,
) -> SnaplinkEntitlementProjection {
    let unlimited = hard == i64::MAX;
    SnaplinkEntitlementProjection {
        workspace_id,
        tenant_id: format!("tenant-{workspace_id}"),
        revision: 1,
        active: true,
        im_enabled: true,
        notifications_enabled: true,
        messages: SnaplinkLimitProjection {
            soft: 0,
            hard: if unlimited { 0 } else { hard },
            unlimited,
        },
        notifications: SnaplinkLimitProjection {
            soft: 0,
            hard: 0,
            unlimited: false,
        },
        effective_at: now - time::Duration::minutes(1),
        expires_at: Some(now + time::Duration::hours(1)),
        generated_at: now,
    }
}

async fn create_workspace(pool: &sqlx::PgPool, workspace: WorkspaceId, actor: ParticipantId) {
    let mut tx = pool.begin().await.expect("workspace transaction");
    sqlx::query("INSERT INTO workspaces (id, name, slug, created_by) VALUES ($1, $2, $3, $4)")
        .bind(workspace.to_uuid())
        .bind("Commercial test")
        .bind(format!("commercial-{workspace}"))
        .bind(actor.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("workspace");
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, participant_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(workspace.to_uuid())
    .bind(actor.to_uuid())
    .execute(&mut *tx)
    .await
    .expect("workspace owner");
    tx.commit().await.expect("commit workspace");
}

async fn create_room(pool: &sqlx::PgPool, workspace: WorkspaceId, actor: ParticipantId) -> RoomId {
    let room = RoomId::new();
    sqlx::query(
        "INSERT INTO rooms (id, kind, name, created_by, workspace_id) VALUES ($1, 'group', $2, $3, $4)",
    )
    .bind(room.to_uuid())
    .bind("Commercial room")
    .bind(actor.to_uuid())
    .bind(workspace.to_uuid())
    .execute(pool)
    .await
    .expect("room");
    sqlx::query(
        "INSERT INTO room_members (room_id, participant_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(room.to_uuid())
    .bind(actor.to_uuid())
    .execute(pool)
    .await
    .expect("room owner");
    room
}

fn new_message(room: RoomId, actor: ParticipantId, text: &str) -> NewMessage {
    NewMessage {
        room_id: room,
        sender_id: actor,
        blocks: vec![Block::text(text)],
        reply_to: None,
        metadata: serde_json::json!({}),
        expires_at: None,
    }
}
