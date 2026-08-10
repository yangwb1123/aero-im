//! MLS (RFC 9420) protocol wire types — opaque storage envelopes.
//!
//! These types **don't** implement the MLS state machine; they describe what
//! the server stores and relays without inspecting. Drop in `openmls` for the
//! actual ratchet + handshake when E2E flips on per-room.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{ParticipantId, RoomId};

/// Opaque MLS group identifier (a few bytes per RFC 9420).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MlsGroupId(#[serde(with = "serde_bytes")] pub Vec<u8>);

impl MlsGroupId {
    #[must_use]
    pub fn new<I: Into<Vec<u8>>>(bytes: I) -> Self {
        Self(bytes.into())
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// A `KeyPackage` as published by a participant; consumed by anyone adding them
/// to a group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyPackage {
    pub id: uuid::Uuid,
    pub participant_id: ParticipantId,
    pub ciphersuite: String,
    #[serde(with = "serde_bytes")]
    pub payload: Vec<u8>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub consumed_at: Option<OffsetDateTime>,
}

/// Persisted server-side state for an MLS group (the bytes openmls owns).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MlsGroupState {
    pub group_id: MlsGroupId,
    pub room_id: Option<RoomId>,
    pub ciphersuite: String,
    pub epoch: u64,
    #[serde(with = "serde_bytes")]
    pub state: Vec<u8>,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

/// Wire envelope for an MLS-encrypted message — replaces `Message.blocks` when
/// the room is E2E. Server can't decrypt; just routes the bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MlsCiphertext {
    pub group_id: MlsGroupId,
    pub epoch: u64,
    #[serde(with = "serde_bytes")]
    pub payload: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_package_roundtrips_through_json() {
        let kp = KeyPackage {
            id: uuid::Uuid::new_v4(),
            participant_id: ParticipantId::new(),
            ciphersuite: "MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519".into(),
            payload: vec![1, 2, 3, 4],
            created_at: OffsetDateTime::now_utc(),
            consumed_at: None,
        };
        let j = serde_json::to_string(&kp).unwrap();
        let back: KeyPackage = serde_json::from_str(&j).unwrap();
        assert_eq!(back.payload, kp.payload);
        assert_eq!(back.ciphersuite, kp.ciphersuite);
    }

    #[test]
    fn group_id_eq_by_bytes() {
        assert_eq!(
            MlsGroupId::new(b"abc".to_vec()),
            MlsGroupId::new(vec![97, 98, 99])
        );
        assert_ne!(
            MlsGroupId::new(b"abc".to_vec()),
            MlsGroupId::new(b"abd".to_vec())
        );
    }
}
