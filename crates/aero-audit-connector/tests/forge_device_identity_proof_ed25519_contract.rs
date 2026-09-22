//! Signed device identity proof parity. This receiver verifies the
//! domain-separated Ed25519 binding and keeps every authority marker inert.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fmt;

const FIXTURE: &[u8] = include_bytes!("testdata/forge-device-identity-proof-ed25519-v1.json");
const SCHEMA: &str = "forge.device-identity-proof/ed25519/v1";
const MODE: &str = "pure_cryptographic_binding_only";
const NOTICE: &str = "This fixture checks Ed25519 proof encoding, public-key digest, domain-separated binding bytes, and signature validity only. It consumes no challenge, issues no credential, persists no enrollment, records no approval, accepts no heartbeat, grants no inventory authority, and grants no execution authority.";

#[derive(Debug, Deserialize, serde::Serialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Owner {
    issuer: String,
    subject: String,
    tenant_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Device {
    device_id: String,
    owner: Owner,
    key_id: String,
    public_key_sha256: String,
    approval_state: String,
    credential_state: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Challenge {
    challenge_id: String,
    challenge_sha256: String,
    issued_at_ms: u64,
    expires_at_ms: u64,
    consumed: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    device_id: String,
    key_id: String,
    public_key_base64url: String,
    owner: Owner,
    challenge_id: String,
    challenge_sha256: String,
    issued_at_ms: u64,
    expires_at_ms: u64,
    signature_base64url: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Expected {
    accepted: bool,
    reason: String,
    identity_bound: bool,
    approval_required: bool,
}

#[derive(Debug, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Authority {
    identity_verified: bool,
    challenge_consumed: bool,
    enrollment_persisted: bool,
    owner_approval_recorded: bool,
    credential_issued: bool,
    inventory_authoritative: bool,
    execution_authorized: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: String,
    evaluation_mode: String,
    notice: String,
    owner_declaration: Owner,
    device: Device,
    challenge: Challenge,
    proof: Proof,
    expected: Expected,
    authority: Authority,
}

#[derive(serde::Serialize)]
struct Payload<'a> {
    domain: &'static str,
    device_id: &'a str,
    key_id: &'a str,
    public_key_sha256: &'a str,
    owner: &'a Owner,
    challenge_id: &'a str,
    challenge_sha256: &'a str,
    issued_at_ms: u64,
    expires_at_ms: u64,
}

fn decode<T: DeserializeOwned>(raw: &[u8]) -> Result<T, String> {
    reject_duplicate_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value = T::deserialize(&mut decoder).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    Ok(value)
}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_owner(owner: &Owner) -> bool {
    valid_token(&owner.issuer) && valid_token(&owner.subject) && valid_token(&owner.tenant_id)
}

