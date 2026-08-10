//! DB-integration tests for the recall (撤回) path — the acceptance-critical
//! permission matrix, transaction contents, and multi-tenant isolation.
//!
//! Run with a live Postgres with migrations applied:
//!
//! ```text
//! DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
//!   cargo test -p aero-storage --lib -- --ignored message::recall
//! ```

use aero_common::{
    BlobId, Block, Error, FileKind, Message, MessageId, ParticipantId, RoomId, WorkspaceId,
    RECALLED_MESSAGE_PLACEHOLDER,
};
use sqlx::PgPool;

use super::{MessageRepo, NewMessage};
use crate::event_outbox::EventOutboxKind;

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect_lazy(&url)
        .expect("valid DATABASE_URL")
}

async fn participant(pool: &PgPool, label: &str) -> ParticipantId {
    let id = ParticipantId::new();
    sqlx::query(
        "INSERT INTO participants (id, kind, display_name)
         VALUES ($1, 'human', $2)",
    )
    .bind(id.to_uuid())
    .bind(format!("{label}-{id}"))
    .execute(pool)
    .await
    .unwrap();
    id
}

/// Throwaway workspace + room with deterministic `room_members` rows (role
/// asserted by the test, not by repo defaults). Cleanup deletes the workspace
/// (rooms cascade) and participants.
pub(super) struct Fixture {
    pub(super) pool: PgPool,
    pub(super) workspace: WorkspaceId,
    pub(super) room: RoomId,
    pub(super) author: ParticipantId,
}

impl Fixture {
    pub(super) async fn create(label: &str) -> Self {
        let pool = pool();
        let author = participant(&pool, &format!("recall-{label}-author")).await;
        let workspace = WorkspaceId::new();
        // The workspace-owner commit guard (migration 0200, deferred) requires
        // the owner membership to exist by the time the workspace INSERT
        // commits, so both rows go in ONE transaction.
        let mut tx = pool.begin().await.unwrap();
        sqlx::query(
            "INSERT INTO workspaces (id, name, slug, created_by, created_at)
             VALUES ($1, $2, $3, $4, now())",
        )
        .bind(workspace.to_uuid())
        .bind(format!("Recall {label} {author}"))
        .bind(format!("recall-{label}-{workspace}"))
        .bind(author.to_uuid())
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, participant_id, role)
             VALUES ($1, $2, 'owner')",
        )
        .bind(workspace.to_uuid())
        .bind(author.to_uuid())
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let room = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
             VALUES ($1, 'group', $2, $3, now(), $4)",
        )
        .bind(room.to_uuid())
        .bind(format!("recall-{label}-{room}"))
        .bind(author.to_uuid())
        .bind(workspace.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
        Self {
            pool,
            workspace,
            room,
            author,
        }
    }

    pub(super) async fn enroll(&self, participant: ParticipantId, role: &str) {
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, participant_id, role)
             VALUES ($1, $2, 'member')
             ON CONFLICT (workspace_id, participant_id) DO NOTHING",
        )
        .bind(self.workspace.to_uuid())
        .bind(participant.to_uuid())
        .execute(&self.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role, joined_at)
             VALUES ($1, $2, $3, now())",
        )
        .bind(self.room.to_uuid())
        .bind(participant.to_uuid())
        .bind(role)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    pub(super) async fn insert_message(&self, sender: ParticipantId, body: &str) -> Message {
        MessageRepo::new(self.pool.clone())
            .insert_outboxed(
                NewMessage {
                    room_id: self.room,
                    sender_id: sender,
                    blocks: vec![Block::text(body)],
                    reply_to: None,
                    metadata: serde_json::Value::Null,
                    expires_at: None,
                },
                None,
                vec![],
                None,
            )
            .await
            .unwrap()
            .message()
            .clone()
    }

    pub(super) async fn cleanup(&self) {
        sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(self.workspace.to_uuid())
            .execute(&self.pool)
            .await
            .ok();
    }
}

