use super::{Block, MessageId, RoomId};
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Serialize)]
struct MessageRequestFingerprint<'a> {
    version: u8,
    room_id: RoomId,
    blocks: &'a [Block],
    reply_to: Option<MessageId>,
    expires_after_secs: Option<u64>,
}

pub(super) fn message_request_hash(
    room_id: RoomId,
    blocks: &[Block],
    reply_to: Option<MessageId>,
    expires_after_secs: Option<u64>,
) -> aero_common::Result<[u8; 32]> {
    let canonical = serde_json::to_vec(&MessageRequestFingerprint {
        version: 1,
        room_id,
        blocks,
        reply_to,
        expires_after_secs,
    })?;
    Ok(Sha256::digest(canonical).into())
}

pub(super) fn message_expiration(
    expires_after_secs: Option<u64>,
    now: time::OffsetDateTime,
) -> aero_common::Result<Option<time::OffsetDateTime>> {
    expires_after_secs
        .filter(|&seconds| seconds > 0)
        .map(|seconds| {
            i64::try_from(seconds)
                .map(|seconds| now + time::Duration::seconds(seconds))
                .map_err(|_| {
                    aero_common::Error::Invalid(
                        "expires_after_secs exceeds the supported range".into(),
                    )
                })
        })
        .transpose()
}

pub(super) fn send_error_retryable(error: &aero_common::Error) -> bool {
    match error {
        aero_common::Error::RateLimited
        | aero_common::Error::Upstream(_)
        | aero_common::Error::Internal(_) => true,
        aero_common::Error::Database(error) => database_error_retryable(error),
        _ => false,
    }
}

fn database_error_retryable(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::Database(error) => {
            error.code().as_deref().is_some_and(postgres_code_retryable)
        }
        sqlx::Error::Io(_)
        | sqlx::Error::Tls(_)
        | sqlx::Error::PoolTimedOut
        | sqlx::Error::PoolClosed
        | sqlx::Error::WorkerCrashed
        | sqlx::Error::BeginFailed => true,
        _ => false,
    }
}

pub(super) fn postgres_code_retryable(code: &str) -> bool {
    code.starts_with("08")
        || matches!(
            code,
            "40001" // serialization_failure
                | "40P01" // deadlock_detected
                | "55P03" // lock_not_available
                | "53300" // too_many_connections
                | "53400" // configuration_limit_exceeded
                | "57014" // query_canceled / timeout
                | "57P01" // admin_shutdown
                | "57P02" // crash_shutdown
                | "57P03" // cannot_connect_now
        )
}
