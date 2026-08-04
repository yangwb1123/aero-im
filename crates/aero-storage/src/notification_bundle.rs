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

use aero_common::{
    MessageId, NotificationId, NotificationKind, NotifyTarget, ParticipantId, RoomEvent, RoomId,
};
use sqlx::{PgPool, Postgres, Transaction};

use crate::event_outbox::{EventOutboxKind, EventOutboxRepo};
use crate::message_side_effect::MessageSideEffectRepo;

/// Default delay before flushing a bundle (in seconds).
const DEFAULT_BUNDLE_DELAY: u64 = 30;

/// Minimum delay (seconds) to prevent a tight loop from starving the inbox.
const MIN_BUNDLE_DELAY: u64 = 10;
const BUNDLE_DELIVERY_NAMESPACE: uuid::Uuid = uuid::uuid!("37301db9-9690-4ad7-8324-12d5cdd730a0");

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
        Self {
            pool,
            deadline: Duration::from_secs(secs),
        }
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

    /// Batch-insert one message's reply bundles with a deterministic delivery
    /// id. Replays after a side-effect worker crash are collapsed by the partial
    /// unique index from migration 0165.
    pub async fn insert_many_idempotent(
        &self,
        room: RoomId,
        message: MessageId,
        actor: ParticipantId,
        thread_root: Option<MessageId>,
        recipients: &[(ParticipantId, NotificationKind)],
        delivery_id: uuid::Uuid,
        source: Option<(uuid::Uuid, i32)>,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let live =
            sqlx::query_as::<_, (Option<time::OffsetDateTime>, Option<time::OffsetDateTime>)>(
                r"SELECT deleted_at, expires_at
                FROM messages
               WHERE id = $1
               FOR UPDATE",
            )
            .bind(message.to_uuid())
            .fetch_optional(&mut *tx)
            .await?
            .is_some_and(|(deleted_at, expires_at)| {
                deleted_at.is_none()
                    && expires_at.map_or(true, |expires_at| {
                        expires_at > time::OffsetDateTime::now_utc()
                    })
            });
        if !live {
            return complete_bundle_source(tx, source).await;
        }

        let candidate_ids: Vec<uuid::Uuid> = recipients
            .iter()
            .map(|(participant, _)| participant.to_uuid())
            .collect();
        let current_members: std::collections::HashSet<uuid::Uuid> = if candidate_ids.is_empty() {
            std::collections::HashSet::new()
        } else {
            sqlx::query_scalar::<_, uuid::Uuid>(
                r"SELECT participant_id
                    FROM room_members
                   WHERE room_id = $1
                     AND participant_id = ANY($2)
                   FOR KEY SHARE",
            )
            .bind(room.to_uuid())
            .bind(&candidate_ids)
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .collect()
        };
        let recipients: Vec<(ParticipantId, NotificationKind)> = recipients
            .iter()
            .copied()
            .filter(|(participant, _)| current_members.contains(&participant.to_uuid()))
            .collect();
        if recipients.is_empty() {
            return complete_bundle_source(tx, source).await;
        }

        let ids: Vec<uuid::Uuid> = recipients.iter().map(|_| uuid::Uuid::new_v4()).collect();
        let participants: Vec<uuid::Uuid> = recipients
            .iter()
            .map(|(participant, _)| participant.to_uuid())
            .collect();
        let kinds: Vec<String> = recipients
            .iter()
            .map(|(_, kind)| kind.as_str().to_owned())
            .collect();
        sqlx::query(
            r"INSERT INTO notification_bundles
                 (id, participant_id, room_id, message_id, kind, actor_id,
                  thread_root, delivery_id)
              SELECT u.id, u.participant_id, $4, $5, u.kind, $6, $7, $8
                FROM UNNEST($1::uuid[], $2::uuid[], $3::text[])
                     AS u(id, participant_id, kind)
              ON CONFLICT (delivery_id, participant_id)
                  WHERE delivery_id IS NOT NULL
                  DO NOTHING",
        )
        .bind(ids)
        .bind(participants)
        .bind(kinds)
        .bind(room.to_uuid())
        .bind(message.to_uuid())
        .bind(actor.to_uuid())
        .bind(thread_root.map(|message_id| message_id.to_uuid()))
        .bind(delivery_id)
        .execute(&mut *tx)
        .await?;
        if !complete_source_claim_in_tx(&mut tx, source, time::OffsetDateTime::now_utc()).await? {
            tx.rollback().await?;
            return Ok(false);
        }
        tx.commit().await?;
        Ok(true)
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
    pub async fn flush(&self) -> Result<FlushResult, sqlx::Error> {
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
                    let mut g = BundleGroup {
                        key,
                        members: Vec::new(),
                    };
                    g.members.push(b);
                    groups.push(g);
                }
            } else {
                let mut g = BundleGroup {
                    key,
                    members: Vec::new(),
                };
                g.members.push(b);
                groups.push(g);
            }
        }

        // 3. For each group, either insert one AggregateReply or one Reply, plus
        // its realtime outbox event on this same transaction.
        let mut inserted: Vec<InsertedNotification> = Vec::new();
        let mut all_ids: Vec<uuid::Uuid> = Vec::new();

        for g in &groups {
            let n = g.members.len();
            let first = &g.members[0];
            let last = &g.members[n - 1];

            let notif_id = NotificationId::new();
            let pid = first.participant_id;
            let room_id = first.room_id;
            let msg_id = last.message_id; // use the latest message as representative
            let created = time::OffsetDateTime::now_utc();
            for b in &g.members {
                all_ids.push(b.id);
            }

            // Lock/recheck the representative message and current membership.
            // Invalid/deleted/departed groups are consumed without producing an
            // inbox row or event.
            let current = sqlx::query_as::<
                _,
                (
                    Option<time::OffsetDateTime>,
                    Option<time::OffsetDateTime>,
                    uuid::Uuid,
                ),
            >(
                r"SELECT deleted_at, expires_at, sender_id
                    FROM messages
                   WHERE id = $1
                   FOR UPDATE",
            )
            .bind(msg_id)
            .fetch_optional(&mut *tx)
            .await?;
            let Some((deleted_at, expires_at, sender_id)) = current else {
                continue;
            };
            if deleted_at.is_some() || expires_at.is_some_and(|expires_at| expires_at <= created) {
                continue;
            }
            let still_member = sqlx::query_scalar::<_, uuid::Uuid>(
                r"SELECT participant_id
                    FROM room_members
                   WHERE room_id = $1 AND participant_id = $2
                   FOR KEY SHARE",
            )
            .bind(room_id)
            .bind(pid)
            .fetch_optional(&mut *tx)
            .await?
            .is_some();
            if !still_member {
                continue;
            }

            let actor = last.actor_id.unwrap_or(sender_id);
            let kind = if matches!(first.kind.as_str(), "mention") {
                NotificationKind::Mention
            } else if n == 1 {
                NotificationKind::Reply
            } else {
                NotificationKind::AggregateReply
            };
            let aggregate_count = if kind == NotificationKind::AggregateReply {
                i32::try_from(n).unwrap_or(i32::MAX)
            } else {
                1
            };
            let bundle_ids = g.members.iter().map(|bundle| bundle.id).collect::<Vec<_>>();
            let delivery_id = bundle_delivery_id(&bundle_ids, pid, room_id, first.thread_root);
            sqlx::query(
                r"INSERT INTO notifications
                     (id, participant_id, room_id, message_id, kind, actor_id,
                      created_at, aggregate_count, delivery_id, importance_score)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                   ON CONFLICT (delivery_id, participant_id)
                       WHERE delivery_id IS NOT NULL
                       DO NOTHING",
            )
            .bind(notif_id.to_uuid())
            .bind(pid)
            .bind(room_id)
            .bind(msg_id)
            .bind(kind.as_str())
            .bind(actor)
            .bind(created)
            .bind(aggregate_count)
            .bind(delivery_id)
            .bind(aero_common::model::importance_for(&kind))
            .execute(&mut *tx)
            .await?;

            let message = MessageId::from_uuid(msg_id);
            let room = RoomId::from_uuid(room_id);
            let participant = ParticipantId::from_uuid(pid);
            let actor = ParticipantId::from_uuid(actor);
            EventOutboxRepo::insert_room_event_in_tx(
                &mut tx,
                message,
                room,
                EventOutboxKind::Notify,
                &RoomEvent::NotifyBatch {
                    room_id: room,
                    message_id: message,
                    by: actor,
                    delivery_id,
                    recipients: vec![NotifyTarget { participant, kind }],
                },
                None,
                Some(delivery_id),
            )
            .await?;

            inserted.push(InsertedNotification {
                id: notif_id,
                participant,
                room,
                message,
                actor,
                delivery_id,
                kind,
            });
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

async fn complete_source_claim_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    source: Option<(uuid::Uuid, i32)>,
    now: time::OffsetDateTime,
) -> Result<bool, sqlx::Error> {
    let Some((id, attempts)) = source else {
        return Ok(true);
    };
    MessageSideEffectRepo::complete_in_tx(tx, id, attempts, now).await
}