/// The migration (0238) must have applied: both `messages` and the 0148 shadow
/// table carry the recall columns, the `event_outbox` kind CHECK accepts
/// `recalled`, and pre-existing rows read as not-recalled.
#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn recall_schema_columns_and_outbox_kind_are_applied() {
    let pool = pool();
    for table in ["messages", "messages_partitioned"] {
        let columns: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM information_schema.columns
              WHERE table_name = $1 AND column_name IN ('recalled_at', 'recalled_by')",
        )
        .bind(table)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            columns, 2,
            "{table} mirrors the recall columns (migration 0238)"
        );
    }
    let accepts: bool = sqlx::query_scalar(
        "SELECT count(*) > 0 FROM pg_constraint
          WHERE conname = 'event_outbox_kind_check'
            AND pg_get_constraintdef(oid) LIKE '%recalled%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(accepts, "event_outbox_kind_check includes 'recalled'");
}

/// Author recall writes the full transaction: placeholder blocks, `recalled_by` /
/// `recalled_at`, version bump, `message_edits` snapshot, `message.recalled`
/// audit row, and a `recalled` outbox row at the new aggregate version — all
/// atomically. The message row survives (not a tombstone).
#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn author_recall_replaces_content_and_records_audit_in_one_tx() {
    let fixture = Fixture::create("author").await;
    fixture.enroll(fixture.author, "owner").await;
    let repo = MessageRepo::new(fixture.pool.clone());
    let original = fixture.insert_message(fixture.author, "recall me").await;

    let recalled = repo
        .recall_outboxed_authorized(original.id, fixture.author, time::Duration::ZERO, None)
        .await
        .unwrap()
        .expect("recall succeeds");

    let stored = repo.get(original.id).await.unwrap().expect("row survives");
    assert_eq!(
        serde_json::to_value(&stored.blocks).unwrap(),
        serde_json::json!([{ "type": "text", "content": RECALLED_MESSAGE_PLACEHOLDER }]),
        "blocks replaced by the system placeholder"
    );
    assert!(stored.recalled_at.is_some(), "recalled_at recorded");
    assert_eq!(stored.recalled_by, Some(fixture.author));
    assert_eq!(stored.deleted_at, None, "recall is not a tombstone");
    assert_eq!(stored.version, original.version + 1, "optimistic-lock bump");
    assert_eq!(recalled.message.id, original.id);
    assert_eq!(recalled.message.recalled_by, Some(fixture.author));

    // Original body is preserved as edit-history evidence, authored by the recaller.
    let edits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM message_edits
          WHERE message_id = $1 AND editor_id = $2 AND blocks::text LIKE '%recall me%'",
    )
    .bind(original.id.to_uuid())
    .bind(fixture.author.to_uuid())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(edits, 1, "pre-recall body snapshotted into message_edits");

    // Workspace audit trail gets the message.recalled action in the same tx.
    let audit: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events
          WHERE workspace_id = $1 AND action = 'message.recalled'
            AND actor_id = $2 AND target = $3",
    )
    .bind(fixture.workspace.to_uuid())
    .bind(fixture.author.to_uuid())
    .bind(original.id.to_string())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(audit, 1, "message.recalled audit row");

    // Durable outbox row: kind 'recalled', aggregate_version = the new version.
    let (kind, version): (String, i64) =
        sqlx::query_as("SELECT event_kind, aggregate_version FROM event_outbox WHERE id = $1")
            .bind(recalled.outbox_id)
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert_eq!(kind, EventOutboxKind::Recalled.as_str());
    assert_eq!(version, i64::from(stored.version));

    // searchable/embedding are cleared so the placeholder never surfaces in FTS.
    let (searchable, embedding): (String, Option<pgvector::Vector>) =
        sqlx::query_as("SELECT searchable_text, embedding FROM messages WHERE id = $1")
            .bind(original.id.to_uuid())
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert!(searchable.is_empty());
    assert!(embedding.is_none());

    fixture.cleanup().await;
}

