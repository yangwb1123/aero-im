//! Notification inbox repository (mentions & thread replies).
//!
//! Backs `migrations/0010_notifications.sql`. A per-recipient inbox: when a
//! message @-mentions a participant or replies to one of their messages, a row
//! is appended here so the recipient has a durable, cross-device record + unread
//! badge — independent of whether they were online for the realtime event.
//!
//! Every read is scoped to a single `participant_id`, so one user's inbox can
//! never surface another's. Purely additive: a NEW [`NotificationRepo`]; no
//! existing repo is touched. The `id` is a ULID stored as UUID (time-sortable),
//! so reverse-chronological listing is a keyset walk on `id` descending — the
//! same pattern as [`crate::AuditRepo`].

use aero_common::{
    MessageId, Notification, NotificationId, NotificationKind, ParticipantId, RoomId,
};
use sqlx::PgPool;

/// Largest page a notification listing will return, regardless of requested `limit`.
const MAX_PAGE: i64 = 100;

/// Clamp a requested page size into `1..=MAX_PAGE` (defaulting `None`/non-positive
/// to `MAX_PAGE`). Pure so it unit-tests without a DB.
fn clamp_limit(requested: Option<i64>) -> i64 {
    match requested {
        Some(n) if n >= 1 => n.min(MAX_PAGE),
        _ => MAX_PAGE,
    }
}

#[derive(Clone)]
pub struct NotificationRepo {
    pool: PgPool,
}