fn display_only(fixture: &Fixture) -> bool {
    if fixture.schema_version != SCHEMA
        || fixture.evaluation_mode != MODE
        || fixture.notice != NOTICE
        || !valid_owner(&fixture.owner_declaration)
        || fixture.device.owner != fixture.owner_declaration
        || fixture.device.approval_state != "approved"
        || fixture.device.credential_state != "active"
        || fixture.challenge.consumed
        || fixture.challenge.expires_at_ms <= fixture.challenge.issued_at_ms
        || fixture.proof.device_id != fixture.device.device_id
        || fixture.proof.key_id != fixture.device.key_id
        || fixture.proof.owner != fixture.owner_declaration
        || fixture.proof.challenge_id != fixture.challenge.challenge_id
        || fixture.proof.challenge_sha256 != fixture.challenge.challenge_sha256
        || fixture.proof.issued_at_ms >= fixture.proof.expires_at_ms
        || !fixture.expected.accepted
        || fixture.expected.reason != "bound_approved"
        || !fixture.expected.identity_bound
        || fixture.expected.approval_required
        || fixture.authority != Authority::default()
    {
        return false;
    }

    let Ok(public_key_bytes) = URL_SAFE_NO_PAD.decode(&fixture.proof.public_key_base64url) else {
        return false;
    };
    let Ok(public_key_array) = <[u8; 32]>::try_from(public_key_bytes.as_slice()) else {
        return false;
    };
    let digest = Sha256::digest(public_key_array);
    let digest_hex = hex_lower(&digest);
    if !valid_digest(&fixture.device.public_key_sha256)
        || digest_hex != fixture.device.public_key_sha256
    {
        return false;
    }

    let Ok(signature_bytes) = URL_SAFE_NO_PAD.decode(&fixture.proof.signature_base64url) else {
        return false;
    };
    let Ok(signature_array) = <[u8; 64]>::try_from(signature_bytes.as_slice()) else {
        return false;
    };
    let Ok(verifying_key) = VerifyingKey::from_bytes(&public_key_array) else {
        return false;
    };
    let signature = Signature::from_bytes(&signature_array);
    let payload = Payload {
        domain: SCHEMA,
        device_id: &fixture.proof.device_id,
        key_id: &fixture.proof.key_id,
        public_key_sha256: &digest_hex,
        owner: &fixture.proof.owner,
        challenge_id: &fixture.proof.challenge_id,
        challenge_sha256: &fixture.proof.challenge_sha256,
        issued_at_ms: fixture.proof.issued_at_ms,
        expires_at_ms: fixture.proof.expires_at_ms,
    };
    let Ok(payload_bytes) = serde_json::to_vec(&payload) else {
        return false;
    };
    verifying_key.verify(&payload_bytes, &signature).is_ok()
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn forge_signed_device_identity_proof_receiver_is_strict_and_inert() {
    let fixture: Fixture = decode(FIXTURE).expect("signed identity fixture");
    assert!(display_only(&fixture));
}

#[test]
fn forge_signed_device_identity_proof_rejects_mutation() {
    let mut unknown: Value = serde_json::from_slice(FIXTURE).unwrap();
    unknown
        .as_object_mut()
        .unwrap()
        .insert("unexpected".into(), Value::Bool(true));
    assert!(decode::<Fixture>(&serde_json::to_vec(&unknown).unwrap()).is_err());

    let duplicate = br#"{"schema_version":"forge.device-identity-proof/ed25519/v1","schema_version":"forge.device-identity-proof/ed25519/v1"}"#;
    assert!(decode::<Fixture>(duplicate).is_err());

    let mut signature: Value = serde_json::from_slice(FIXTURE).unwrap();
    let original = signature["proof"]["signature_base64url"].as_str().unwrap();
    signature["proof"]["signature_base64url"] = Value::String(format!("f{}", &original[1..]));
    let mutated: Fixture = decode(&serde_json::to_vec(&signature).unwrap()).unwrap();
    assert!(!display_only(&mutated));

    let mut authority: Value = serde_json::from_slice(FIXTURE).unwrap();
    authority["authority"]["identity_verified"] = Value::Bool(true);
    let mutated: Fixture = decode(&serde_json::to_vec(&authority).unwrap()).unwrap();
    assert!(!display_only(&mutated));

    let mut trailing = FIXTURE.to_vec();
    trailing.extend_from_slice(b" true");
    assert!(decode::<Fixture>(&trailing).is_err());
}

fn reject_duplicate_keys(bytes: &[u8]) -> Result<(), String> {
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    UniqueJson
        .deserialize(&mut decoder)
        .map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())
}

struct UniqueJson;
impl<'de> DeserializeSeed<'de> for UniqueJson {
    type Value = ();
    fn deserialize<D: de::Deserializer<'de>>(self, decoder: D) -> Result<Self::Value, D::Error> {
        decoder.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for UniqueJson {
    type Value = ();
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }
    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_borrowed_str<E>(self, _: &'de str) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_string<E>(self, _: String) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_some<D>(self, decoder: D) -> Result<Self::Value, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        decoder.deserialize_any(self)
    }
    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while seq.next_element_seed(UniqueJson)?.is_some() {}
        Ok(())
    }
    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut keys = std::collections::HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(de::Error::custom("duplicate JSON object key"));
            }
            map.next_value_seed(UniqueJson)?;
        }
        Ok(())
    }
}