/// Permission matrix at the commit-time fence: author / admin / owner recall;
/// a plain member (non-author) gets the stable Forbidden.
#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn recall_permission_matrix_author_admin_owner_member() {
    let fixture = Fixture::create("matrix").await;
    fixture.enroll(fixture.author, "member").await;
    let admin = participant(&fixture.pool, "recall-matrix-admin").await;
    let owner = participant(&fixture.pool, "recall-matrix-owner").await;
    let member = participant(&fixture.pool, "recall-matrix-member").await;
    fixture.enroll(admin, "admin").await;
    fixture.enroll(owner, "owner").await;
    fixture.enroll(member, "member").await;
    let repo = MessageRepo::new(fixture.pool.clone());

    let message = fixture.insert_message(fixture.author, "matrix body").await;

    // Author.
    assert!(repo
        .recall_outboxed_authorized(message.id, fixture.author, time::Duration::ZERO, None)
        .await
        .unwrap()
        .is_some());

    // Admin recalls a fresh message.
    let message2 = fixture.insert_message(fixture.author, "admin target").await;
    let recalled = repo
        .recall_outboxed_authorized(message2.id, admin, time::Duration::ZERO, None)
        .await
        .unwrap()
        .expect("admin may recall");
    assert_eq!(recalled.message.recalled_by, Some(admin));

    // Owner recalls a fresh message.
    let message3 = fixture.insert_message(fixture.author, "owner target").await;
    assert!(repo
        .recall_outboxed_authorized(message3.id, owner, time::Duration::ZERO, None)
        .await
        .unwrap()
        .is_some());

    // Plain member (non-author) → stable 403.
    let message4 = fixture
        .insert_message(fixture.author, "member target")
        .await;
    let err = repo
        .recall_outboxed_authorized(message4.id, member, time::Duration::ZERO, None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Forbidden(msg) if msg == "only author or room admin may recall"),
        "member recall must be a stable Forbidden, got {err}"
    );

    // Already-recalled → stable Conflict (recall is one-shot, not idempotent).
    let err = repo
        .recall_outboxed_authorized(message.id, fixture.author, time::Duration::ZERO, None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Conflict(msg) if msg == "message is already recalled"),
        "double recall must be a stable Conflict, got {err}"
    );

    // Deleted message → stable Conflict.
    let message5 = fixture
        .insert_message(fixture.author, "delete target")
        .await;
    assert!(repo.soft_delete(message5.id).await.unwrap());
    let err = repo
        .recall_outboxed_authorized(message5.id, fixture.author, time::Duration::ZERO, None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Conflict(msg) if msg == "message is deleted"),
        "recalling a tombstone must be a stable Conflict, got {err}"
    );

    fixture.cleanup().await;
}

/// Multi-tenant isolation: an actor whose only membership is a room in a
/// DIFFERENT workspace must get Forbidden — and must never learn whether the
/// target message exists or what state it is in (no cross-tenant oracle).
#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn recall_cannot_cross_workspace_boundaries() {
    let fixture = Fixture::create("tenant-a").await;
    fixture.enroll(fixture.author, "owner").await;
    let repo = MessageRepo::new(fixture.pool.clone());
    let message = fixture
        .insert_message(fixture.author, "tenant a secret")
        .await;

    // A second workspace + room that the outsider belongs to.
    let outsider = participant(&fixture.pool, "recall-tenant-outsider").await;
    let other_workspace = WorkspaceId::new();
    // Workspace + owner membership must commit together (deferred owner guard).
    let mut tx = fixture.pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO workspaces (id, name, slug, created_by, created_at)
         VALUES ($1, $2, $3, $4, now())",
    )
    .bind(other_workspace.to_uuid())
    .bind(format!("Recall tenant B {outsider}"))
    .bind(format!("recall-tenant-b-{other_workspace}"))
    .bind(outsider.to_uuid())
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, participant_id, role)
         VALUES ($1, $2, 'owner')",
    )
    .bind(other_workspace.to_uuid())
    .bind(outsider.to_uuid())
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let other_room = RoomId::new();
    sqlx::query(
        "INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
         VALUES ($1, 'group', $2, $3, now(), $4)",
    )
    .bind(other_room.to_uuid())
    .bind(format!("recall-tenant-b-room-{other_room}"))
    .bind(outsider.to_uuid())
    .bind(other_workspace.to_uuid())
    .execute(&fixture.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO room_members (room_id, participant_id, role, joined_at)
         VALUES ($1, $2, 'owner', now())",
    )
    .bind(other_room.to_uuid())
    .bind(outsider.to_uuid())
    .execute(&fixture.pool)
    .await
    .unwrap();

    let err = repo
        .recall_outboxed_authorized(message.id, outsider, time::Duration::ZERO, None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Forbidden(_)),
        "cross-workspace recall must be Forbidden, got {err}"
    );

    // The message is untouched and its state is not leaked.
    let stored = repo.get(message.id).await.unwrap().unwrap();
    assert!(stored.recalled_at.is_none());
    assert_eq!(stored.version, message.version);
    assert_eq!(
        serde_json::to_value(&stored.blocks).unwrap(),
        serde_json::json!([{ "type": "text", "content": "tenant a secret" }]),
        "content untouched by the cross-tenant attempt"
    );

    fixture.cleanup().await;
}

