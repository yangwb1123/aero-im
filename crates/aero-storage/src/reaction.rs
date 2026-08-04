//! Reaction repository.
//!
//! Reactions are (message, participant, emoji) triples. Toggling adds or removes
//! the row. Aggregates are computed on read into [`ReactionSummary`].

use std::collections::BTreeMap;

use aero_common::{
    Error, MessageId, ParticipantId, ReactionOp, ReactionSummary, RoomEvent, RoomId,
};
use sqlx::PgPool;

use crate::event_outbox::{EventOutboxKind, EventOutboxRepo};
use crate::message::authorization::{lock_effective_message_write_access, PostPolicy};
use crate::message::MessageRepo;

/// Cap on the per-emoji reactor preview returned by [`ReactionRepo::summaries_for`].
///
/// `count` stays exact; this only bounds the `participants` avatar-preview list so
/// a wildly-reacted message can't materialise an unbounded Vec (memory + JSON).
/// The UI shows the first `MAX_REACTORS_PREVIEW` reactors and a "+N" overflow.
const MAX_REACTORS_PREVIEW: i64 = 50;

/// Maximum encoded reaction token size accepted by every write path.
///
/// This preserves the original API contract: Unicode emoji and custom
/// `:shortcode:` text are both accepted, but the UTF-8 payload is bounded.
const MAX_EMOJI_BYTES: usize = 32;

/// One committed reaction toggle plus the durable event waiting for dispatch.
#[derive(Debug, Clone, Copy)]
pub struct OutboxedReactionToggle {
    pub room_id: RoomId,
    pub message_id: MessageId,
    pub message_sender: ParticipantId,
    pub op: ReactionOp,
    pub outbox_id: uuid::Uuid,
}

#[derive(Clone)]
pub struct ReactionRepo {
    pool: PgPool,
}

