//! Notification bundle repository (ROADMAP7 方向三: 通知聚合).
//!
//! Pending notification rows that accumulate over a short delay so multiple
//! replies to the same thread can be folded into one `AggregateReply`.
//! Backs `migrations/0143_notification_bundles.sql`.
//!
//! ## Flush semantics
//!
//! `flush()` selects all bundles older than `delay`, groups them by
//! (participant_id, room_id, thread_root), and for each group:
//!
//! - **Count > 1**: inserts one `AggregateReply` notification with
//!   `aggregate_count = N`. The last `message_id` in the group becomes the
//!   representative.
//! - **Count == 1**: inserts a plain `Reply` notification.
//!
//! Mentions (`kind = 'mention'`) are never bundled; each gets its own
//! notification immediately. The bundle table only delays Reply-type rows;
//! Mentions are inserted directly in the hot path.

use std::time::Duration;

use aero_common::{MessageId, NotificationId, NotificationKind, ParticipantId, RoomId};
use sqlx::PgPool;

/// Default delay before flushing a bundle (in seconds).
const DEFAULT_BUNDLE_DELAY: u64 = 30;

/// Minimum delay (seconds) to prevent a tight loop from starving the inbox.
const MIN_BUNDLE_DELAY: u64 = 10;

#[derive(Clone)]
pub struct NotificationBundleRepo {
    pool: PgPool,
    deadline: Duration,
}

