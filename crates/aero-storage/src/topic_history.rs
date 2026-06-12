//! Channel topic change history repository.
//!
//! Backs `migrations/0100_channel_topic_history.sql`. Each time a channel's
//! topic is changed (via the PATCH `/api/rooms/:id/channel` endpoint), one
//! [`TopicHistoryEntry`] row is appended capturing the actor, the old value,
//! and the new value. The list is queryable for audit / compliance purposes.
//!
//! Purely additive: a NEW [`TopicHistoryRepo`]; no existing repo is touched.

use aero_common::{Error as AeroError, ParticipantId, RoomId};
use sqlx::PgPool;
use uuid::Uuid;

/// One recorded topic change on a channel room.
#[derive(sqlx::FromRow, serde::Serialize)]
pub struct TopicHistoryEntry {
    pub id: Uuid,
    pub room_id: Uuid,
    pub changed_by: Uuid,
    pub old_topic: Option<String>,
    pub new_topic: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub changed_at: time::OffsetDateTime,
}

/// Repository for channel topic change history.
#[derive(Clone)]
pub struct TopicHistoryRepo {
    pub pg: PgPool,
}

impl TopicHistoryRepo {
    pub fn new(pg: PgPool) -> Self {
        Self { pg }
    }

    /// Record a topic change for a room. `old_topic` and `new_topic` are the
    /// values before and after the change respectively; both may be `None` when
    /// the topic is unset.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the INSERT.
    pub async fn record(
        &self,
        changed_by: ParticipantId,
        room: RoomId,
        old_topic: Option<&str>,
        new_topic: Option<&str>,
    ) -> Result<(), AeroError> {
        sqlx::query(
            r"INSERT INTO channel_topic_history (room_id, changed_by, old_topic, new_topic, changed_at)
               VALUES ($1, $2, $3, $4, now())",
        )
        .bind(room.to_uuid())
        .bind(changed_by.to_uuid())
        .bind(old_topic)
        .bind(new_topic)
        .execute(&self.pg)
        .await
        .map_err(AeroError::from)?;
        Ok(())
    }

    /// List topic changes for `room`, newest first, with pagination.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the SELECT.
    pub async fn list(
        &self,
        room: RoomId,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TopicHistoryEntry>, AeroError> {
        let rows = sqlx::query_as::<_, TopicHistoryEntry>(
            r"SELECT id, room_id, changed_by, old_topic, new_topic, changed_at
               FROM channel_topic_history
               WHERE room_id = $1
               ORDER BY changed_at DESC
               LIMIT $2 OFFSET $3",
        )
        .bind(room.to_uuid())
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pg)
        .await
        .map_err(AeroError::from)?;
        Ok(rows)
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::{ParticipantId, RoomId};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a throwaway participant + workspace + room so FK constraints are met.
    async fn fixture(p: &PgPool) -> (ParticipantId, RoomId) {
        let participant = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(participant.to_uuid())
            .bind(format!("topic-hist-actor-{participant}"))
            .execute(p)
            .await
            .expect("insert participant");
        let room = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
             VALUES ($1,'channel',$2,$3,now(),'00000000-0000-0000-0000-000000000000')",
        )
        .bind(room.to_uuid())
        .bind(format!("topic-hist-room-{room}"))
        .bind(participant.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        (participant, room)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn topic_history_record_and_list_roundtrip() {
        let p = pool();
        let repo = TopicHistoryRepo::new(p.clone());
        let (actor, room) = fixture(&p).await;

        // No history initially.
        let initial = repo.list(room, 10, 0).await.unwrap();
        assert!(initial.is_empty());

        // Record: None -> "daily standup".
        repo.record(actor, room, None, Some("daily standup")).await.unwrap();

        // Record: "daily standup" -> "weekly sync".
        repo.record(actor, room, Some("daily standup"), Some("weekly sync")).await.unwrap();

        let entries = repo.list(room, 10, 0).await.unwrap();
        assert_eq!(entries.len(), 2, "two topic changes recorded");
        // Newest first.
        assert_eq!(entries[0].new_topic.as_deref(), Some("weekly sync"));
        assert_eq!(entries[0].old_topic.as_deref(), Some("daily standup"));
        assert_eq!(entries[1].new_topic.as_deref(), Some("daily standup"));
        assert_eq!(entries[1].old_topic, None);

        // Pagination: limit=1 offset=0 should return the newest only.
        let page1 = repo.list(room, 1, 0).await.unwrap();
        assert_eq!(page1.len(), 1);
        assert_eq!(page1[0].new_topic.as_deref(), Some("weekly sync"));

        // Pagination: limit=1 offset=1 should return the second newest.
        let page2 = repo.list(room, 1, 1).await.unwrap();
        assert_eq!(page2.len(), 1);
        assert_eq!(page2[0].new_topic.as_deref(), Some("daily standup"));
    }
}