/// A recalled message can still be tombstoned afterwards (recall and delete are
/// orthogonal state transitions).
#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn recalled_message_can_still_be_deleted() {
    let fixture = Fixture::create("then-delete").await;
    fixture.enroll(fixture.author, "owner").await;
    let repo = MessageRepo::new(fixture.pool.clone());
    let message = fixture
        .insert_message(fixture.author, "recall then delete")
        .await;

    assert!(repo
        .recall_outboxed_authorized(message.id, fixture.author, time::Duration::ZERO, None)
        .await
        .unwrap()
        .is_some());
    assert!(
        repo.soft_delete(message.id).await.unwrap(),
        "tombstone after recall"
    );

    let stored = repo.get(message.id).await.unwrap().unwrap();
    assert!(stored.deleted_at.is_some());
    assert!(
        stored.recalled_at.is_some(),
        "recall evidence survives the tombstone"
    );

    fixture.cleanup().await;
}

/// Regression (gate P1, security S1): a system edit (unfurl link-preview,
/// transcribe) landing AFTER a recall must be a no-op — the storage fence
/// returns `None`, so a slow bot network fetch can never resurrect the
/// original content over the system placeholder.
#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn system_edit_after_recall_is_fenced() {
    let fixture = Fixture::create("fence").await;
    fixture.enroll(fixture.author, "owner").await;
    let repo = MessageRepo::new(fixture.pool.clone());
    let message = fixture
        .insert_message(fixture.author, "pre-recall body")
        .await;

    let recalled = repo
        .recall_outboxed_authorized(message.id, fixture.author, time::Duration::ZERO, None)
        .await
        .unwrap()
        .expect("recall succeeds");

    // The unfurl bot's system edit arrives late (slow network fetch): it must
    // be fenced, not a rewrite of the placeholder with the original body.
    let edit = repo
        .edit_outboxed_system(
            message.id,
            vec![Block::text("resurrected original")],
            recalled.message.version,
            None,
            None,
        )
        .await
        .unwrap();
    assert!(edit.is_none(), "system edit after recall must be fenced");

    // Same for the voice-transcript path.
    let transcript = repo
        .update_voice_transcript_outboxed(message.id, "late transcript", None)
        .await
        .unwrap();
    assert!(
        transcript.is_none(),
        "transcribe after recall must be fenced"
    );

    let stored = repo.get(message.id).await.unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(&stored.blocks).unwrap(),
        serde_json::json!([{ "type": "text", "content": RECALLED_MESSAGE_PLACEHOLDER }]),
        "placeholder body survives the late system edit"
    );
    assert!(stored.recalled_at.is_some());
    assert_eq!(
        stored.version, recalled.message.version,
        "no version bump from the fenced edit"
    );

    fixture.cleanup().await;
}

/// Regression (gate P2, security S2): `changes_since` — the reconnect mutation
/// backfill — must surface recalls. Recall does NOT bump `edited_at`, so a
/// query keyed on `GREATEST(edited_at, deleted_at)` would silently hide the
/// placeholder from offline/reconnecting clients forever (original content
/// would stay on screen).
#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn changes_since_delivers_recalls() {
    let fixture = Fixture::create("replay").await;
    fixture.enroll(fixture.author, "owner").await;
    let repo = MessageRepo::new(fixture.pool.clone());
    let message = fixture
        .insert_message(fixture.author, "replay target")
        .await;

    let before = time::OffsetDateTime::now_utc();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let recalled = repo
        .recall_outboxed_authorized(message.id, fixture.author, time::Duration::ZERO, None)
        .await
        .unwrap()
        .expect("recall succeeds");

    let changes = repo.changes_since(fixture.room, before, 100).await.unwrap();
    let hit = changes
        .iter()
        .find(|m| m.id == message.id)
        .expect("recalled message is delivered by changes_since");
    assert!(hit.recalled_at.is_some(), "replay row carries recall state");
    assert_eq!(
        serde_json::to_value(&hit.blocks).unwrap(),
        serde_json::json!([{ "type": "text", "content": RECALLED_MESSAGE_PLACEHOLDER }]),
        "replay row carries the placeholder body"
    );
    // The mutation instant is recalled_at — recall does not fake an edit.
    assert_eq!(hit.recalled_at, recalled.message.recalled_at);
    assert_eq!(hit.edited_at, None, "recall must not bump edited_at");

    // Sanity: an ordinary message created before `since` is not reported.
    let other = fixture
        .insert_message(fixture.author, "older sibling")
        .await;
    assert!(
        !changes.iter().any(|m| m.id == other.id),
        "created-before-since messages are not changes"
    );

    fixture.cleanup().await;
}

