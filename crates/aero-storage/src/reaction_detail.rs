//! "Who reacted" — the participants behind each emoji on a single message.
//!
//! The reaction picker's detail view: given a message, list every distinct emoji
//! together with the participants who reacted with it. This is a read-only
//! projection over the EXISTING `reactions` table (`message_id, participant_id,
//! emoji, created_at`, see `migrations/0002_p2_collab_ai.sql`) — it adds NO table,
//! NO id and NO migration, and never mutates: toggling a reaction stays in
//! [`ReactionRepo`](crate::ReactionRepo).
//!
//! Purely additive: a NEW [`ReactionDetailRepo`]; no existing repo is touched.
//! Unlike [`ReactionSummary`](aero_common::ReactionSummary) (per-message *counts*
//! over a batch), this returns the full reactor list for ONE message — the data a
//! client needs to render "Alice, Bob and 3 others reacted with 👍". The
//! [`EmojiReactors`] projection lives here (and is re-exported from the crate
//! root) rather than in `aero-common`, since it is a storage-layer view.

use aero_common::{MessageId, ParticipantId, RoomId};
use serde::Serialize;
use sqlx::PgPool;

/// One emoji on a message, together with the participants who reacted with it.
///
/// A storage-layer projection grouped from `reactions` rows. `Serialize` so a
/// handler can hand the list straight back as JSON.
#[derive(Debug, Clone, Serialize)]
pub struct EmojiReactors {
    /// The emoji that was reacted with.
    pub emoji: String,
    /// The participants who reacted with [`emoji`](Self::emoji), in the order they
    /// reacted (oldest first).
    pub participants: Vec<ParticipantId>,
}

/// Group ordered `(emoji, participant)` rows into one [`EmojiReactors`] per
/// distinct emoji, preserving first-seen emoji order (and, within each emoji, the
/// input order of its participants). Pure, so the grouping is unit-tested offline
/// (Postgres absent in CI). Callers feed rows already ordered by `emoji` then
/// `created_at`, so each emoji's rows are contiguous and its reactors come out
/// oldest-first.
fn group_reactors(rows: Vec<(String, ParticipantId)>) -> Vec<EmojiReactors> {
    let mut out: Vec<EmojiReactors> = Vec::new();
    for (emoji, participant) in rows {
        if let Some(group) = out.iter_mut().find(|g| g.emoji == emoji) {
            group.participants.push(participant);
        } else {
            out.push(EmojiReactors {
                emoji,
                participants: vec![participant],
            });
        }
    }
    out
}

