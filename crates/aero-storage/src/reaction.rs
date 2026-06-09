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

        let mut out: BTreeMap<MessageId, Vec<ReactionSummary>> = BTreeMap::new();
        for (mid, emoji, count, reactors) in rows {
            out.entry(MessageId::from_uuid(mid)).or_default().push(ReactionSummary {
                emoji,
                count: u32::try_from(count).unwrap_or(u32::MAX),
                participants: reactors.into_iter().map(ParticipantId::from_uuid).collect(),
            });
        }
        Ok(out)
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
}