/// Regression (gate HIGH, DB F1): the partition-cutover prep function reissued
/// by migration 0238 must carry the recall columns into the shadow — a cutover
/// backfill must not silently drop recall state (badge/guard/replay break).
#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn partition_backfill_carries_recall_columns() {
    let fixture = Fixture::create("backfill").await;
    fixture.enroll(fixture.author, "owner").await;
    let repo = MessageRepo::new(fixture.pool.clone());
    // Mint the cursor BEFORE the insert so the batch starts at this message.
    let backfill_from = MessageId::new();
    let message = fixture
        .insert_message(fixture.author, "backfill target")
        .await;
    repo.recall_outboxed_authorized(message.id, fixture.author, time::Duration::ZERO, None)
        .await
        .unwrap()
        .expect("recall succeeds");

    // Backfill a single batch STARTING just before the fixture message: ULID
    // ids are time-ordered, so a `from_id` minted before the insert bounds the
    // batch to the fixture row. Sweeping the whole table from nil would copy
    // every other test's data too — including rows the blob-scope fence tests
    // deliberately leave with cross-workspace references, which the shadow's
    // `messages_enforce_blob_workspace_scope` trigger then rejects.
    let (rows, _last): (i64, uuid::Uuid) =
        sqlx::query_as("SELECT rows_copied, last_id FROM backfill_messages_partition(5000, $1)")
            .bind(backfill_from.to_uuid())
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert!(rows >= 1, "backfill copied the fixture message");

    let (recalled_at, recalled_by): (Option<time::OffsetDateTime>, Option<uuid::Uuid>) =
        sqlx::query_as("SELECT recalled_at, recalled_by FROM messages_partitioned WHERE id = $1")
            .bind(message.id.to_uuid())
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert!(
        recalled_at.is_some() && recalled_by == Some(fixture.author.to_uuid()),
        "shadow row carries recall state through the cutover backfill"
    );

    fixture.cleanup().await;
}

/// Gap ② (plan §5.3): two concurrent recalls of the same message must have
/// exactly one winner — the row lock + `WHERE recalled_at IS NULL` final fence
/// serialize the transition, the loser observes the committed recall and gets
/// the stable Conflict, and exactly ONE outbox event is produced.
#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn concurrent_double_recall_has_exactly_one_winner() {
    let fixture = Fixture::create("race").await;
    fixture.enroll(fixture.author, "owner").await;
    let repo = MessageRepo::new(fixture.pool.clone());
    let message = fixture.insert_message(fixture.author, "race target").await;

    let first =
        repo.recall_outboxed_authorized(message.id, fixture.author, time::Duration::ZERO, None);
    let second =
        repo.recall_outboxed_authorized(message.id, fixture.author, time::Duration::ZERO, None);
    let (first, second) = tokio::join!(first, second);

    let winners = [&first, &second]
        .iter()
        .filter(|result| matches!(result, Ok(Some(_))))
        .count();
    let conflicts = [&first, &second]
        .iter()
        .filter(|result| {
            matches!(result, Err(Error::Conflict(msg)) if msg == "message is already recalled")
        })
        .count();
    assert_eq!(winners, 1, "exactly one recall wins the race");
    assert_eq!(
        conflicts, 1,
        "the loser gets the stable already-recalled Conflict"
    );

    let stored = repo.get(message.id).await.unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(&stored.blocks).unwrap(),
        serde_json::json!([{ "type": "text", "content": RECALLED_MESSAGE_PLACEHOLDER }]),
        "placeholder body"
    );
    assert_eq!(
        stored.version,
        message.version + 1,
        "exactly one version bump"
    );

    // Exactly one durable outbox row for the recall, at the new version.
    let outbox_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM event_outbox
          WHERE message_id = $1 AND event_kind = 'recalled'",
    )
    .bind(message.id.to_uuid())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(outbox_rows, 1, "exactly one Recalled outbox row");

    fixture.cleanup().await;
}

