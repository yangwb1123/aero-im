//! Shared edge policy for immediate message sends.

use aero_common::{Error, ParticipantId, Result, RoomId};
use fred::{
    error::RedisError,
    interfaces::LuaInterface,
    prelude::{KeysInterface, RedisClient},
    types::{Expiration, SetOptions},
};

use crate::state::AppState;

/// A token-owned slow-mode reservation.
///
/// Callers must consume it through [`Self::finish`] after the attempted write.
/// Definitive pre-commit failures release the exact token with a compare-and-
/// delete Lua script; successful or ambiguous database/internal results retain
/// the key until its TTL expires.
pub(crate) struct SlowmodeReservation {
    client: Option<RedisClient>,
    key: String,
    token: String,
    room: RoomId,
    participant: ParticipantId,
}

impl SlowmodeReservation {
    fn inert(room: RoomId, participant: ParticipantId) -> Self {
        Self {
            client: None,
            key: String::new(),
            token: String::new(),
            room,
            participant,
        }
    }

    /// Resolve an attempted write and release only when the error proves no
    /// durable message could have committed.
    pub(crate) async fn finish<T>(mut self, result: Result<T>) -> Result<T> {
        if result
            .as_ref()
            .err()
            .is_some_and(definitive_precommit_failure)
        {
            self.release_inner().await;
        }
        result
    }

    /// Explicitly abandon a request that failed before attempting its durable
    /// message write (for example a provider request that returned an error).
    pub(crate) async fn release(mut self) {
        self.release_inner().await;
    }

    async fn release_inner(&mut self) {
        let Some(client) = self.client.take() else {
            return;
        };
        if let Err(error) = release_slowmode(&client, &self.key, &self.token).await {
            // Conservative failure mode: leave the TTL-bound reservation in
            // place rather than risking deletion of another request's token.
            tracing::warn!(
                ?error,
                room = %self.room,
                participant = %self.participant,
                "slowmode reservation release failed"
            );
        }
    }
}

fn definitive_precommit_failure(error: &Error) -> bool {
    !matches!(error, Error::Database(_) | Error::Internal(_))
}

/// Reserve the room's current slow-mode interval before an immediate message
/// write.
///
/// Provider-backed commands call this before spending external quota; the WS
/// send/edit and REST paths share it so those entry points cannot drift.
///
/// After checking the durable last message, a Redis `SET NX EX` reservation
/// closes the cross-node race where two concurrent requests both observe the
/// same old row and publish together. The stored random token makes a later
/// release safe even if the original key expired and another request acquired
/// the same sender/room slot. Redis failures remain fail-open, matching the rest
/// of the cluster rate/presence policy.
pub(crate) async fn reserve_slowmode(
    state: &AppState,
    participant: ParticipantId,
    room: RoomId,
) -> Result<SlowmodeReservation> {
    let slowmode = state.rooms.get_slowmode(room).await.unwrap_or(0);
    if slowmode == 0 {
        return Ok(SlowmodeReservation::inert(room, participant));
    }
    let last_message: Option<(time::OffsetDateTime,)> = sqlx::query_as(
        "SELECT created_at FROM messages
          WHERE room_id = $1 AND sender_id = $2 AND deleted_at IS NULL
          ORDER BY created_at DESC
          LIMIT 1",
    )
    .bind(room.to_uuid())
    .bind(participant.to_uuid())
    .fetch_optional(&state.pg)
    .await
    .unwrap_or(None);
    if let Some((last_at,)) = last_message {
        let elapsed = (time::OffsetDateTime::now_utc() - last_at).whole_seconds();
        if elapsed < i64::from(slowmode) {
            return Err(Error::Invalid(format!(
                "slowmode: wait {}s before sending again",
                i64::from(slowmode) - elapsed
            )));
        }
    }

    let key = slowmode_key(room, participant);
    let token = uuid::Uuid::new_v4().to_string();
    match reserve_slowmode_token(&state.redis_client, &key, &token, i64::from(slowmode)).await {
        Ok(true) => Ok(SlowmodeReservation {
            client: Some(state.redis_client.clone()),
            key,
            token,
            room,
            participant,
        }),
        Ok(false) => {
            let wait = state
                .redis_client
                .ttl::<i64, _>(&key)
                .await
                .unwrap_or(i64::from(slowmode))
                .max(1);
            Err(Error::Invalid(format!(
                "slowmode: wait {wait}s before sending again"
            )))
        }
        Err(error) => {
            tracing::warn!(?error, %room, %participant, "slowmode reservation failed open");
            Ok(SlowmodeReservation::inert(room, participant))
        }
    }
}