async fn complete_bundle_source(
    mut tx: Transaction<'_, Postgres>,
    source: Option<(uuid::Uuid, i32)>,
) -> Result<bool, sqlx::Error> {
    if !complete_source_claim_in_tx(&mut tx, source, time::OffsetDateTime::now_utc()).await? {
        tx.rollback().await?;
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
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

fn bundle_delivery_id(
    bundle_ids: &[uuid::Uuid],
    participant: uuid::Uuid,
    room: uuid::Uuid,
    thread_root: Option<uuid::Uuid>,
) -> uuid::Uuid {
    let mut ids = bundle_ids.to_vec();
    ids.sort_unstable();
    let mut name = format!("{participant}:{room}:{}:", thread_root.unwrap_or_default());
    for id in ids {
        name.push_str(&id.to_string());
        name.push(':');
    }
    uuid::Uuid::new_v5(&BUNDLE_DELIVERY_NAMESPACE, name.as_bytes())
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
    pub message: MessageId,
    pub actor: ParticipantId,
    pub delivery_id: uuid::Uuid,
    pub kind: NotificationKind,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{MessageRepo, NewMessage};
    use crate::participant::NewHuman;
    use crate::{
        MessageSideEffectKind, MessageSideEffectRepo, ParticipantRepo, RoomRepo, WorkspaceRepo,
    };
    use aero_common::{Block, RoomEvent, RoomKind, WorkspaceId, WorkspaceRole};

    #[test]
    fn bundled_delivery_id_is_stable_across_row_order_and_group_specific() {
        let first = uuid::Uuid::new_v4();
        let second = uuid::Uuid::new_v4();
        let participant = uuid::Uuid::new_v4();
        let room = uuid::Uuid::new_v4();
        let root = Some(uuid::Uuid::new_v4());
        let forward = bundle_delivery_id(&[first, second], participant, room, root);
        let reverse = bundle_delivery_id(&[second, first], participant, room, root);
        assert_eq!(forward, reverse);
        assert_ne!(
            forward,
            bundle_delivery_id(&[first, second], uuid::Uuid::new_v4(), room, root)
        );
        assert_ne!(
            forward,
            bundle_delivery_id(&[first, second], participant, room, None)
        );
    }

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(8)
            .connect_lazy(&url)
            .expect("valid DATABASE_URL")
    }

    async fn participant(pool: &PgPool, label: &str) -> aero_common::Participant {
        let unique = uuid::Uuid::new_v4();
        let participant = ParticipantRepo::new(pool.clone())
            .create_human(NewHuman {
                email: format!("{label}-{unique}@example.test"),
                display_name: format!("{label}-{unique}"),
                password_hash: "test".into(),
            })
            .await
            .unwrap();
        WorkspaceRepo::new(pool.clone())
            .add_member(
                WorkspaceId::from_uuid(uuid::Uuid::nil()),
                participant.id,
                WorkspaceRole::Member,
            )
            .await
            .unwrap();
        participant
    }

    async fn message(
        repo: &MessageRepo,
        room: RoomId,
        sender: ParticipantId,
        text: &str,
    ) -> aero_common::Message {
        repo.insert_outboxed(
            NewMessage {
                room_id: room,
                sender_id: sender,
                blocks: vec![Block::text(text)],
                reply_to: None,
                metadata: serde_json::Value::Null,
                expires_at: None,
            },
            None,
            Vec::new(),
            None,
        )
        .await
        .unwrap()
        .message()
        .clone()
    }

    async fn notification_claim(pool: &PgPool, message: MessageId) -> (uuid::Uuid, i32) {
        let jobs = MessageSideEffectRepo::new(pool.clone())
            .claim_for_message(
                message,
                time::OffsetDateTime::now_utc(),
                time::Duration::seconds(30),
            )
            .await
            .unwrap();
        let job = jobs
            .iter()
            .find(|job| job.kind == MessageSideEffectKind::Notifications)
            .expect("notification side effect");
        (job.id, job.attempts)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres with migrations through 0165"]
    async fn flush_uses_real_identity_stable_delivery_and_suppresses_departed_recipient() {
        let pool = pool();
        let actor_one = participant(&pool, "bundle-actor-one").await;
        let actor_two = participant(&pool, "bundle-actor-two").await;
        let recipient = participant(&pool, "bundle-recipient").await;
        let rooms = RoomRepo::new(pool.clone());
        let room = rooms
            .create(RoomKind::Group, Some("bundle-outbox".into()), actor_one.id)
            .await
            .unwrap();
        rooms.add_member(room.id, actor_two.id).await.unwrap();
        rooms.add_member(room.id, recipient.id).await.unwrap();
        let messages = MessageRepo::new(pool.clone());
        let first = message(&messages, room.id, actor_one.id, "first reply").await;
        let second = message(&messages, room.id, actor_two.id, "second reply").await;
        let repo = NotificationBundleRepo {
            pool: pool.clone(),
            deadline: Duration::from_secs(10),
        };
        let first_delivery = uuid::Uuid::new_v4();
        let second_delivery = uuid::Uuid::new_v4();
        let recipients = [(recipient.id, NotificationKind::Reply)];

        assert!(repo
            .insert_many_idempotent(
                room.id,
                first.id,
                actor_one.id,
                Some(first.id),
                &recipients,
                first_delivery,
                Some(notification_claim(&pool, first.id).await),
            )
            .await
            .unwrap());
        assert!(repo
            .insert_many_idempotent(
                room.id,
                second.id,
                actor_two.id,
                Some(first.id),
                &recipients,
                second_delivery,
                Some(notification_claim(&pool, second.id).await),
            )
            .await
            .unwrap());
        // A retry after the source worker crashed before observing commit is
        // idempotent at the durable bundle projection.
        assert!(repo
            .insert_many_idempotent(
                room.id,
                first.id,
                actor_one.id,
                Some(first.id),
                &recipients,
                first_delivery,
                None,
            )
            .await
            .unwrap());
        sqlx::query(
            r"UPDATE notification_bundles
                  SET created_at = CASE delivery_id
                      WHEN $1 THEN now() - interval '30 seconds'
                      ELSE now() - interval '20 seconds'
                  END
                WHERE delivery_id = ANY($2)",
        )
        .bind(first_delivery)
        .bind(&[first_delivery, second_delivery])
        .execute(&pool)
        .await
        .unwrap();
        let bundle_ids: Vec<uuid::Uuid> =
            sqlx::query_scalar("SELECT id FROM notification_bundles WHERE delivery_id = ANY($1)")
                .bind(&[first_delivery, second_delivery])
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(bundle_ids.len(), 2, "retry did not duplicate a bundle");
        let expected_delivery = bundle_delivery_id(
            &bundle_ids,
            recipient.id.to_uuid(),
            room.id.to_uuid(),
            Some(first.id.to_uuid()),
        );

        let flushed = repo.flush().await.unwrap();
        assert_eq!(
            (flushed.bundles_consumed, flushed.notifications_inserted),
            (2, 1)
        );
        let inserted = flushed.inserted.first().expect("aggregate notification");
        assert_eq!(inserted.participant, recipient.id);
        assert_eq!(inserted.message, second.id);
        assert_eq!(
            inserted.actor, actor_two.id,
            "actor follows representative message"
        );
        assert_eq!(inserted.delivery_id, expected_delivery);
        assert_eq!(inserted.kind, NotificationKind::AggregateReply);

        let row: (uuid::Uuid, uuid::Uuid, i32, uuid::Uuid) = sqlx::query_as(
            r"SELECT message_id, actor_id, aggregate_count, delivery_id
                FROM notifications
               WHERE participant_id = $1 AND delivery_id = $2",
        )
        .bind(recipient.id.to_uuid())
        .bind(expected_delivery)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            row,
            (
                second.id.to_uuid(),
                actor_two.id.to_uuid(),
                2,
                expected_delivery
            )
        );
        let payload: serde_json::Value = sqlx::query_scalar(
            "SELECT payload FROM event_outbox WHERE event_id = $1 AND event_kind = 'notify'",
        )
        .bind(expected_delivery)
        .fetch_one(&pool)
        .await
        .unwrap();
        let event: RoomEvent = serde_json::from_value(payload).unwrap();
        let RoomEvent::NotifyBatch {
            message_id,
            by,
            delivery_id,
            recipients: event_recipients,
            ..
        } = event
        else {
            panic!("expected notify batch");
        };
        assert_eq!(message_id, second.id);
        assert_eq!(by, actor_two.id);
        assert_eq!(delivery_id, expected_delivery);
        assert_eq!(event_recipients.len(), 1);
        assert_eq!(event_recipients[0].participant, recipient.id);
        assert_eq!(repo.flush().await.unwrap().bundles_consumed, 0);

        let departed = message(&messages, room.id, actor_one.id, "departed recipient").await;
        let departed_delivery = uuid::Uuid::new_v4();
        assert!(repo
            .insert_many_idempotent(
                room.id,
                departed.id,
                actor_one.id,
                Some(first.id),
                &recipients,
                departed_delivery,
                Some(notification_claim(&pool, departed.id).await),
            )
            .await
            .unwrap());
        rooms.remove_member(room.id, recipient.id).await.unwrap();
        sqlx::query(
            "UPDATE notification_bundles SET created_at = now() - interval '30 seconds' WHERE delivery_id = $1",
        )
        .bind(departed_delivery)
        .execute(&pool)
        .await
        .unwrap();
        let stale = repo.flush().await.unwrap();
        assert_eq!(
            (stale.bundles_consumed, stale.notifications_inserted),
            (1, 0)
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM notifications WHERE message_id = $1"
            )
            .bind(departed.id.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap(),
            0
        );
        assert!(repo
            .insert_many_idempotent(
                room.id,
                departed.id,
                actor_one.id,
                Some(first.id),
                &recipients,
                uuid::Uuid::new_v4(),
                None,
            )
            .await
            .unwrap());
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM notification_bundles WHERE message_id = $1",
            )
            .bind(departed.id.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap(),
            0,
            "departed recipient is also filtered at bundle insertion"
        );

        sqlx::query(
            "DELETE FROM notifications
              WHERE message_id IN (SELECT id FROM messages WHERE room_id = $1)",
        )
        .bind(room.id.to_uuid())
        .execute(&pool)
        .await
        .ok();
        sqlx::query(
            "DELETE FROM message_side_effect_jobs
              WHERE message_id IN (SELECT id FROM messages WHERE room_id = $1)",
        )
        .bind(room.id.to_uuid())
        .execute(&pool)
        .await
        .ok();
        sqlx::query(
            "DELETE FROM event_outbox
              WHERE message_id IN (SELECT id FROM messages WHERE room_id = $1)",
        )
        .bind(room.id.to_uuid())
        .execute(&pool)
        .await
        .ok();
        sqlx::query("DELETE FROM messages WHERE room_id = $1")
            .bind(room.id.to_uuid())
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM room_members WHERE room_id = $1")
            .bind(room.id.to_uuid())
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(room.id.to_uuid())
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = ANY($1)")
            .bind(&[
                actor_one.id.to_uuid(),
                actor_two.id.to_uuid(),
                recipient.id.to_uuid(),
            ])
            .execute(&pool)
            .await
            .ok();
    }
}
