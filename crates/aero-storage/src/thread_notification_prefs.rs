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

use aero_common::{Error, MessageId, ParticipantId};
use sqlx::PgPool;

use crate::thread_subscription::lock_effective_live_thread_root_in_tx;

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
    #[cfg(test)]
    pub(crate) async fn set_level(
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
    #[cfg(test)]
    pub(crate) async fn get_level(
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
        Ok(row.map_or_else(|| "all".to_owned(), |(level,)| level))
    }

    /// Batch-fetch the notification level for `participant` across many thread roots
    /// in ONE round-trip. Missing entries (no row) default to `"all"`. Empty input
    /// short-circuits to an empty map without touching the database.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    #[cfg(test)]
    pub(crate) async fn level_map(
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

    /// Batch-fetch the notification level for many `participants` on ONE thread
    /// `root` in a single round-trip — the "many people, one root" axis
    /// notification dispatch needs (the inverse of [`Self::level_map`], which
    /// batches one person across many roots). Missing entries default to `"all"`
    /// (fail-open). Empty input short-circuits without touching the database.
    /// Index-backed by `(root_message_id, participant_id)` (migration 0124),
    /// since the table PK leads with `participant_id`. ROADMAP 方向二.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn levels_for(
        &self,
        root: MessageId,
        participants: &[ParticipantId],
    ) -> Result<HashMap<ParticipantId, String>, aero_common::Error> {
        if participants.is_empty() {
            return Ok(HashMap::new());
        }
        let ids: Vec<uuid::Uuid> = participants.iter().map(ParticipantId::to_uuid).collect();
        let rows = sqlx::query_as::<_, (uuid::Uuid, String)>(
            r"SELECT preference.participant_id, preference.level
                FROM thread_notification_prefs AS preference
                JOIN messages AS root
                  ON root.id = preference.root_message_id
                 AND root.reply_to IS NULL
                 AND root.deleted_at IS NULL
               WHERE preference.root_message_id = $1
                 AND preference.participant_id = ANY($2)
                 AND aero_effective_room_access(
                         root.room_id,
                         preference.participant_id,
                         NULL
                     )",
        )
        .bind(root.to_uuid())
        .bind(&ids)
        .fetch_all(&self.pg)
        .await
        .map_err(aero_common::Error::from)?;
        Ok(rows
            .into_iter()
            .map(|(p, l)| (ParticipantId::from_uuid(p), l))
            .collect())
    }

    /// Set a notification level under a live-root/current-access fence.
    ///
    /// # Errors
    /// Returns an opaque root/access error or a database error.
    pub async fn set_level_authorized(
        &self,
        participant: ParticipantId,
        root: MessageId,
        level: &str,
    ) -> Result<(), Error> {
        let mut tx = self.pg.begin().await?;
        lock_effective_live_thread_root_in_tx(&mut tx, participant, root).await?;
        sqlx::query(
            r"INSERT INTO thread_notification_prefs
                  (participant_id, root_message_id, level, created_at)
               VALUES ($1, $2, $3, now())
               ON CONFLICT (participant_id, root_message_id)
               DO UPDATE SET level = EXCLUDED.level",
        )
        .bind(participant.to_uuid())
        .bind(root.to_uuid())
        .bind(level)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Read a notification level under a live-root/current-access fence.
    ///
    /// # Errors
    /// Returns an opaque root/access error or a database error.
    pub async fn get_level_authorized(
        &self,
        participant: ParticipantId,
        root: MessageId,
    ) -> Result<String, Error> {
        let mut tx = self.pg.begin().await?;
        lock_effective_live_thread_root_in_tx(&mut tx, participant, root).await?;
        let level = sqlx::query_scalar::<_, String>(
            r"SELECT level
                FROM thread_notification_prefs
               WHERE participant_id = $1 AND root_message_id = $2",
        )
        .bind(participant.to_uuid())
        .bind(root.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or_else(|| "all".to_owned());
        tx.commit().await?;
        Ok(level)
    }

    /// Batch-read only live roots the caller can currently access. Missing
    /// entries remain absent so callers can apply the `"all"` default.
    ///
    /// # Errors
    /// Propagates database errors.
    pub async fn level_map_accessible(
        &self,
        participant: ParticipantId,
        roots: &[MessageId],
    ) -> Result<HashMap<MessageId, String>, Error> {
        if roots.is_empty() {
            return Ok(HashMap::new());
        }
        let ids: Vec<uuid::Uuid> = roots.iter().map(MessageId::to_uuid).collect();
        let rows = sqlx::query_as::<_, (uuid::Uuid, String)>(
            r"SELECT preference.root_message_id, preference.level
                FROM thread_notification_prefs AS preference
                JOIN messages AS root
                  ON root.id = preference.root_message_id
                 AND root.reply_to IS NULL
                 AND root.deleted_at IS NULL
               WHERE preference.participant_id = $1
                 AND preference.root_message_id = ANY($2)
                 AND aero_effective_room_access(root.room_id, $1, NULL)",
        )
        .bind(participant.to_uuid())
        .bind(&ids)
        .fetch_all(&self.pg)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(message, level)| (MessageId::from_uuid(message), level))
            .collect())
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::{MessageId, ParticipantId};

    struct Fixture {
        owner: ParticipantId,
        member: ParticipantId,
        roots: Vec<MessageId>,
    }

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    async fn fixture(p: &PgPool, root_count: usize) -> Fixture {
        let owner = ParticipantId::new();
        let member = ParticipantId::new();
        let workspace = uuid::Uuid::new_v4();
        let room = uuid::Uuid::new_v4();
        let roots: Vec<MessageId> = (0..root_count).map(|_| MessageId::new()).collect();
        let mut tx = p.begin().await.expect("begin thread notification fixture");

        for participant in [owner, member] {
            sqlx::query(
                "INSERT INTO participants (id, kind, display_name)
                 VALUES ($1, 'human', $2)",
            )
            .bind(participant.to_uuid())
            .bind(format!("thread-notification-{participant}"))
            .execute(&mut *tx)
            .await
            .expect("insert participant");
        }
        sqlx::query(
            "INSERT INTO workspaces (id, name, slug, created_by)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(workspace)
        .bind(format!("Thread notification {workspace}"))
        .bind(format!("thread-notification-{workspace}"))
        .bind(owner.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("insert workspace");
        for (participant, role) in [(owner, "owner"), (member, "member")] {
            sqlx::query(
                "INSERT INTO workspace_members (workspace_id, participant_id, role)
                 VALUES ($1, $2, $3)",
            )
            .bind(workspace)
            .bind(participant.to_uuid())
            .bind(role)
            .execute(&mut *tx)
            .await
            .expect("insert workspace member");
        }
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, workspace_id)
             VALUES ($1, 'group', $2, $3, $4)",
        )
        .bind(room)
        .bind(format!("Thread notification room {room}"))
        .bind(owner.to_uuid())
        .bind(workspace)
        .execute(&mut *tx)
        .await
        .expect("insert room");
        for (participant, role) in [(owner, "owner"), (member, "member")] {
            sqlx::query(
                "INSERT INTO room_members (room_id, participant_id, role)
                 VALUES ($1, $2, $3)",
            )
            .bind(room)
            .bind(participant.to_uuid())
            .bind(role)
            .execute(&mut *tx)
            .await
            .expect("insert room member");
        }
        for (index, root) in roots.iter().enumerate() {
            sqlx::query(
                "INSERT INTO messages (id, room_id, sender_id, blocks, searchable_text)
                 VALUES ($1, $2, $3, '[]'::jsonb, $4)",
            )
            .bind(root.to_uuid())
            .bind(room)
            .bind(owner.to_uuid())
            .bind(format!("thread notification root {index}"))
            .execute(&mut *tx)
            .await
            .expect("insert live root message");
        }
        tx.commit()
            .await
            .expect("commit thread notification fixture");

        Fixture {
            owner,
            member,
            roots,
        }
    }

    #[tokio::test]
    async fn levels_for_empty_input_short_circuits() {
        // Empty participant list returns empty WITHOUT querying (lazy pool stays
        // unconnected), so this runs offline.
        let repo = ThreadNotificationPrefsRepo::new(pool());
        let got = repo
            .levels_for(MessageId::new(), &[])
            .await
            .expect("empty input must not query");
        assert!(got.is_empty());
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn levels_for_returns_only_set_levels() {
        let p = pool();
        let repo = ThreadNotificationPrefsRepo::new(p.clone());
        let Fixture {
            owner: muted,
            member: defaulted,
            roots,
        } = fixture(&p, 1).await;
        let root = roots[0];
        repo.set_level(muted, root, "none").await.unwrap();

        let map = repo.levels_for(root, &[muted, defaulted]).await.unwrap();
        assert_eq!(map.get(&muted).map(String::as_str), Some("none"));
        assert!(
            !map.contains_key(&defaulted),
            "no row → absent (caller defaults to all)"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn thread_notif_prefs_set_get_roundtrip() {
        let p = pool();
        let repo = ThreadNotificationPrefsRepo::new(p.clone());
        let Fixture {
            owner: participant,
            roots,
            ..
        } = fixture(&p, 1).await;
        let root = roots[0];

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
        let Fixture {
            owner: participant,
            roots,
            ..
        } = fixture(&p, 3).await;
        let [root_a, root_b, root_c] = roots.as_slice() else {
            panic!("fixture creates three roots");
        };

        repo.set_level(participant, *root_a, "all").await.unwrap();
        repo.set_level(participant, *root_b, "none").await.unwrap();

        let map = repo
            .level_map(participant, &[*root_a, *root_b, *root_c])
            .await
            .unwrap();
        assert_eq!(map.get(root_a).map(String::as_str), Some("all"));
        assert_eq!(map.get(root_b).map(String::as_str), Some("none"));
        assert!(!map.contains_key(root_c), "missing row absent from map");

        // Empty input short-circuits.
        let empty = repo.level_map(participant, &[]).await.unwrap();
        assert!(empty.is_empty());
    }
}