impl ReactionRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Toggle a reaction while authorization, message identity, the room cap,
    /// projection mutation, and realtime outbox append remain one transaction.
    ///
    /// Lock order is the platform-wide workspace → room → membership → message
    /// order. Announcement-only posting policy deliberately does not apply:
    /// members who cannot create messages there may still react to an existing
    /// message, matching the product's established behaviour.
    ///
    /// # Errors
    /// Returns [`Error::Invalid`] for an empty/oversized token or a cap breach,
    /// [`Error::Forbidden`] when effective room access was revoked, and
    /// [`Error::NotFound`] when the message is absent, deleted, or changes room
    /// while the operation is waiting. Database/outbox failures roll back the
    /// reaction mutation.
    pub async fn toggle_authorized_outboxed(
        &self,
        message: MessageId,
        participant: ParticipantId,
        emoji: &str,
        traceparent: Option<&str>,
    ) -> aero_common::Result<OutboxedReactionToggle> {
        if emoji.is_empty() || emoji.len() > MAX_EMOJI_BYTES {
            return Err(Error::Invalid("emoji length".into()));
        }

        let mut tx = self.pool.begin().await?;
        // Resolve the immutable routing edge without a row lock. Authorization
        // then acquires the canonical workspace/room/membership locks before the
        // message aggregate itself is locked and revalidated.
        let resolved_room = sqlx::query_scalar::<_, uuid::Uuid>(
            "SELECT room_id
               FROM messages
              WHERE id = $1
                AND deleted_at IS NULL",
        )
        .bind(message.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .map(RoomId::from_uuid)
        .ok_or_else(|| Error::NotFound(format!("live message {message}")))?;

        if lock_effective_message_write_access(
            &mut tx,
            resolved_room,
            participant,
            PostPolicy::Ignore,
        )
        .await?
        .is_none()
        {
            return Err(Error::Forbidden(
                "reaction room access was revoked before commit".into(),
            ));
        }

        let existing = MessageRepo::lock_message_in_tx(&mut tx, message).await?;
        let Some(existing) = existing else {
            return Err(Error::NotFound(format!("live message {message}")));
        };
        if existing.room_id != resolved_room || existing.deleted_at.is_some() {
            return Err(Error::NotFound(format!(
                "live message {message} in room {resolved_room}"
            )));
        }

        let removed = sqlx::query(
            r"DELETE FROM reactions
               WHERE message_id = $1 AND participant_id = $2 AND emoji = $3",
        )
        .bind(message.to_uuid())
        .bind(participant.to_uuid())
        .bind(emoji)
        .execute(&mut *tx)
        .await?;
        let op = if removed.rows_affected() > 0 {
            ReactionOp::Remove
        } else {
            // It is a new Add. The locked message serializes toggles on this
            // aggregate, making the room-level distinct-emoji cap race-free.
            let cap = sqlx::query_scalar::<_, Option<i32>>(
                "SELECT max_reactions_per_user
                   FROM rooms
                  WHERE id = $1",
            )
            .bind(resolved_room.to_uuid())
            .fetch_one(&mut *tx)
            .await?;
            if let Some(cap) = cap {
                let cap = i64::from(cap);
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(DISTINCT emoji)
                       FROM reactions
                      WHERE message_id = $1
                        AND participant_id = $2",
                )
                .bind(message.to_uuid())
                .bind(participant.to_uuid())
                .fetch_one(&mut *tx)
                .await?;
                if count >= cap {
                    return Err(Error::Invalid(format!(
                        "reaction limit: max {cap} reactions per message"
                    )));
                }
            }

            sqlx::query(
                r"INSERT INTO reactions (message_id, participant_id, emoji)
                   VALUES ($1, $2, $3)",
            )
            .bind(message.to_uuid())
            .bind(participant.to_uuid())
            .bind(emoji)
            .execute(&mut *tx)
            .await?;
            ReactionOp::Add
        };

        let event = RoomEvent::Reaction {
            room_id: resolved_room,
            message_id: message,
            participant,
            emoji: emoji.to_owned(),
            op,
        };
        let outbox_id = EventOutboxRepo::insert_room_event_in_tx(
            &mut tx,
            message,
            resolved_room,
            EventOutboxKind::Reaction,
            &event,
            traceparent.map(str::to_owned),
            None,
        )
        .await?;
        tx.commit().await?;
        Ok(OutboxedReactionToggle {
            room_id: resolved_room,
            message_id: message,
            message_sender: existing.sender_id,
            op,
            outbox_id,
        })
    }

    /// Aggregated reactions for a batch of messages. Returns
    /// `message_id → Vec<ReactionSummary>` in stable emoji order.
    pub async fn summaries_for(
        &self,
        message_ids: &[MessageId],
    ) -> Result<BTreeMap<MessageId, Vec<ReactionSummary>>, sqlx::Error> {
        if message_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let uuids: Vec<uuid::Uuid> = message_ids.iter().map(MessageId::to_uuid).collect();
        // Aggregate in SQL rather than streaming every raw reaction row and
        // grouping in Rust: `COUNT(*)` keeps the per-emoji tally EXACT, while the
        // reactor list is a BOUNDED preview of the earliest reactors (the avatar
        // strip; the UI renders "+N" beyond it). This caps both the rows
        // transferred and the per-emoji participant Vec — a message with 10k of
        // one emoji previously materialised a 10k-element list in memory + JSON.
        let rows = sqlx::query_as::<_, (uuid::Uuid, String, i64, Vec<uuid::Uuid>)>(
            r"SELECT message_id,
                      emoji,
                      COUNT(*) AS count,
                      (array_agg(participant_id ORDER BY created_at ASC))[1:$2] AS reactors
               FROM reactions
               WHERE message_id = ANY($1)
               GROUP BY message_id, emoji
               ORDER BY message_id, MIN(created_at) ASC",
        )
        .bind(&uuids)
        .bind(MAX_REACTORS_PREVIEW)
        .fetch_all(&self.pool)
        .await?;

        Ok(Self::group_summaries(rows))
    }

    /// Like [`summaries_for`](Self::summaries_for) but ONLY for messages in rooms
    /// `viewer` may currently access. The query requires room + workspace
    /// membership, an active account, no workspace deactivation, and mandatory
    /// 2FA enrollment. Keeping those predicates in this aggregate query avoids
    /// both an N-query service filter and a revocation race between authorization
    /// and reading reaction counts/reactor ids. Inaccessible ids are omitted.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn summaries_for_accessible(
        &self,
        viewer: ParticipantId,
        message_ids: &[MessageId],
    ) -> Result<BTreeMap<MessageId, Vec<ReactionSummary>>, sqlx::Error> {
        if message_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let uuids: Vec<uuid::Uuid> = message_ids.iter().map(MessageId::to_uuid).collect();
        let rows = sqlx::query_as::<_, (uuid::Uuid, String, i64, Vec<uuid::Uuid>)>(
            r"SELECT r.message_id,
                      r.emoji,
                      COUNT(*) AS count,
                      (array_agg(r.participant_id ORDER BY r.created_at ASC))[1:$2] AS reactors
               FROM reactions r
               JOIN messages m
                 ON m.id = r.message_id
               JOIN rooms room
                 ON room.id = m.room_id
               JOIN workspaces workspace
                 ON workspace.id = room.workspace_id
               JOIN room_members rm
                 ON rm.room_id = m.room_id
                AND rm.participant_id = $3
               JOIN workspace_members wm
                 ON wm.workspace_id = room.workspace_id
                AND wm.participant_id = rm.participant_id
               JOIN participants participant
                 ON participant.id = rm.participant_id
                AND participant.deleted_at IS NULL
               LEFT JOIN workspace_deactivations deactivated
                 ON deactivated.workspace_id = room.workspace_id
                AND deactivated.participant_id = rm.participant_id
               LEFT JOIN totp_secrets totp
                 ON totp.participant_id = rm.participant_id
               WHERE r.message_id = ANY($1)
                 AND deactivated.participant_id IS NULL
                 AND (
                     participant.kind <> 'human'
                     OR NOT workspace.require_2fa
                     OR COALESCE(totp.activated, false)
               )
               GROUP BY r.message_id, r.emoji
               ORDER BY r.message_id, MIN(r.created_at) ASC",
        )
        .bind(&uuids)
        .bind(MAX_REACTORS_PREVIEW)
        .bind(viewer.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(Self::group_summaries(rows))
    }

    /// Fold aggregate `(message_id, emoji, count, reactors)` rows into the
    /// per-message summary map (shared by the scoped + unscoped queries).
    fn group_summaries(
        rows: Vec<(uuid::Uuid, String, i64, Vec<uuid::Uuid>)>,
    ) -> BTreeMap<MessageId, Vec<ReactionSummary>> {
        let mut out: BTreeMap<MessageId, Vec<ReactionSummary>> = BTreeMap::new();
        for (mid, emoji, count, reactors) in rows {
            out.entry(MessageId::from_uuid(mid))
                .or_default()
                .push(ReactionSummary {
                    emoji,
                    count: u32::try_from(count).unwrap_or(u32::MAX),
                    participants: reactors.into_iter().map(ParticipantId::from_uuid).collect(),
                });
        }
        out
    }
}