/// Gate round-3 B1 (failing-test-first): the recall history snapshot must be
/// REDACTED of byte references. The recall tx enqueues the original attachment
/// blobs for GC (bytes are content — removed), while `message_edits` is
/// invisible to the GC live-reference scan; a snapshot carrying `blob_id`s
/// would point at destroyed bytes within ~60s. Assert: snapshot has no
/// `blob_id` (transcript/text evidence preserved), GC still enqueues both
/// blobs (no leak), message row == placeholder.
#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn recall_snapshot_redacts_blob_references_and_gc_proceeds() {
    let fixture = Fixture::create("redact").await;
    fixture.enroll(fixture.author, "owner").await;
    let repo = MessageRepo::new(fixture.pool.clone());

    let file_blob = BlobId::new();
    let voice_blob = BlobId::new();
    for (blob, kind, name, mime, size) in [
        (file_blob, "document", "f.txt", "text/plain", 100i64),
        (voice_blob, "audio", "v.mp3", "audio/mpeg", 5000i64),
    ] {
        sqlx::query(
            "INSERT INTO blobs
                 (id, owner_id, workspace_id, kind, name, mime, size, storage_key, finalized_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now())",
        )
        .bind(blob.to_uuid())
        .bind(fixture.author.to_uuid())
        .bind(fixture.workspace.to_uuid())
        .bind(kind)
        .bind(name)
        .bind(mime)
        .bind(size)
        .bind(format!("recall-test/{blob}"))
        .execute(&fixture.pool)
        .await
        .unwrap();
    }

    let message = repo
        .insert_outboxed(
            NewMessage {
                room_id: fixture.room,
                sender_id: fixture.author,
                blocks: vec![
                    Block::File {
                        blob_id: file_blob,
                        kind: FileKind::Document,
                        name: "f.txt".into(),
                        size: 100,
                    },
                    Block::Voice {
                        blob_id: voice_blob,
                        duration_ms: 5000,
                        transcript: Some("spoken words".into()),
                    },
                ],
                reply_to: None,
                metadata: serde_json::Value::Null,
                expires_at: None,
            },
            None,
            vec![],
            None,
        )
        .await
        .unwrap()
        .message()
        .clone();

    repo.recall_outboxed_authorized(message.id, fixture.author, time::Duration::ZERO, None)
        .await
        .unwrap()
        .expect("recall succeeds");

    // ① Snapshot is redacted: no blob_id anywhere, transcript preserved.
    let snapshot: serde_json::Value = sqlx::query_scalar(
        "SELECT blocks FROM message_edits
          WHERE message_id = $1
          ORDER BY recorded_at DESC LIMIT 1",
    )
    .bind(message.id.to_uuid())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    let snapshot_text = snapshot.to_string();
    assert!(
        !snapshot_text.contains(&file_blob.to_string()),
        "file blob_id must not leak into the recall snapshot"
    );
    assert!(
        !snapshot_text.contains(&voice_blob.to_string()),
        "voice blob_id must not leak into the recall snapshot"
    );
    assert_eq!(snapshot_text.matches("\"blob_id\"").count(), 0);
    assert!(
        snapshot_text.contains("spoken words"),
        "voice transcript survives as text evidence"
    );
    assert!(snapshot_text.contains("[附件已移除]"));

    // ② GC still enqueues both attachment blobs (bytes removed, no leak).
    let gc_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM blob_gc_queue
          WHERE blob_id = ANY($1)",
    )
    .bind(vec![file_blob.to_uuid(), voice_blob.to_uuid()])
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(gc_rows, 2, "both attachment blobs remain enqueued for GC");

    // ③ The live row carries the placeholder body.
    let stored = repo.get(message.id).await.unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(&stored.blocks).unwrap(),
        serde_json::json!([{ "type": "text", "content": RECALLED_MESSAGE_PLACEHOLDER }])
    );

    fixture.cleanup().await;
}