/// Repository for the "who reacted" detail view over the `reactions` table.
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`ReactionDetailRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct ReactionDetailRepo {
    pool: PgPool,
}

impl ReactionDetailRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Resolve the room a message belongs to, or `None` if no such message exists.
    /// The HTTP layer calls this to find the message's room and assert room access
    /// before exposing who reacted.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn room_for(&self, message: MessageId) -> Result<Option<RoomId>, sqlx::Error> {
        let row = sqlx::query_as::<_, (uuid::Uuid,)>("SELECT room_id FROM messages WHERE id = $1")
            .bind(message.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|(r,)| RoomId::from_uuid(r)))
    }

    /// The reactors on `message`, grouped into one [`EmojiReactors`] per distinct
    /// emoji. Emojis come out in ascending order; within each, reactors are
    /// oldest-first. An un-reacted (or unknown) message yields an empty `Vec` — the
    /// caller resolves existence/authorization via [`room_for`](Self::room_for).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn reactors_for(
        &self,
        message: MessageId,
    ) -> Result<Vec<EmojiReactors>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (String, uuid::Uuid)>(
            r"SELECT emoji, participant_id
               FROM reactions
              WHERE message_id = $1
              ORDER BY emoji ASC, created_at ASC",
        )
        .bind(message.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        let rows = rows
            .into_iter()
            .map(|(emoji, pid)| (emoji, ParticipantId::from_uuid(pid)))
            .collect();
        Ok(group_reactors(rows))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_reactors_groups_per_emoji_preserving_order() {
        let a = ParticipantId::new();
        let b = ParticipantId::new();
        let c = ParticipantId::new();
        // Rows arrive ordered by emoji, then created_at: 👍 has a then b, 🎉 has c.
        let grouped = group_reactors(vec![
            ("👍".to_string(), a),
            ("👍".to_string(), b),
            ("🎉".to_string(), c),
        ]);
        assert_eq!(grouped.len(), 2, "one entry per distinct emoji");
        assert_eq!(grouped[0].emoji, "👍");
        assert_eq!(grouped[0].participants, vec![a, b], "reactors in input order");
        assert_eq!(grouped[1].emoji, "🎉");
        assert_eq!(grouped[1].participants, vec![c]);
    }

    #[test]
    fn group_reactors_empty_is_empty() {
        assert!(group_reactors(Vec::new()).is_empty());
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored reaction_detail
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a throwaway participant so the test is self-contained.
    async fn mk_participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("reaction-detail-user-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    /// Create a throwaway channel owned by `creator`, in the default workspace.
    async fn mk_room(p: &PgPool, creator: ParticipantId) -> RoomId {
        let id = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, workspace_id) \
             VALUES ($1, 'channel', $2, $3, '00000000-0000-0000-0000-000000000000')",
        )
        .bind(id.to_uuid())
        .bind(format!("reaction-detail-room-{id}"))
        .bind(creator.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        id
    }

    /// Create a throwaway message in `room` sent by `sender`.
    async fn mk_message(p: &PgPool, room: RoomId, sender: ParticipantId) -> MessageId {
        let id = MessageId::new();
        sqlx::query(
            "INSERT INTO messages (id, room_id, sender_id, blocks) VALUES ($1, $2, $3, $4::jsonb)",
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .bind(sender.to_uuid())
        .bind("[]")
        .execute(p)
        .await
        .expect("insert message");
        id
    }

    async fn react(p: &PgPool, message: MessageId, participant: ParticipantId, emoji: &str) {
        sqlx::query("INSERT INTO reactions (message_id, participant_id, emoji) VALUES ($1, $2, $3)")
            .bind(message.to_uuid())
            .bind(participant.to_uuid())
            .bind(emoji)
            .execute(p)
            .await
            .expect("insert reaction");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn reactors_for_groups_per_emoji() {
        let p = pool();
        let repo = ReactionDetailRepo::new(p.clone());
        let alice = mk_participant(&p).await;
        let bob = mk_participant(&p).await;
        let room = mk_room(&p, alice).await;
        let msg = mk_message(&p, room, alice).await;

        // room_for resolves the seeded room; unknown id is None.
        assert_eq!(repo.room_for(msg).await.unwrap(), Some(room));
        assert!(repo.room_for(MessageId::new()).await.unwrap().is_none());

        // No reactions yet → empty.
        assert!(repo.reactors_for(msg).await.unwrap().is_empty());

        // Two participants react with 👍, one with 🎉.
        react(&p, msg, alice, "👍").await;
        react(&p, msg, bob, "👍").await;
        react(&p, msg, alice, "🎉").await;

        let detail = repo.reactors_for(msg).await.unwrap();
        assert_eq!(detail.len(), 2, "one entry per distinct emoji");
        let thumbs = detail.iter().find(|e| e.emoji == "👍").expect("👍 present");
        assert_eq!(thumbs.participants.len(), 2);
        assert!(thumbs.participants.contains(&alice) && thumbs.participants.contains(&bob));
        let party = detail.iter().find(|e| e.emoji == "🎉").expect("🎉 present");
        assert_eq!(party.participants, vec![alice]);

        // Cleanup so reruns stay self-contained (FK cascades clear reactions).
        sqlx::query("DELETE FROM messages WHERE id = $1")
            .bind(msg.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(room.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = $1 OR id = $2")
            .bind(alice.to_uuid())
            .bind(bob.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