#[cfg(test)]
mod db_tests {
    use super::{MessageId, ReactionRepo, MAX_REACTORS_PREVIEW};
    use sqlx::PgPool;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    #[tokio::test]
    async fn toggle_rejects_invalid_emoji_bytes_before_database_access() {
        let repo = ReactionRepo::new(pool());
        let oversized_ascii = "x".repeat(33);
        let oversized_unicode = "👍".repeat(9);
        for emoji in ["", oversized_ascii.as_str(), oversized_unicode.as_str()] {
            assert!(matches!(
                repo.toggle_authorized_outboxed(
                    MessageId::new(),
                    aero_common::ParticipantId::new(),
                    emoji,
                    None,
                )
                .await,
                Err(aero_common::Error::Invalid(_))
            ));
        }
    }

    /// `summaries_for` keeps `count` EXACT while bounding the reactor preview:
    /// 51 reactors on one emoji ⇒ `count == 51`, but only `MAX_REACTORS_PREVIEW`
    /// participants are previewed. Also exercises the parameterized array slice.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn summaries_count_exact_reactor_preview_capped() {
        let p = pool();
        let repo = ReactionRepo::new(p.clone());

        let sender = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(sender)
            .bind(format!("reaction-sender-{sender}"))
            .execute(&p)
            .await
            .expect("sender");
        let room = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, workspace_id)
             VALUES ($1,'group',$2,$3,'00000000-0000-0000-0000-000000000000')",
        )
        .bind(room)
        .bind("reaction-room")
        .bind(sender)
        .execute(&p)
        .await
        .expect("room");
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, participant_id, role)
             VALUES ('00000000-0000-0000-0000-000000000000',$1,'member')
             ON CONFLICT DO NOTHING",
        )
        .bind(sender)
        .execute(&p)
        .await
        .expect("sender workspace membership");
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role)
             VALUES ($1,$2,'owner')",
        )
        .bind(room)
        .bind(sender)
        .execute(&p)
        .await
        .expect("sender room membership");
        let msg = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO messages (id, room_id, sender_id, blocks, metadata)
             VALUES ($1,$2,$3,'[]'::jsonb,'{}'::jsonb)",
        )
        .bind(msg)
        .bind(room)
        .bind(sender)
        .execute(&p)
        .await
        .expect("message");

        // 51 distinct reactors, all the same emoji (count 51 > preview cap 50).
        let reactors: Vec<uuid::Uuid> = (0..51).map(|_| uuid::Uuid::new_v4()).collect();
        sqlx::query(
            "INSERT INTO participants (id, kind, display_name)
             SELECT u, 'human', 'reactor-' || u FROM unnest($1::uuid[]) u",
        )
        .bind(&reactors)
        .execute(&p)
        .await
        .expect("reactors");
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, participant_id, role)
             SELECT '00000000-0000-0000-0000-000000000000', u, 'member'
               FROM unnest($1::uuid[]) u
             ON CONFLICT DO NOTHING",
        )
        .bind(&reactors)
        .execute(&p)
        .await
        .expect("reactor workspace memberships");
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role)
             SELECT $1, u, 'member' FROM unnest($2::uuid[]) u",
        )
        .bind(room)
        .bind(&reactors)
        .execute(&p)
        .await
        .expect("reactor room memberships");
        sqlx::query(
            "INSERT INTO reactions (message_id, participant_id, emoji)
             SELECT $1, u, '👍' FROM unnest($2::uuid[]) u",
        )
        .bind(msg)
        .bind(&reactors)
        .execute(&p)
        .await
        .expect("reactions");

        let mid = MessageId::from_uuid(msg);
        let summaries = repo.summaries_for(&[mid]).await.expect("summaries");
        let v = summaries.get(&mid).expect("message present");
        assert_eq!(v.len(), 1, "one emoji group");
        assert_eq!(v[0].emoji, "👍");
        assert_eq!(v[0].count, 51, "count is exact across ALL reactors");
        assert_eq!(
            v[0].participants.len(),
            usize::try_from(MAX_REACTORS_PREVIEW).unwrap(),
            "reactor preview capped at MAX_REACTORS_PREVIEW"
        );

        // Cleanup.
        sqlx::query("DELETE FROM reactions WHERE message_id = $1")
            .bind(msg)
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM messages WHERE id = $1")
            .bind(msg)
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(room)
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = $1 OR id = ANY($2)")
            .bind(sender)
            .bind(&reactors)
            .execute(&p)
            .await
            .ok();
    }

    /// `summaries_for_accessible` only returns reactions for messages in rooms the
    /// viewer belongs to — a batch can't leak counts/reactor ids cross-room (IDOR).
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn summaries_for_accessible_is_membership_scoped() {
        use aero_common::ParticipantId;
        let p = pool();
        let repo = ReactionRepo::new(p.clone());

        let viewer = ParticipantId::new();
        let sender = ParticipantId::new();
        for id in [viewer, sender] {
            sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
                .bind(id.to_uuid())
                .bind(format!("acc-{id}"))
                .execute(&p)
                .await
                .expect("participant");
        }
        for id in [viewer, sender] {
            sqlx::query(
                "INSERT INTO workspace_members (workspace_id, participant_id, role)
                 VALUES ('00000000-0000-0000-0000-000000000000',$1,'member')
                 ON CONFLICT DO NOTHING",
            )
            .bind(id.to_uuid())
            .execute(&p)
            .await
            .expect("join default workspace");
        }
        // Room A: viewer IS a member. Room B: viewer is NOT.
        let mk_room_msg = |room: uuid::Uuid, msg: uuid::Uuid| {
            let p = p.clone();
            async move {
                sqlx::query(
                    "INSERT INTO rooms (id, kind, name, created_by, workspace_id)
                     VALUES ($1,'group','r',$2,'00000000-0000-0000-0000-000000000000')",
                )
                .bind(room)
                .bind(sender.to_uuid())
                .execute(&p)
                .await
                .expect("room");
                sqlx::query(
                    "INSERT INTO messages (id, room_id, sender_id, blocks, metadata)
                     VALUES ($1,$2,$3,'[]'::jsonb,'{}'::jsonb)",
                )
                .bind(msg)
                .bind(room)
                .bind(sender.to_uuid())
                .execute(&p)
                .await
                .expect("msg");
                sqlx::query(
                    "INSERT INTO room_members (room_id, participant_id, role)
                     VALUES ($1,$2,'owner')",
                )
                .bind(room)
                .bind(sender.to_uuid())
                .execute(&p)
                .await
                .expect("sender room membership");
                sqlx::query(
                    "INSERT INTO reactions (message_id, participant_id, emoji) VALUES ($1,$2,'👍')",
                )
                .bind(msg)
                .bind(sender.to_uuid())
                .execute(&p)
                .await
                .expect("reaction");
            }
        };
        let (room_a, msg_a) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        let (room_b, msg_b) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        mk_room_msg(room_a, msg_a).await;
        mk_room_msg(room_b, msg_b).await;
        // Viewer joins ONLY room A.
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role) VALUES ($1,$2,'member')",
        )
        .bind(room_a)
        .bind(viewer.to_uuid())
        .execute(&p)
        .await
        .expect("join A");
        let (mid_a, mid_b) = (MessageId::from_uuid(msg_a), MessageId::from_uuid(msg_b));
        let out = repo
            .summaries_for_accessible(viewer, &[mid_a, mid_b])
            .await
            .expect("scoped");
        assert!(out.contains_key(&mid_a), "viewer sees room-A reactions");
        assert!(
            !out.contains_key(&mid_b),
            "room-B reactions are NOT leaked to a non-member"
        );

        // The unscoped variant would have leaked both — proving the scoping matters.
        let unscoped = repo.summaries_for(&[mid_a, mid_b]).await.expect("unscoped");
        assert!(
            unscoped.contains_key(&mid_b),
            "unscoped sees both (the IDOR the scoped form closes)"
        );

        for r in [room_a, room_b] {
            sqlx::query("DELETE FROM reactions WHERE message_id IN (SELECT id FROM messages WHERE room_id=$1)")
                .bind(r)
                .execute(&p)
                .await
                .ok();
            sqlx::query("DELETE FROM messages WHERE room_id = $1")
                .bind(r)
                .execute(&p)
                .await
                .ok();
            sqlx::query("DELETE FROM rooms WHERE id = $1")
                .bind(r)
                .execute(&p)
                .await
                .ok();
        }
        sqlx::query("DELETE FROM participants WHERE id = $1 OR id = $2")
            .bind(viewer.to_uuid())
            .bind(sender.to_uuid())
            .execute(&p)
            .await
            .ok();
    }

    /// The authorized toggle enforces the per-(message, participant)
    /// distinct-emoji cap under the message aggregate lock. A Remove remains
    /// allowed at the cap and frees a slot.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn authorized_toggle_enforces_distinct_emoji_cap() {
        use aero_common::{ParticipantId, ReactionOp};
        let p = pool();
        let repo = ReactionRepo::new(p.clone());

        let sender = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(sender)
            .bind(format!("cap-sender-{sender}"))
            .execute(&p)
            .await
            .expect("sender");
        let room = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, workspace_id)
             VALUES ($1,'group',$2,$3,'00000000-0000-0000-0000-000000000000')",
        )
        .bind(room)
        .bind("cap-room")
        .bind(sender)
        .execute(&p)
        .await
        .expect("room");
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, participant_id, role)
             VALUES ('00000000-0000-0000-0000-000000000000',$1,'member')
             ON CONFLICT DO NOTHING",
        )
        .bind(sender)
        .execute(&p)
        .await
        .expect("workspace membership");
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role)
             VALUES ($1,$2,'owner')",
        )
        .bind(room)
        .bind(sender)
        .execute(&p)
        .await
        .expect("room membership");
        sqlx::query("UPDATE rooms SET max_reactions_per_user = 2 WHERE id = $1")
            .bind(room)
            .execute(&p)
            .await
            .expect("reaction cap");
        let msg = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO messages (id, room_id, sender_id, blocks, metadata)
             VALUES ($1,$2,$3,'[]'::jsonb,'{}'::jsonb)",
        )
        .bind(msg)
        .bind(room)
        .bind(sender)
        .execute(&p)
        .await
        .expect("message");

        let m = MessageId::from_uuid(msg);
        let s = ParticipantId::from_uuid(sender);
        assert_eq!(
            repo.toggle_authorized_outboxed(m, s, "🎉", None)
                .await
                .unwrap()
                .op,
            ReactionOp::Add
        );
        assert_eq!(
            repo.toggle_authorized_outboxed(m, s, "🚀", None)
                .await
                .unwrap()
                .op,
            ReactionOp::Add
        );
        assert!(matches!(
            repo.toggle_authorized_outboxed(m, s, "🔥", None).await,
            Err(aero_common::Error::Invalid(_))
        ));
        // Remove is always allowed and frees a slot.
        assert_eq!(
            repo.toggle_authorized_outboxed(m, s, "🎉", None)
                .await
                .unwrap()
                .op,
            ReactionOp::Remove
        );
        assert_eq!(
            repo.toggle_authorized_outboxed(m, s, "🔥", None)
                .await
                .unwrap()
                .op,
            ReactionOp::Add
        );
        sqlx::query("UPDATE rooms SET max_reactions_per_user = NULL WHERE id = $1")
            .bind(room)
            .execute(&p)
            .await
            .unwrap();
        assert_eq!(
            repo.toggle_authorized_outboxed(m, s, "💯", None)
                .await
                .unwrap()
                .op,
            ReactionOp::Add
        );

        sqlx::query("DELETE FROM event_outbox WHERE message_id = $1")
            .bind(msg)
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM messages WHERE id = $1")
            .bind(msg)
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(room)
            .execute(&p)
            .await
            .ok();
        sqlx::query(
            "DELETE FROM workspace_members
              WHERE workspace_id = '00000000-0000-0000-0000-000000000000'
                AND participant_id = $1",
        )
        .bind(sender)
        .execute(&p)
        .await
        .ok();
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(sender)
            .execute(&p)
            .await
            .ok();
    }
}

#[cfg(test)]
mod transaction_tests;
