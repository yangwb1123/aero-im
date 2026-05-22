//! Presence tracking — who is online in which room.
//! Kept in Redis with TTL so crashed clients age out automatically.

use aero_common::{ParticipantId, RoomId};
use fred::prelude::{Expiration, KeysInterface, RedisClient};
use std::time::Duration;

#[derive(Clone)]
pub struct PresenceStore {
    client: RedisClient,
}

impl PresenceStore {
    pub fn new(client: RedisClient) -> Self {
        Self { client }
    }

    fn key(room: RoomId, participant: ParticipantId) -> String {
        format!("presence:room:{room}:p:{participant}")
    }

    pub async fn heartbeat(
        &self,
        room: RoomId,
        participant: ParticipantId,
        ttl: Duration,
    ) -> anyhow::Result<()> {
        let key = Self::key(room, participant);
        self.client
            .set::<(), _, _>(&key, "1", Some(Expiration::EX(ttl.as_secs() as i64)), None, false)
            .await?;
        Ok(())
    }

    pub async fn leave(&self, room: RoomId, participant: ParticipantId) -> anyhow::Result<()> {
        let key = Self::key(room, participant);
        let _: () = self.client.del(&key).await?;
        Ok(())
    }
}