fn slowmode_key(room: RoomId, participant: ParticipantId) -> String {
    format!("aero:slowmode:{room}:{participant}")
}

async fn reserve_slowmode_token(
    client: &RedisClient,
    key: &str,
    token: &str,
    seconds: i64,
) -> std::result::Result<bool, RedisError> {
    let result: Option<String> = client
        .set(
            key,
            token,
            Some(Expiration::EX(seconds.max(1))),
            Some(SetOptions::NX),
            false,
        )
        .await?;
    Ok(result.is_some())
}

const RELEASE_SLOWMODE_SCRIPT: &str = "if redis.call('GET', KEYS[1]) == ARGV[1] then \
         return redis.call('DEL', KEYS[1]) \
     else \
         return 0 \
     end";

async fn release_slowmode(
    client: &RedisClient,
    key: &str,
    token: &str,
) -> std::result::Result<bool, RedisError> {
    let deleted: i64 = client
        .eval(RELEASE_SLOWMODE_SCRIPT, vec![key], vec![token])
        .await?;
    Ok(deleted == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fred::prelude::ClientLike;

    #[test]
    fn slowmode_reservation_key_is_sender_and_room_scoped() {
        let room = RoomId::new();
        let other_room = RoomId::new();
        let participant = ParticipantId::new();
        let other_participant = ParticipantId::new();
        assert_ne!(
            slowmode_key(room, participant),
            slowmode_key(other_room, participant)
        );
        assert_ne!(
            slowmode_key(room, participant),
            slowmode_key(room, other_participant)
        );
    }

    #[tokio::test]
    #[ignore = "requires live Redis"]
    async fn concurrent_slowmode_reservations_have_one_cluster_winner() {
        let url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
        let client = RedisClient::new(
            fred::types::RedisConfig::from_url(&url).unwrap(),
            None,
            None,
            None,
        );
        client.connect();
        client.wait_for_connect().await.unwrap();
        let key = slowmode_key(RoomId::new(), ParticipantId::new());
        let left = reserve_slowmode_token(&client, &key, "left", 30);
        let right = reserve_slowmode_token(&client, &key, "right", 30);
        let (left, right) = tokio::join!(left, right);
        assert_ne!(left.unwrap(), right.unwrap());
        let _: i64 = client.del(&key).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires live Redis"]
    async fn release_is_token_scoped_and_allows_immediate_retry() {
        let url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
        let client = RedisClient::new(
            fred::types::RedisConfig::from_url(&url).unwrap(),
            None,
            None,
            None,
        );
        client.connect();
        client.wait_for_connect().await.unwrap();
        let key = slowmode_key(RoomId::new(), ParticipantId::new());
        assert!(reserve_slowmode_token(&client, &key, "original", 30)
            .await
            .unwrap());
        assert!(
            !release_slowmode(&client, &key, "stale-token")
                .await
                .unwrap(),
            "a stale request must not delete the current reservation"
        );
        assert!(!reserve_slowmode_token(&client, &key, "blocked", 30)
            .await
            .unwrap());
        assert!(release_slowmode(&client, &key, "original").await.unwrap());
        assert!(reserve_slowmode_token(&client, &key, "retry", 30)
            .await
            .unwrap());
        assert!(client.ttl::<i64, _>(&key).await.unwrap() > 0);
        let _: i64 = client.del(&key).await.unwrap();
    }

    #[test]
    fn only_ambiguous_write_errors_retain_the_reservation() {
        assert!(definitive_precommit_failure(&Error::Invalid(
            "blocked".into()
        )));
        assert!(definitive_precommit_failure(&Error::RateLimited));
        assert!(!definitive_precommit_failure(&Error::Database(
            sqlx::Error::RowNotFound
        )));
        assert!(!definitive_precommit_failure(&Error::Internal(
            anyhow::anyhow!("unknown commit result")
        )));
    }
}