/// Recall window (撤回时间窗): enforced for the AUTHOR inside the transaction
/// against the row-locked snapshot, while room owner/admin recall (moderation)
/// is exempt. Backdating uses an APP-clock parameter-bound timestamp
/// (`created_at` is app-minted at insert; DB `now()` would mix clocks).
#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn recall_window_expired_author_rejected_admin_override() {
    let fixture = Fixture::create("window-admin").await;
    fixture.enroll(fixture.author, "owner").await;
    let admin = participant(&fixture.pool, "recall-window-admin-admin").await;
    fixture.enroll(admin, "admin").await;
    let repo = MessageRepo::new(fixture.pool.clone());
    let message = fixture.insert_message(fixture.author, "old message").await;
    sqlx::query("UPDATE messages SET created_at = $1 WHERE id = $2")
        .bind(aero_common::time::now_utc() - time::Duration::seconds(90 * 60))
        .bind(message.id.to_uuid())
        .execute(&fixture.pool)
        .await
        .unwrap();

    let err = repo
        .recall_outboxed_authorized(
            message.id,
            fixture.author,
            time::Duration::seconds(3600),
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(&err, Error::Conflict(msg) if msg == "recall window expired"));

    // Admin/owner override: the SAME expired message is recallable.
    let recalled = repo
        .recall_outboxed_authorized(message.id, admin, time::Duration::seconds(3600), None)
        .await
        .unwrap()
        .expect("admin recall of an expired message succeeds");
    assert_eq!(recalled.message.recalled_by, Some(admin));
    fixture.cleanup().await;
}

/// Boundary margins (latency-proof): 86399s < 86400s window → allowed;
/// 86401s > 86400s → expired. The exact `t = window` proof lives in the pure
/// unit test (`recall_window_tests` in aero-im-core) with a single captured
/// clock — a real clock makes the exact instant unprovable at this layer.
#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn recall_window_boundary_margins() {
    let fixture = Fixture::create("window-margin").await;
    fixture.enroll(fixture.author, "owner").await;
    let repo = MessageRepo::new(fixture.pool.clone());

    let inside = fixture.insert_message(fixture.author, "inside").await;
    sqlx::query("UPDATE messages SET created_at = $1 WHERE id = $2")
        .bind(aero_common::time::now_utc() - time::Duration::seconds(86_399))
        .bind(inside.id.to_uuid())
        .execute(&fixture.pool)
        .await
        .unwrap();
    let recalled = repo
        .recall_outboxed_authorized(
            inside.id,
            fixture.author,
            time::Duration::seconds(86_400),
            None,
        )
        .await
        .unwrap()
        .expect("age 86399s is within the 86400s window");
    assert_eq!(recalled.message.recalled_by, Some(fixture.author));

    let outside = fixture.insert_message(fixture.author, "outside").await;
    sqlx::query("UPDATE messages SET created_at = $1 WHERE id = $2")
        .bind(aero_common::time::now_utc() - time::Duration::seconds(86_401))
        .bind(outside.id.to_uuid())
        .execute(&fixture.pool)
        .await
        .unwrap();
    let err = repo
        .recall_outboxed_authorized(
            outside.id,
            fixture.author,
            time::Duration::seconds(86_400),
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(&err, Error::Conflict(msg) if msg == "recall window expired"));
    fixture.cleanup().await;
}

/// `AERO_RECALL_WINDOW_SECS=0` = unlimited: any age is recallable.
#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn recall_window_zero_is_unlimited() {
    let fixture = Fixture::create("window-zero").await;
    fixture.enroll(fixture.author, "owner").await;
    let repo = MessageRepo::new(fixture.pool.clone());
    let message = fixture.insert_message(fixture.author, "ancient").await;
    sqlx::query("UPDATE messages SET created_at = $1 WHERE id = $2")
        .bind(aero_common::time::now_utc() - time::Duration::days(30))
        .bind(message.id.to_uuid())
        .execute(&fixture.pool)
        .await
        .unwrap();

    let recalled = repo
        .recall_outboxed_authorized(message.id, fixture.author, time::Duration::ZERO, None)
        .await
        .unwrap()
        .expect("window 0 = unlimited");
    assert_eq!(recalled.message.recalled_by, Some(fixture.author));
    fixture.cleanup().await;
}