impl NotificationBundleRepo {
    pub fn new(pool: PgPool) -> Self {
        let secs = std::env::var("AERO_BUNDLE_DEADLINE_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_BUNDLE_DELAY)
            .max(MIN_BUNDLE_DELAY);
        Self { pool, deadline: Duration::from_secs(secs) }
    }

    /// Insert one deferred notification bundle row.
    pub async fn insert(
        &self,
        participant: ParticipantId,
        room: RoomId,
        message: MessageId,
        kind: NotificationKind,
        actor: Option<ParticipantId>,
        thread_root: Option<MessageId>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO notification_bundles
                 (participant_id, room_id, message_id, kind, actor_id, thread_root)
               VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(participant.to_uuid())
        .bind(room.to_uuid())
        .bind(message.to_uuid())
        .bind(kind.as_str())
        .bind(actor.map(|a| a.to_uuid()))
        .bind(thread_root.map(|r| r.to_uuid()))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Flush all bundles that have been sitting longer than `deadline`.
    ///
    /// Returns a list of inserted notification IDs (those that survived
    /// aggregation) so the caller can publish a `NotifyBatch`. Returns the
    /// number of bundles consumed (deleted).
    ///
    /// # Errors
    ///
    /// Propagates any [`sqlx::Error`]. The flush is best-effort: a failure
    /// leaves bundles in the table and they will be retried next cycle.
    pub async fn flush(
        &self,
    ) -> Result<FlushResult, sqlx::Error> {
        

        // Flush MUST be transactional with row locks: it is spawned once PER NODE
        // (background.rs), so on a multi-node deploy two flushers tick concurrently.
        // Without locking, both SELECT the same expired bundles and both INSERT the
        // notifications → every recipient is notified TWICE (and the downstream
        // NotifyBatch delivery_id is regenerated per flush, so its `ON CONFLICT`
        // dedup can't collapse the duplicate). `FOR UPDATE SKIP LOCKED` claims each
        // bundle row for exactly one flusher; the other skips it. The INSERT+DELETE
        // then commit atomically with the claim.
        let mut tx = self.pool.begin().await?;

        // 1. Select expired bundles, claimed for THIS flusher only (skip rows another
        //    node already locked).
        let bundles = sqlx::query_as::<_, BundleRow>(
            r"SELECT id, participant_id, room_id, message_id, kind, actor_id, thread_root
                FROM notification_bundles
               WHERE created_at < now() - make_interval(secs => $1)
               ORDER BY participant_id, room_id, thread_root, created_at
               FOR UPDATE SKIP LOCKED",
        )
        .bind(self.deadline.as_secs() as f64)
        .fetch_all(&mut *tx)
        .await?;

        if bundles.is_empty() {
            return Ok(FlushResult::default());
        }

        // 2. Group by (participant_id, room_id, thread_root).
        let mut groups: Vec<BundleGroup> = Vec::new();
        for b in &bundles {
            let key = (b.participant_id, b.room_id, b.thread_root);
            let last = groups.last_mut();
            if let Some(prev) = last {
                if prev.key == key {
                    prev.members.push(b);
                } else {
                    let mut g = BundleGroup { key, members: Vec::new() };
                    g.members.push(b);
                    groups.push(g);
                }
            } else {
                let mut g = BundleGroup { key, members: Vec::new() };
                g.members.push(b);
                groups.push(g);
            }
        }

        // 3. For each group, either insert one AggregateReply or one Reply.
        let mut inserted: Vec<InsertedNotification> = Vec::new();
        let mut all_ids: Vec<uuid::Uuid> = Vec::new();

        for g in &groups {
            let n = g.members.len();
            let first = &g.members[0];
            let last = &g.members[n - 1];

            let notif_id = NotificationId::new();
            let pid = first.participant_id;
            let room_id = first.room_id;
            let msg_id = last.message_id;  // use the latest message as representative
            let actor = first.actor_id;
            let created = time::OffsetDateTime::now_utc();

            if matches!(first.kind.as_str(), "mention") || n == 1 {
                // Singles and mentions: insert as-is.
                sqlx::query(
                    r"INSERT INTO notifications
                         (id, participant_id, room_id, message_id, kind, actor_id, created_at)
                       VALUES ($1, $2, $3, $4, $5, $6, $7)",
                )
                .bind(notif_id.to_uuid())
                .bind(pid)
                .bind(room_id)
                .bind(msg_id)
                .bind(first.kind.as_str())
                .bind(actor)
                .bind(created)
                .execute(&mut *tx)
                .await?;

                inserted.push(InsertedNotification {
                    id: notif_id,
                    participant: ParticipantId::from_uuid(pid),
                    room: RoomId::from_uuid(room_id),
                    kind: match first.kind.as_str() {
                        "mention" => NotificationKind::Mention,
                        _ => NotificationKind::Reply,
                    },
                });
            } else {
                // N > 1, kind = reply: one AggregateReply.
                sqlx::query(
                    r"INSERT INTO notifications
                         (id, participant_id, room_id, message_id, kind, actor_id, created_at, aggregate_count)
                       VALUES ($1, $2, $3, $4, 'aggregate_reply', $5, $6, $7)",
                )
                .bind(notif_id.to_uuid())
                .bind(pid)
                .bind(room_id)
                .bind(msg_id)
                .bind(actor)
                .bind(created)
                .bind(n as i32)
                .execute(&mut *tx)
                .await?;

                inserted.push(InsertedNotification {
                    id: notif_id,
                    participant: ParticipantId::from_uuid(pid),
                    room: RoomId::from_uuid(room_id),
                    kind: NotificationKind::AggregateReply,
                });
            }

            // Collect bundle ids for deletion.
            for b in &g.members {
                all_ids.push(b.id);
            }
        }

        // 4. Delete consumed bundles (still inside the tx, so the claim + insert +
        //    delete commit atomically).
        if !all_ids.is_empty() {
            sqlx::query("DELETE FROM notification_bundles WHERE id = ANY($1)")
                .bind(&all_ids)
                .execute(&mut *tx)
                .await?;
        }

        tx.commit().await?;

        Ok(FlushResult {
            bundles_consumed: all_ids.len() as u64,
            notifications_inserted: inserted.len() as u64,
            inserted,
        })
    }

    /// Sweep stale bundles (data-lifecycle, GDPR right-to-erasure).
    /// Removes all bundles older than `cutoff` regardless of flush state.
    pub async fn sweep_before(&self, cutoff: time::OffsetDateTime) -> Result<u64, sqlx::Error> {
        let res = sqlx::query("DELETE FROM notification_bundles WHERE created_at < $1")
            .bind(cutoff)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }
}

/// A raw row from `notification_bundles`.
#[derive(Debug, Clone, sqlx::FromRow)]
struct BundleRow {
    id: uuid::Uuid,
    participant_id: uuid::Uuid,
    room_id: uuid::Uuid,
    message_id: uuid::Uuid,
    kind: String,
    actor_id: Option<uuid::Uuid>,
    thread_root: Option<uuid::Uuid>,
}

#[derive(Debug, Clone)]
struct BundleGroup<'a> {
    key: (uuid::Uuid, uuid::Uuid, Option<uuid::Uuid>),
    members: Vec<&'a BundleRow>,
}

/// Result of a single `flush()` call.
#[derive(Debug, Default)]
pub struct FlushResult {
    pub bundles_consumed: u64,
    pub notifications_inserted: u64,
    pub inserted: Vec<InsertedNotification>,
}

/// A notification that was materialised by flushing bundles.
#[derive(Debug, Clone)]
pub struct InsertedNotification {
    pub id: NotificationId,
    pub participant: ParticipantId,
    pub room: RoomId,
    pub kind: NotificationKind,
}
