//! Reaction repository.
//!
//! Reactions are (message, participant, emoji) triples. Toggling adds or removes
//! the row. Aggregates are computed on read into [`ReactionSummary`].

use std::collections::BTreeMap;

use aero_common::{MessageId, ParticipantId, ReactionOp, ReactionSummary};
use sqlx::PgPool;

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
        let rows = sqlx::query_as::<_, (uuid::Uuid, String, uuid::Uuid)>(
            r#"SELECT message_id, emoji, participant_id
               FROM reactions
               WHERE message_id = ANY($1)
               ORDER BY created_at ASC"#,
        )
        .bind(&uuids)
        .fetch_all(&self.pool)
        .await?;

        let mut out: BTreeMap<MessageId, Vec<ReactionSummary>> = BTreeMap::new();
        for (mid, emoji, pid) in rows {
            let m = MessageId::from_uuid(mid);
            let p = ParticipantId::from_uuid(pid);
            let entry = out.entry(m).or_default();
            if let Some(s) = entry.iter_mut().find(|s| s.emoji == emoji) {
                s.count += 1;
                s.participants.push(p);
            } else {
                entry.push(ReactionSummary { emoji, count: 1, participants: vec![p] });
            }
        }
        Ok(out)
    }
}