/// Boundary race: author (expired) vs admin on the same row. Either interleave
/// is safe — author-first: author gets the window Conflict (no write), admin
/// commits; admin-first: author observes `recalled_at` and gets the stable
/// already-recalled Conflict. Assert the partition, not the order.
#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn recall_boundary_race_author_vs_admin() {
    let fixture = Fixture::create("window-race").await;
    fixture.enroll(fixture.author, "owner").await;
    let admin = participant(&fixture.pool, "recall-window-race-admin").await;
    fixture.enroll(admin, "admin").await;
    let repo = MessageRepo::new(fixture.pool.clone());
    let message = fixture.insert_message(fixture.author, "race me").await;
    sqlx::query("UPDATE messages SET created_at = $1 WHERE id = $2")
        .bind(aero_common::time::now_utc() - time::Duration::seconds(2 * 86_400))
        .bind(message.id.to_uuid())
        .execute(&fixture.pool)
        .await
        .unwrap();

    let author_fut = repo.recall_outboxed_authorized(
        message.id,
        fixture.author,
        time::Duration::seconds(86_400),
        None,
    );
    let admin_fut =
        repo.recall_outboxed_authorized(message.id, admin, time::Duration::seconds(86_400), None);
    let (author_res, admin_res) = tokio::join!(author_fut, admin_fut);

    let admin_recalled = admin_res
        .expect("admin recall never errors")
        .expect("admin recall wins");
    assert_eq!(admin_recalled.message.recalled_by, Some(admin));
    let author_err = author_res.expect_err("author recall of an expired message must fail");
    assert!(
        matches!(&author_err, Error::Conflict(msg)
            if msg == "recall window expired" || msg == "message is already recalled"),
        "author failure must be a stable Conflict, got {author_err:?}"
    );
    fixture.cleanup().await;
}

/// Precedence: state checks (deleted / already-recalled) run before the window
/// check in both layers — an expired-and-deleted message still reports
/// "message is deleted".
#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn recall_window_precedence_deleted_before_expired() {
    let fixture = Fixture::create("window-precedence").await;
    fixture.enroll(fixture.author, "owner").await;
    let repo = MessageRepo::new(fixture.pool.clone());
    let message = fixture.insert_message(fixture.author, "old and gone").await;
    sqlx::query("UPDATE messages SET created_at = $1 WHERE id = $2")
        .bind(aero_common::time::now_utc() - time::Duration::days(30))
        .bind(message.id.to_uuid())
        .execute(&fixture.pool)
        .await
        .unwrap();
    repo.soft_delete_outboxed_authorized(message.id, fixture.author, None)
        .await
        .unwrap();

    let err = repo
        .recall_outboxed_authorized(
            message.id,
            fixture.author,
            time::Duration::seconds(3600),
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(&err, Error::Conflict(msg) if msg == "message is deleted"));
    fixture.cleanup().await;
}

/// The window state is never leaked to non-privileged actors: a plain member
/// probing an expired message gets the same Forbidden as a member probing a
/// fresh one (the role gate precedes the window check).
#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn recall_window_no_leak_to_member() {
    let fixture = Fixture::create("window-leak").await;
    fixture.enroll(fixture.author, "owner").await;
    let member = participant(&fixture.pool, "recall-window-leak-member").await;
    fixture.enroll(member, "member").await;
    let repo = MessageRepo::new(fixture.pool.clone());
    let message = fixture.insert_message(fixture.author, "old secret").await;
    sqlx::query("UPDATE messages SET created_at = $1 WHERE id = $2")
        .bind(aero_common::time::now_utc() - time::Duration::days(30))
        .bind(message.id.to_uuid())
        .execute(&fixture.pool)
        .await
        .unwrap();

    let err = repo
        .recall_outboxed_authorized(message.id, member, time::Duration::seconds(3600), None)
        .await
        .unwrap_err();
    assert!(matches!(&err, Error::Forbidden(msg) if msg == "only author or room admin may recall"));
    fixture.cleanup().await;
}
