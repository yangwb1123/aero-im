//! Short-lived, single-use SAML AuthnRequest correlation state.

use aero_common::Error as AeroError;
use fred::{
    prelude::{KeysInterface, RedisClient},
    types::{Expiration, SetOptions},
};

pub(super) const MAX_REQUEST_ID_BYTES: usize = 64;
const REQUEST_TTL_SECS: i64 = 5 * 60;
const KEY_PREFIX: &str = "aero:saml:request:";

pub(super) fn validate_request_id(request_id: &str) -> Result<(), AeroError> {
    let bytes = request_id.as_bytes();
    let valid = (2..=MAX_REQUEST_ID_BYTES).contains(&bytes.len())
        && bytes[0] == b'_'
        && bytes[1..].iter().all(u8::is_ascii_alphanumeric);
    if valid {
        Ok(())
    } else {
        Err(AeroError::Unauthorized(
            "saml: invalid InResponseTo request id".into(),
        ))
    }
}

pub(super) async fn issue_request(client: &RedisClient, request_id: &str) -> Result<(), AeroError> {
    validate_request_id(request_id)?;
    let result: Option<String> = client
        .set(
            request_key(request_id),
            "pending",
            Some(Expiration::EX(REQUEST_TTL_SECS)),
            Some(SetOptions::NX),
            false,
        )
        .await
        .map_err(|error| {
            AeroError::Internal(anyhow::anyhow!("saml request-state write failed: {error}"))
        })?;
    if result.is_none() {
        return Err(AeroError::Internal(anyhow::anyhow!(
            "saml request id collision"
        )));
    }
    Ok(())
}

pub(super) async fn consume_request(
    client: &RedisClient,
    request_id: &str,
) -> Result<(), AeroError> {
    validate_request_id(request_id)?;
    let consumed: Option<String> =
        client
            .getdel(request_key(request_id))
            .await
            .map_err(|error| {
                AeroError::Internal(anyhow::anyhow!(
                    "saml request-state consume failed: {error}"
                ))
            })?;
    if consumed.as_deref() != Some("pending") {
        return Err(AeroError::Unauthorized(
            "saml: unknown, expired, or already-consumed InResponseTo".into(),
        ));
    }
    Ok(())
}

fn request_key(request_id: &str) -> String {
    format!("{KEY_PREFIX}{request_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use fred::prelude::ClientLike;

    #[test]
    fn request_ids_are_strictly_bounded_xml_ids() {
        assert!(validate_request_id("_01HZX4M8M8J5GSY7CX3F2TZC9Q").is_ok());
        let overlong = format!("_{}", "A".repeat(MAX_REQUEST_ID_BYTES));
        for invalid in [
            "",
            "_",
            "01HZX4",
            "_has-dash",
            "_has:colon",
            "_has space",
            &overlong,
        ] {
            assert!(validate_request_id(invalid).is_err(), "{invalid:?}");
        }
    }

    #[tokio::test]
    #[ignore = "requires live Redis"]
    async fn issued_request_is_consumed_exactly_once_under_race() {
        let url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
        let client = RedisClient::new(
            fred::types::RedisConfig::from_url(&url).expect("redis URL"),
            None,
            None,
            None,
        );
        client.connect();
        client.wait_for_connect().await.expect("connect Redis");
        let request_id = format!("_{}", ulid::Ulid::new());
        issue_request(&client, &request_id).await.expect("issue");
        let (left, right) = tokio::join!(
            consume_request(&client, &request_id),
            consume_request(&client, &request_id)
        );
        assert_ne!(left.is_ok(), right.is_ok(), "GETDEL has one winner");
        let _: i64 = client.del(request_key(&request_id)).await.unwrap();
    }
}