impl NotificationRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Hard-delete already-read notifications older than `cutoff` (data-lifecycle
    /// retention sweep, ROADMAP5 方向四). `notifications` is append-only with no
    /// prior retention, so it grew without bound. Only *read* rows are swept — an
    /// unread mention must still reach the user however old it is — so the sweep
    /// can never drop a notification the recipient hasn't seen. Returns the number
    /// of rows deleted.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn sweep_read_before(
        &self,
        cutoff: time::OffsetDateTime,
    ) -> Result<u64, sqlx::Error> {
        let res = sqlx::query(
            r"DELETE FROM notifications
               WHERE read_at IS NOT NULL AND created_at < $1",
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Append one notification, returning its generated id.
    pub async fn insert(
        &self,
        participant: ParticipantId,
        room: RoomId,
        message: MessageId,
        kind: NotificationKind,
        actor: Option<ParticipantId>,
    ) -> Result<NotificationId, sqlx::Error> {
        let id = NotificationId::new();
        let created_at = time::OffsetDateTime::now_utc();
        sqlx::query(
            r"INSERT INTO notifications
                 (id, participant_id, room_id, message_id, kind, actor_id, created_at, importance_score)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .bind(room.to_uuid())
        .bind(message.to_uuid())
        .bind(kind.as_str())
        .bind(actor.map(|p| p.to_uuid()))
        .bind(created_at)
        // Compute the heuristic importance at insert (the column otherwise keeps
        // the static 0.5 default — `importance_for` was dead code).
        .bind(aero_common::model::importance_for(&kind))
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Batch-insert notifications for many recipients of ONE message in a single
    /// round-trip (ROADMAP 第三版 方向四 — write amplification). A large-room
    /// `@everyone` previously did one INSERT transaction per recipient (O(N) PG
    /// round-trips on the send path); this collapses them to a single multi-row
    /// INSERT via `UNNEST`. `room`/`message`/`actor`/`created_at` are constant
    /// across the batch; only `participant` + `kind` vary. Returns the row count.
    /// A no-op (Ok(0)) for an empty `recipients`.
    ///
    /// `delivery_id` is the NotifyBatch idempotency token (mig 0137). When
    /// `Some`, every row carries it and the insert is `ON CONFLICT
    /// (delivery_id, participant_id) WHERE delivery_id IS NOT NULL DO NOTHING`,
    /// so a redelivered batch (same deterministic `delivery_id`) re-expanded
    /// into the same recipients yields zero new rows for anyone already
    /// notified — matching the partial unique index. When `None`, rows get a
    /// NULL `delivery_id` and never participate in de-duplication (the legacy
    /// behaviour, preserved for any caller outside the NotifyBatch path).
    pub async fn insert_many(
        &self,
        room: RoomId,
        message: MessageId,
        actor: Option<ParticipantId>,
        recipients: &[(ParticipantId, NotificationKind)],
        delivery_id: Option<uuid::Uuid>,
    ) -> Result<u64, sqlx::Error> {
        if recipients.is_empty() {
            return Ok(0);
        }
        let created_at = time::OffsetDateTime::now_utc();
        let ids: Vec<uuid::Uuid> = recipients.iter().map(|_| NotificationId::new().to_uuid()).collect();
        let pids: Vec<uuid::Uuid> = recipients.iter().map(|(p, _)| p.to_uuid()).collect();
        let kinds: Vec<String> = recipients.iter().map(|(_, k)| k.as_str().to_owned()).collect();
        // Per-row importance from the kind (parallel to the UNNEST arrays below) —
        // otherwise every batched row keeps the static 0.5 default.
        let imps: Vec<f32> = recipients.iter().map(|(_, k)| aero_common::model::importance_for(k)).collect();
        let actor_uuid = actor.map(|a| a.to_uuid());
        // The same `delivery_id` (or NULL) is stamped onto every row of the
        // batch via the $8 bind (broadcast across the UNNEST rows). The partial
        // unique index only covers non-NULL delivery_ids, so the ON CONFLICT
        // target must repeat the index predicate to be inferred.
        let res = sqlx::query(
            r"INSERT INTO notifications
                 (id, participant_id, room_id, message_id, kind, actor_id, created_at, delivery_id, importance_score)
              SELECT u.id, u.pid, $4, $5, u.kind, $6, $7, $8, u.imp
                FROM UNNEST($1::uuid[], $2::uuid[], $3::text[], $9::real[]) AS u(id, pid, kind, imp)
              ON CONFLICT (delivery_id, participant_id) WHERE delivery_id IS NOT NULL
                 DO NOTHING",
        )
        .bind(&ids)
        .bind(&pids)
        .bind(&kinds)
        .bind(room.to_uuid())
        .bind(message.to_uuid())
        .bind(actor_uuid)
        .bind(created_at)
        .bind(delivery_id)
        .bind(&imps)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// List a participant's notifications, newest first, paginated by a keyset
    /// cursor. `before` is an exclusive upper bound (pass the oldest id from the
    /// previous page to fetch the next). When `unread_only`, soft-filters to rows
    /// with `read_at IS NULL`. Always scoped to `participant` — never leaks
    /// another user's inbox.
    pub async fn list(
        &self,
        participant: ParticipantId,
        before: Option<NotificationId>,
        unread_only: bool,
        limit: Option<i64>,
    ) -> Result<Vec<Notification>, sqlx::Error> {
        let limit = clamp_limit(limit);
        let rows = sqlx::query_as::<_, NotificationRow>(
            r"SELECT id, participant_id, room_id, message_id, kind, actor_id, created_at, read_at,
                      aggregate_count, importance_score
               FROM notifications
               WHERE participant_id = $1
                 AND ($2::uuid IS NULL OR id < $2)
                 AND (NOT $3 OR read_at IS NULL)
               ORDER BY id DESC
               LIMIT $4",
        )
        .bind(participant.to_uuid())
        .bind(before.map(|n| n.to_uuid()))
        .bind(unread_only)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Notification::from).collect())
    }

    /// Count a participant's unread notifications (the mention badge).
    pub async fn unread_count(&self, participant: ParticipantId) -> Result<u64, sqlx::Error> {
        let row = sqlx::query_as::<_, (i64,)>(
            r"SELECT COUNT(*) FROM notifications
               WHERE participant_id = $1 AND read_at IS NULL",
        )
        .bind(participant.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(u64::try_from(row.0).unwrap_or(0))
    }

    /// Unread notification counts grouped by room — drives per-room mention
    /// badges in the sidebar. Only rooms with at least one unread appear.
    pub async fn unread_counts_by_room(
        &self,
        participant: ParticipantId,
    ) -> Result<Vec<(RoomId, u32)>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid, i64)>(
            r"SELECT room_id, COUNT(*)
               FROM notifications
               WHERE participant_id = $1 AND read_at IS NULL
               GROUP BY room_id",
        )
        .bind(participant.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(r, c)| (RoomId::from_uuid(r), u32::try_from(c).unwrap_or(u32::MAX)))
            .collect())
    }

    /// Mark specific notifications read. Scoped to `participant` so a caller can
    /// never flip another user's rows. Returns the number of rows updated.
    pub async fn mark_read(
        &self,
        participant: ParticipantId,
        ids: &[NotificationId],
    ) -> Result<u64, sqlx::Error> {
        if ids.is_empty() {
            return Ok(0);
        }
        let uuids: Vec<uuid::Uuid> = ids.iter().map(NotificationId::to_uuid).collect();
        let result = sqlx::query(
            r"UPDATE notifications
                 SET read_at = now()
               WHERE participant_id = $1 AND read_at IS NULL AND id = ANY($2)",
        )
        .bind(participant.to_uuid())
        .bind(&uuids)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Mark all of a participant's notifications read (optionally only one room's).
    /// Returns the number of rows updated.
    pub async fn mark_all_read(
        &self,
        participant: ParticipantId,
        room: Option<RoomId>,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE notifications
                 SET read_at = now()
               WHERE participant_id = $1
                 AND read_at IS NULL
                 AND ($2::uuid IS NULL OR room_id = $2)",
        )
        .bind(participant.to_uuid())
        .bind(room.map(|r| r.to_uuid()))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

#[derive(sqlx::FromRow)]
struct NotificationRow {
    id: uuid::Uuid,
    participant_id: uuid::Uuid,
    room_id: uuid::Uuid,
    message_id: uuid::Uuid,
    kind: String,
    actor_id: Option<uuid::Uuid>,
    created_at: time::OffsetDateTime,
    read_at: Option<time::OffsetDateTime>,
    aggregate_count: Option<i32>,
    importance_score: f32,
}

impl From<NotificationRow> for Notification {
    fn from(r: NotificationRow) -> Self {
        Self {
            id: NotificationId::from_uuid(r.id),
            participant_id: ParticipantId::from_uuid(r.participant_id),
            room_id: RoomId::from_uuid(r.room_id),
            message_id: MessageId::from_uuid(r.message_id),
            kind: NotificationKind::from_str_lenient(&r.kind),
            actor_id: r.actor_id.map(ParticipantId::from_uuid),
            created_at: r.created_at,
            read_at: r.read_at,
            // Surface the columns the read SELECT now fetches (the bundle flush
            // writes aggregate_count; migration 0139 + the insert path write
            // importance_score). These were hardcoded to defaults, so the
            // aggregation/importance feature never reached GET /api/notifications.
            aggregate_count: r.aggregate_count.map(|n| u32::try_from(n).unwrap_or(0)),
            importance_score: r.importance_score,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_limit_defaults_and_bounds() {
        assert_eq!(clamp_limit(None), MAX_PAGE);
        assert_eq!(clamp_limit(Some(0)), MAX_PAGE);
        assert_eq!(clamp_limit(Some(-5)), MAX_PAGE);
        assert_eq!(clamp_limit(Some(1)), 1);
        assert_eq!(clamp_limit(Some(25)), 25);
        assert_eq!(clamp_limit(Some(10_000)), MAX_PAGE);
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored notif_
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::{MessageId, NotificationKind, ParticipantId, RoomId};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a participant, a room (in the default workspace), and a message so
    /// the FKs are satisfied. Returns (recipient, room, message, actor).
    async fn fixture(p: &PgPool) -> (ParticipantId, RoomId, MessageId, ParticipantId) {
        let recipient = ParticipantId::new();
        let actor = ParticipantId::new();
        for (who, name) in [(recipient, "notif-recipient"), (actor, "notif-actor")] {
            sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
                .bind(who.to_uuid())
                .bind(format!("{name}-{who}"))
                .execute(p)
                .await
                .expect("insert participant");
        }
        let room = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
             VALUES ($1,'channel',$2,$3, now(), '00000000-0000-0000-0000-000000000000')",
        )
        .bind(room.to_uuid())
        .bind("notif-room")
        .bind(actor.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        let message = MessageId::new();
        sqlx::query(
            "INSERT INTO messages (id, room_id, sender_id, blocks, searchable_text, created_at)
             VALUES ($1,$2,$3,'[]'::jsonb,'', now())",
        )
        .bind(message.to_uuid())
        .bind(room.to_uuid())
        .bind(actor.to_uuid())
        .execute(p)
        .await
        .expect("insert message");
        (recipient, room, message, actor)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn notif_insert_list_count_and_mark_read() {
        let p = pool();
        let repo = NotificationRepo::new(p.clone());
        let (recipient, room, message, actor) = fixture(&p).await;

        let id = repo
            .insert(recipient, room, message, NotificationKind::Mention, Some(actor))
            .await
            .unwrap();

        // Listed, unread, newest-first, scoped to the recipient.
        let list = repo.list(recipient, None, true, Some(10)).await.unwrap();
        assert!(list.iter().any(|n| n.id == id), "inserted notif is listed");
        let before = repo.unread_count(recipient).await.unwrap();
        assert!(before >= 1, "unread count includes the new notif");

        // Per-room grouping includes this room.
        let by_room = repo.unread_counts_by_room(recipient).await.unwrap();
        assert!(by_room.iter().any(|(r, c)| *r == room && *c >= 1));

        // Marking read drops it from the unread set.
        let updated = repo.mark_read(recipient, &[id]).await.unwrap();
        assert_eq!(updated, 1, "exactly the one row flips to read");
        let after = repo.unread_count(recipient).await.unwrap();
        assert_eq!(after, before - 1, "unread count drops by one");
    }

    /// A NotifyBatch redelivered after a consumer crash re-expands into the same
    /// recipients with the SAME deterministic `delivery_id`; the partial unique
    /// index + `ON CONFLICT (delivery_id, participant_id) DO NOTHING` must keep
    /// exactly one row per (delivery_id, participant) — mig 0137 idempotency.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn notif_insert_many_dedups_on_delivery_id() {
        let p = pool();
        let repo = NotificationRepo::new(p.clone());
        let (recipient, room, message, actor) = fixture(&p).await;
        let delivery_id = uuid::Uuid::new_v4();
        let recipients = [(recipient, NotificationKind::Mention)];

        // First delivery: one row inserted.
        let first = repo
            .insert_many(room, message, Some(actor), &recipients, Some(delivery_id))
            .await
            .unwrap();
        assert_eq!(first, 1, "original delivery inserts the recipient");

        // Redelivery with the SAME delivery_id: ON CONFLICT swallows it.
        let second = repo
            .insert_many(room, message, Some(actor), &recipients, Some(delivery_id))
            .await
            .unwrap();
        assert_eq!(second, 0, "redelivery inserts zero rows");

        // Exactly one row physically exists for that (delivery_id, participant).
        let count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM notifications
              WHERE delivery_id = $1 AND participant_id = $2",
        )
        .bind(delivery_id)
        .bind(recipient.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(count.0, 1, "exactly one durable row survives the redelivery");

        // A DIFFERENT delivery_id for the same recipient is NOT de-duped (two
        // distinct batches — e.g. mention then reply of one message — coexist).
        let other_delivery = uuid::Uuid::new_v4();
        let third = repo
            .insert_many(room, message, Some(actor), &recipients, Some(other_delivery))
            .await
            .unwrap();
        assert_eq!(third, 1, "a distinct delivery_id inserts a fresh row");

        // A NULL delivery_id (legacy / non-NotifyBatch caller) never de-dups:
        // two such inserts both land.
        let n1 = repo
            .insert_many(room, message, Some(actor), &recipients, None)
            .await
            .unwrap();
        let n2 = repo
            .insert_many(room, message, Some(actor), &recipients, None)
            .await
            .unwrap();
        assert_eq!((n1, n2), (1, 1), "NULL delivery_id rows never conflict");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn notif_mark_read_is_recipient_scoped() {
        let p = pool();
        let repo = NotificationRepo::new(p.clone());
        let (recipient, room, message, actor) = fixture(&p).await;
        let other = actor; // a different participant

        let id = repo
            .insert(recipient, room, message, NotificationKind::Reply, Some(actor))
            .await
            .unwrap();

        // `other` cannot mark `recipient`'s notification read.
        let updated = repo.mark_read(other, &[id]).await.unwrap();
        assert_eq!(updated, 0, "cross-user mark-read is a no-op");
        let still = repo.list(recipient, None, true, Some(10)).await.unwrap();
        assert!(still.iter().any(|n| n.id == id), "still unread for the owner");
    }

    /// `importance_score` is COMPUTED at insert (per kind) and SURFACED by `list`
    /// — previously the read SELECT dropped the column and the insert never bound
    /// it, so every notification reported 0.5/null. Guards both halves of the fix.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn list_surfaces_computed_importance_per_kind() {
        use aero_common::NotificationKind;
        let p = pool();
        let repo = NotificationRepo::new(p.clone());
        let (recipient, room, message, actor) = fixture(&p).await;

        // A Mention scores 1.0; a Reaction scores 0.3 (per importance_for).
        repo.insert(recipient, room, message, NotificationKind::Mention, Some(actor)).await.unwrap();
        repo.insert(recipient, room, message, NotificationKind::Reaction, Some(actor)).await.unwrap();

        let list = repo.list(recipient, None, false, Some(10)).await.unwrap();
        let mention = list.iter().find(|n| n.kind == NotificationKind::Mention).expect("mention present");
        let reaction = list.iter().find(|n| n.kind == NotificationKind::Reaction).expect("reaction present");
        assert!((mention.importance_score - 1.0).abs() < 1e-6, "mention importance computed + surfaced, got {}", mention.importance_score);
        assert!((reaction.importance_score - 0.3).abs() < 1e-6, "reaction importance computed + surfaced, got {}", reaction.importance_score);
        // Non-aggregate notifications surface aggregate_count = None (not a hardcoded default).
        assert!(mention.aggregate_count.is_none());
    }
}
