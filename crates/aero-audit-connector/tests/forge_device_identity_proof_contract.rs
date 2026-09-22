//! The identity proof fixture is a pure binding vector. This receiver validates
//! shape and inert authority markers; it performs no cryptography, persistence,
//! challenge consumption, enrollment, credential issuance, or execution.
use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;

const FIXTURE: &[u8] = include_bytes!("testdata/forge-device-identity-proof-contract-v1.json");
const NOTICE: &str = "This fixture checks exact device-owner-key and one-time challenge binding only. The proof digest is a test-vector label; no cryptographic verifier, credential issuer, persistence, network, approval write, inventory authority, or execution authority is present.";

#[derive(Debug, Deserialize, PartialEq, Eq)]
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
struct Proof {
    device_id: String,
    key_id: String,
    public_key_sha256: String,
    owner: Owner,
    challenge_id: String,
    challenge_sha256: String,
    proof_sha256: String,
    issued_at_ms: u64,
    expires_at_ms: u64,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Expected {
    accepted: bool,
    reason: String,
    identity_bound: bool,
    approval_required: bool,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    now_ms: u64,
    challenge_consumed: bool,
    device_approval_state: String,
    device_credential_state: String,
    proof: Proof,
    expected: Expected,
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
    authority: Authority,
    cases: Vec<Case>,
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
        && value.len() <= 128
        && value.trim() == value
        && !value.chars().any(char::is_control)
}
fn valid_owner(owner: &Owner) -> bool {
    valid_token(&owner.issuer) && valid_token(&owner.subject) && valid_token(&owner.tenant_id)
}
fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}
fn display_only(fixture: &Fixture) -> bool {
    fixture.schema_version == "forge.device-identity-proof-contract/v1"
        && fixture.evaluation_mode == "pure_binding_only"
        && fixture.notice == NOTICE
        && valid_owner(&fixture.owner_declaration)
        && fixture.device.owner == fixture.owner_declaration
        && valid_token(&fixture.device.device_id)
        && valid_token(&fixture.device.key_id)
        && valid_digest(&fixture.device.public_key_sha256)
        && fixture.device.approval_state == "approved"
        && fixture.device.credential_state == "active"
        && valid_token(&fixture.challenge.challenge_id)
        && valid_digest(&fixture.challenge.challenge_sha256)
        && fixture.challenge.issued_at_ms > 0
        && fixture.challenge.expires_at_ms > fixture.challenge.issued_at_ms
        && !fixture.challenge.consumed
        && fixture.authority == Authority::default()
}

#[test]
fn forge_device_identity_proof_receiver_is_strict_and_inert() {
    let fixture: Fixture = decode(FIXTURE).expect("identity proof fixture");
    assert!(display_only(&fixture));
    assert_eq!(fixture.cases.len(), 12);
    let mut names = HashSet::new();
    for case in fixture.cases {
        assert!(names.insert(case.name));
        assert!(case.now_ms > 0 && case.now_ms <= 9_007_199_254_740_991);
        assert!(
            !case.challenge_consumed || case.proof.challenge_id == fixture.challenge.challenge_id
        );
        assert!(matches!(
            case.device_approval_state.as_str(),
            "approved" | "pending"
        ));
        assert!(matches!(
            case.device_credential_state.as_str(),
            "active" | "revoked" | "expired"
        ));
        assert!(
            valid_token(&case.proof.device_id)
                && valid_token(&case.proof.key_id)
                && valid_token(&case.proof.challenge_id)
        );
        assert!(
            valid_digest(&case.proof.public_key_sha256)
                && valid_digest(&case.proof.challenge_sha256)
                && valid_digest(&case.proof.proof_sha256)
        );
        assert!(
            valid_owner(&case.proof.owner)
                && case.proof.issued_at_ms > 0
                && case.proof.expires_at_ms > case.proof.issued_at_ms
        );
        if case.expected.accepted {
            assert!(matches!(
                case.expected.reason.as_str(),
                "bound_approved" | "bound_pending_approval"
            ));
            assert!(case.expected.identity_bound);
        } else {
            assert!(
                !case.expected.reason.is_empty()
                    && !case.expected.identity_bound
                    && !case.expected.approval_required
            );
        }
    }
}

#[test]
fn forge_device_identity_proof_receiver_rejects_unknown_duplicate_trailing_and_authority() {
    let mut unknown: Value = serde_json::from_slice(FIXTURE).unwrap();
    unknown
        .as_object_mut()
        .unwrap()
        .insert("unexpected".into(), Value::Bool(true));
    assert!(decode::<Fixture>(&serde_json::to_vec(&unknown).unwrap()).is_err());
    let duplicate = br#"{"schema_version":"forge.device-identity-proof-contract/v1","schema_version":"forge.device-identity-proof-contract/v1"}"#;
    assert!(decode::<Fixture>(duplicate).is_err());
    let mut authority: Value = serde_json::from_slice(FIXTURE).unwrap();
    authority["authority"]["identity_verified"] = Value::Bool(true);
    let decoded: Fixture = decode(&serde_json::to_vec(&authority).unwrap()).unwrap();
    assert!(!display_only(&decoded));
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
    fn deserialize<D: de::Deserializer<'de>>(self, decoder: D) -> Result<(), D::Error> {
        decoder.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for UniqueJson {
    type Value = ();
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("unique JSON object keys")
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while seq.next_element_seed(UniqueJson)?.is_some() {}
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let mut keys = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(de::Error::custom("duplicate JSON object key"));
            }
            map.next_value_seed(UniqueJson)?;
        }
        Ok(())
    }
}
