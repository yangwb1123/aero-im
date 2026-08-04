use aero_common::{Error, ParticipantId, RoomId};

use super::NotificationPrefsRepo;
use crate::draft::lock_effective_room_access_in_tx;

impl NotificationPrefsRepo {
    /// Mute a room while holding the caller's current effective-access fence.
    ///
    /// # Errors
    /// Returns an opaque room access error or a database error.
    pub async fn mute_authorized(
        &self,
        participant: ParticipantId,
        room: RoomId,
    ) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        lock_effective_room_access_in_tx(&mut tx, participant, room).await?;
        sqlx::query(
            r"INSERT INTO channel_mutes (participant_id, room_id, created_at)
               VALUES ($1, $2, now())
               ON CONFLICT (participant_id, room_id) DO NOTHING",
        )
        .bind(participant.to_uuid())
        .bind(room.to_uuid())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Unmute a room while holding the caller's current effective-access fence.
    ///
    /// # Errors
    /// Returns an opaque room access error or a database error.
    pub async fn unmute_authorized(
        &self,
        participant: ParticipantId,
        room: RoomId,
    ) -> Result<bool, Error> {
        let mut tx = self.pool.begin().await?;
        lock_effective_room_access_in_tx(&mut tx, participant, room).await?;
        let result =
            sqlx::query("DELETE FROM channel_mutes WHERE participant_id = $1 AND room_id = $2")
                .bind(participant.to_uuid())
                .bind(room.to_uuid())
                .execute(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(result.rows_affected() > 0)
    }

    /// List only muted rooms the caller can currently access.
    ///
    /// # Errors
    /// Propagates database errors.
    pub async fn muted_rooms_accessible(
        &self,
        participant: ParticipantId,
    ) -> Result<Vec<RoomId>, Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT room_id
                FROM channel_mutes
               WHERE participant_id = $1
                 AND aero_effective_room_access(room_id, $1, NULL)
               ORDER BY created_at DESC, room_id ASC",
        )
        .bind(participant.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(room,)| RoomId::from_uuid(room))
            .collect())
    }

    /// Set a room notification level under the current effective-access fence.
    ///
    /// # Errors
    /// Returns an opaque room access error or a database error.
    pub async fn set_level_authorized(
        &self,
        participant: ParticipantId,
        room: RoomId,
        level: &str,
    ) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        lock_effective_room_access_in_tx(&mut tx, participant, room).await?;
        sqlx::query(
            r"INSERT INTO channel_notification_prefs
                  (participant_id, room_id, level, updated_at)
               VALUES ($1, $2, $3, now())
               ON CONFLICT (participant_id, room_id)
               DO UPDATE SET level = EXCLUDED.level, updated_at = now()",
        )
        .bind(participant.to_uuid())
        .bind(room.to_uuid())
        .bind(level)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Read the explicit level and mute bit under one current-access fence.
    ///
    /// # Errors
    /// Returns an opaque room access error or a database error.
    pub async fn room_preference_authorized(
        &self,
        participant: ParticipantId,
        room: RoomId,
    ) -> Result<(Option<String>, bool), Error> {
        let mut tx = self.pool.begin().await?;
        lock_effective_room_access_in_tx(&mut tx, participant, room).await?;
        let level = sqlx::query_scalar::<_, String>(
            r"SELECT level
                FROM channel_notification_prefs
               WHERE participant_id = $1 AND room_id = $2",
        )
        .bind(participant.to_uuid())
        .bind(room.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let muted: bool = sqlx::query_scalar(
            r"SELECT EXISTS(
                   SELECT 1
                     FROM channel_mutes
                    WHERE participant_id = $1 AND room_id = $2
               )",
        )
        .bind(participant.to_uuid())
        .bind(room.to_uuid())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok((level, muted))
    }
}
