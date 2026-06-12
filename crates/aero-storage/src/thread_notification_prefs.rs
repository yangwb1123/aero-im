//! Per-thread notification level repository.
//!
//! Backs `migrations/0094_thread_notification_prefs.sql`. A participant chooses a
//! per-thread notification level for any thread (identified by the root message):
//!
//! * `all`      — notified on every reply (the default when no row exists).
//! * `mentions` — notified only when the message mentions them.
//! * `none`     — no notifications from this thread.
//!
//! Used by `ImService::dispatch_notifications` after the thread-mute step to apply
//! finer-grained per-thread suppression. Purely additive: a NEW
//! [`ThreadNotificationPrefsRepo`]; no existing repo is touched.

use std::collections::HashMap;

use aero_common::{MessageId, ParticipantId};
use sqlx::PgPool;

#[derive(Clone)]
pub struct ThreadNotificationPrefsRepo {
    pub pg: PgPool,
}

impl ThreadNotificationPrefsRepo {
    pub fn new(pg: PgPool) -> Self {
        Self { pg }
    }

    /// Set (or overwrite) the notification level for `participant` on the thread
    /// rooted at `root`. An idempotent upsert; a subsequent call with a different
    /// level updates the row.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the upsert.
    pub async fn set_level(
        &self,
        participant: ParticipantId,
        root: MessageId,
        level: &str,
    ) -> Result<(), aero_common::Error> {
        sqlx::query(
            r"INSERT INTO thread_notification_prefs (participant_id, root_message_id, level, created_at)
               VALUES ($1, $2, $3, now())
               ON CONFLICT (participant_id, root_message_id)
               DO UPDATE SET level = EXCLUDED.level",
        )
        .bind(participant.to_uuid())
        .bind(root.to_uuid())
        .bind(level)
        .execute(&self.pg)
        .await
        .map_err(aero_common::Error::from)?;
        Ok(())
    }

    /// The notification level for `participant` on `root`. Returns `"all"` when no
    /// row exists (the fail-open default so new threads always notify).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get_level(
        &self,
        participant: ParticipantId,
        root: MessageId,
    ) -> Result<String, aero_common::Error> {
        let row = sqlx::query_as::<_, (String,)>(
            r"SELECT level FROM thread_notification_prefs
               WHERE participant_id = $1 AND root_message_id = $2",
        )
        .bind(participant.to_uuid())
        .bind(root.to_uuid())
        .fetch_optional(&self.pg)
        .await
        .map_err(aero_common::Error::from)?;
        Ok(row.map(|(l,)| l).unwrap_or_else(|| "all".to_owned()))
    }

    /// Batch-fetch the notification level for `participant` across many thread roots
    /// in ONE round-trip. Missing entries (no row) default to `"all"`. Empty input
    /// short-circuits to an empty map without touching the database.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn level_map(
        &self,
        participant: ParticipantId,
        roots: &[MessageId],
    ) -> Result<HashMap<MessageId, String>, aero_common::Error> {
        if roots.is_empty() {
            return Ok(HashMap::new());
        }
        let ids: Vec<uuid::Uuid> = roots.iter().map(MessageId::to_uuid).collect();
        let rows = sqlx::query_as::<_, (uuid::Uuid, String)>(
            r"SELECT root_message_id, level FROM thread_notification_prefs
               WHERE participant_id = $1 AND root_message_id = ANY($2)",
        )
        .bind(participant.to_uuid())
        .bind(&ids)
        .fetch_all(&self.pg)
        .await
        .map_err(aero_common::Error::from)?;
        Ok(rows
            .into_iter()
            .map(|(m, l)| (MessageId::from_uuid(m), l))
            .collect())
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::{MessageId, ParticipantId};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn thread_notif_prefs_set_get_roundtrip() {
        let p = pool();
        let repo = ThreadNotificationPrefsRepo::new(p.clone());
        let participant = ParticipantId::new();
        let root = MessageId::new();

        // No row → default "all".
        let level = repo.get_level(participant, root).await.unwrap();
        assert_eq!(level, "all");

        repo.set_level(participant, root, "mentions").await.unwrap();
        let level = repo.get_level(participant, root).await.unwrap();
        assert_eq!(level, "mentions");

        // Overwrite via upsert.
        repo.set_level(participant, root, "none").await.unwrap();
        let level = repo.get_level(participant, root).await.unwrap();
        assert_eq!(level, "none");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn thread_notif_prefs_level_map_batch() {
        let p = pool();
        let repo = ThreadNotificationPrefsRepo::new(p.clone());
        let participant = ParticipantId::new();
        let root_a = MessageId::new();
        let root_b = MessageId::new();
        let root_c = MessageId::new(); // no row

        repo.set_level(participant, root_a, "all").await.unwrap();
        repo.set_level(participant, root_b, "none").await.unwrap();

        let map = repo.level_map(participant, &[root_a, root_b, root_c]).await.unwrap();
        assert_eq!(map.get(&root_a).map(String::as_str), Some("all"));
        assert_eq!(map.get(&root_b).map(String::as_str), Some("none"));
        assert!(map.get(&root_c).is_none(), "missing row absent from map");

        // Empty input short-circuits.
        let empty = repo.level_map(participant, &[]).await.unwrap();
        assert!(empty.is_empty());
    }
}
