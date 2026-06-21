//! Reaction repository.
//!
//! Reactions are (message, participant, emoji) triples. Toggling adds or removes
//! the row. Aggregates are computed on read into [`ReactionSummary`].

use std::collections::BTreeMap;

use aero_common::{MessageId, ParticipantId, ReactionOp, ReactionSummary};
use sqlx::PgPool;

/// Cap on the per-emoji reactor preview returned by [`ReactionRepo::summaries_for`].
///
/// `count` stays exact; this only bounds the `participants` avatar-preview list so
/// a wildly-reacted message can't materialise an unbounded Vec (memory + JSON).
/// The UI shows the first `MAX_REACTORS_PREVIEW` reactors and a "+N" overflow.
const MAX_REACTORS_PREVIEW: i64 = 50;

#[derive(Clone)]
pub struct ReactionRepo {
    pool: PgPool,
}

impl ReactionRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Toggle a reaction; returns the resulting [`ReactionOp`] that actually happened.
    pub async fn toggle(
        &self,
        message: MessageId,
        participant: ParticipantId,
        emoji: &str,
    ) -> Result<ReactionOp, sqlx::Error> {
        let removed = sqlx::query(
            r#"DELETE FROM reactions
               WHERE message_id = $1 AND participant_id = $2 AND emoji = $3"#,
        )
        .bind(message.to_uuid())
        .bind(participant.to_uuid())
        .bind(emoji)
        .execute(&self.pool)
        .await?;
        if removed.rows_affected() > 0 {
            return Ok(ReactionOp::Remove);
        }
        sqlx::query(
            r#"INSERT INTO reactions (message_id, participant_id, emoji)
               VALUES ($1, $2, $3) ON CONFLICT DO NOTHING"#,
        )
        .bind(message.to_uuid())
        .bind(participant.to_uuid())
        .bind(emoji)
        .execute(&self.pool)
        .await?;
        Ok(ReactionOp::Add)
    }

    /// Atomically toggle a reaction, enforcing an optional per-`(message,
    /// participant)` distinct-emoji cap. A `pg_advisory_xact_lock` keyed on
    /// `(message, participant)` serializes concurrent toggles by the SAME user on
    /// the SAME message, so the count-check and the insert are race-free — two
    /// concurrent new-emoji Adds can no longer both slip past the cap (the bare
    /// `count_by_sender`-then-`toggle` path could). Returns `Ok(None)` when a new
    /// Add is rejected by the cap; `Ok(Some(op))` for the toggle that happened.
    pub async fn toggle_capped(
        &self,
        message: MessageId,
        participant: ParticipantId,
        emoji: &str,
        max_distinct: Option<i64>,
    ) -> Result<Option<ReactionOp>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        // Transaction-scoped advisory lock (auto-released at commit/rollback). The
        // key hashes (message, participant) into a bigint; a hash collision only
        // adds harmless cross-key contention, never a correctness problem.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("{}:{}", message.to_uuid(), participant.to_uuid()))
            .execute(&mut *tx)
            .await?;

        let removed = sqlx::query(
            r#"DELETE FROM reactions
               WHERE message_id = $1 AND participant_id = $2 AND emoji = $3"#,
        )
        .bind(message.to_uuid())
        .bind(participant.to_uuid())
        .bind(emoji)
        .execute(&mut *tx)
        .await?;
        if removed.rows_affected() > 0 {
            tx.commit().await?;
            return Ok(Some(ReactionOp::Remove));
        }

        // It's a new Add — enforce the distinct-emoji cap INSIDE the lock.
        if let Some(cap) = max_distinct {
            let (count,): (i64,) = sqlx::query_as(
                "SELECT COUNT(DISTINCT emoji) FROM reactions \
                 WHERE message_id = $1 AND participant_id = $2",
            )
            .bind(message.to_uuid())
            .bind(participant.to_uuid())
            .fetch_one(&mut *tx)
            .await?;
            if count >= cap {
                tx.commit().await?;
                return Ok(None);
            }
        }

        sqlx::query(
            r#"INSERT INTO reactions (message_id, participant_id, emoji)
               VALUES ($1, $2, $3) ON CONFLICT DO NOTHING"#,
        )
        .bind(message.to_uuid())
        .bind(participant.to_uuid())
        .bind(emoji)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(ReactionOp::Add))
    }

    /// Count of distinct emoji this `sender` has already added to `message`.
    /// Used by the reaction-spam-limit gate in `ImService::toggle_reaction`.
    pub async fn count_by_sender(
        &self,
        message: MessageId,
        sender: ParticipantId,
    ) -> Result<i64, sqlx::Error> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(DISTINCT emoji) FROM reactions \
             WHERE message_id = $1 AND participant_id = $2",
        )
        .bind(message.to_uuid())
        .bind(sender.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// Whether `sender` has already reacted to `message` with `emoji`.
    /// Used alongside [`count_by_sender`](Self::count_by_sender) to distinguish
    /// a Remove (emoji already present) from an Add (new emoji) without calling
    /// `toggle` first.
    pub async fn has_reacted(
        &self,
        message: MessageId,
        sender: ParticipantId,
        emoji: &str,
    ) -> Result<bool, sqlx::Error> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM reactions \
             WHERE message_id = $1 AND participant_id = $2 AND emoji = $3",
        )
        .bind(message.to_uuid())
        .bind(sender.to_uuid())
        .bind(emoji)
        .fetch_one(&self.pool)
        .await?;
        Ok(count > 0)
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
            r#"SELECT message_id,
                      emoji,
                      COUNT(*) AS count,
                      (array_agg(participant_id ORDER BY created_at ASC))[1:$2] AS reactors
               FROM reactions
               WHERE message_id = ANY($1)
               GROUP BY message_id, emoji
               ORDER BY message_id, MIN(created_at) ASC"#,
        )
        .bind(&uuids)
        .bind(MAX_REACTORS_PREVIEW)
        .fetch_all(&self.pool)
        .await?;

        Ok(Self::group_summaries(rows))
    }

    /// Like [`summaries_for`](Self::summaries_for) but ONLY for messages in rooms
    /// `viewer` is a member of — the `JOIN room_members` is the security boundary
    /// (mirrors `MessageRepo::search_all_rooms`), so a batch request can never
    /// surface reaction counts or reactor ids for a room the caller isn't in.
    /// Inaccessible message ids are silently dropped from the result.
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
            r#"SELECT r.message_id,
                      r.emoji,
                      COUNT(*) AS count,
                      (array_agg(r.participant_id ORDER BY r.created_at ASC))[1:$2] AS reactors
               FROM reactions r
               JOIN messages m      ON m.id = r.message_id
               JOIN room_members rm ON rm.room_id = m.room_id AND rm.participant_id = $3
               WHERE r.message_id = ANY($1)
               GROUP BY r.message_id, r.emoji
               ORDER BY r.message_id, MIN(r.created_at) ASC"#,
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
            out.entry(MessageId::from_uuid(mid)).or_default().push(ReactionSummary {
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
        sqlx::query("DELETE FROM reactions WHERE message_id = $1").bind(msg).execute(&p).await.ok();
        sqlx::query("DELETE FROM messages WHERE id = $1").bind(msg).execute(&p).await.ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1").bind(room).execute(&p).await.ok();
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
        // Room A: viewer IS a member. Room B: viewer is NOT.
        let mk_room_msg = |room: uuid::Uuid, msg: uuid::Uuid| {
            let p = p.clone();
            async move {
                sqlx::query(
                    "INSERT INTO rooms (id, kind, name, created_by, workspace_id)
                     VALUES ($1,'group','r',$2,'00000000-0000-0000-0000-000000000000')",
                )
                .bind(room).bind(sender.to_uuid()).execute(&p).await.expect("room");
                sqlx::query(
                    "INSERT INTO messages (id, room_id, sender_id, blocks, metadata)
                     VALUES ($1,$2,$3,'[]'::jsonb,'{}'::jsonb)",
                )
                .bind(msg).bind(room).bind(sender.to_uuid()).execute(&p).await.expect("msg");
                sqlx::query("INSERT INTO reactions (message_id, participant_id, emoji) VALUES ($1,$2,'👍')")
                    .bind(msg).bind(sender.to_uuid()).execute(&p).await.expect("reaction");
            }
        };
        let (room_a, msg_a) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        let (room_b, msg_b) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        mk_room_msg(room_a, msg_a).await;
        mk_room_msg(room_b, msg_b).await;
        // Viewer joins ONLY room A.
        sqlx::query("INSERT INTO room_members (room_id, participant_id, role) VALUES ($1,$2,'member')")
            .bind(room_a).bind(viewer.to_uuid()).execute(&p).await.expect("join A");

        let (mid_a, mid_b) = (MessageId::from_uuid(msg_a), MessageId::from_uuid(msg_b));
        let out = repo.summaries_for_accessible(viewer, &[mid_a, mid_b]).await.expect("scoped");
        assert!(out.contains_key(&mid_a), "viewer sees room-A reactions");
        assert!(!out.contains_key(&mid_b), "room-B reactions are NOT leaked to a non-member");

        // The unscoped variant would have leaked both — proving the scoping matters.
        let unscoped = repo.summaries_for(&[mid_a, mid_b]).await.expect("unscoped");
        assert!(unscoped.contains_key(&mid_b), "unscoped sees both (the IDOR the scoped form closes)");

        for r in [room_a, room_b] {
            sqlx::query("DELETE FROM reactions WHERE message_id IN (SELECT id FROM messages WHERE room_id=$1)").bind(r).execute(&p).await.ok();
            sqlx::query("DELETE FROM messages WHERE room_id = $1").bind(r).execute(&p).await.ok();
            sqlx::query("DELETE FROM rooms WHERE id = $1").bind(r).execute(&p).await.ok();
        }
        sqlx::query("DELETE FROM participants WHERE id = $1 OR id = $2").bind(viewer.to_uuid()).bind(sender.to_uuid()).execute(&p).await.ok();
    }

    /// `toggle_capped` enforces the per-(message, participant) distinct-emoji cap
    /// atomically: Adds up to the cap succeed; the next NEW emoji is rejected
    /// (`None`); a Remove is always allowed and frees a slot. Guards the
    /// advisory-locked count-check that closes the count-then-insert race.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn toggle_capped_enforces_distinct_emoji_cap() {
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
        let cap = Some(2_i64);
        assert_eq!(repo.toggle_capped(m, s, "🎉", cap).await.unwrap(), Some(ReactionOp::Add));
        assert_eq!(repo.toggle_capped(m, s, "🚀", cap).await.unwrap(), Some(ReactionOp::Add));
        // Third distinct emoji exceeds the cap → rejected.
        assert_eq!(repo.toggle_capped(m, s, "🔥", cap).await.unwrap(), None);
        // Remove is always allowed and frees a slot.
        assert_eq!(repo.toggle_capped(m, s, "🎉", cap).await.unwrap(), Some(ReactionOp::Remove));
        assert_eq!(repo.toggle_capped(m, s, "🔥", cap).await.unwrap(), Some(ReactionOp::Add));
        // No cap = unlimited.
        assert_eq!(repo.toggle_capped(m, s, "💯", None).await.unwrap(), Some(ReactionOp::Add));

        sqlx::query("DELETE FROM participants WHERE id = $1").bind(sender).execute(&p).await.ok();
    }
}
