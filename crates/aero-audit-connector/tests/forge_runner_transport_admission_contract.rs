use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::{collections::BTreeSet, fmt};

const FIXTURE: &[u8] = include_bytes!("testdata/forge-runner-transport-admission-v1.json");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Owner {
    issuer: String,
    subject: String,
    tenant_id: String,
}

#[derive(Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct Authority {
    device_identity_verified: bool,
    transport_authenticated: bool,
    reservation_created: bool,
    execution_authorized: bool,
    dispatch_performed: bool,
    audit_published: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Observation {
    schema_version: String,
    evaluation_mode: String,
    owner: Owner,
    conversation_id: String,
    run_id: String,
    attempt_id: String,
    attempt_state: String,
    attempt_state_admissible: bool,
    command_id: String,
    command_sha256: String,
    target_id: String,
    lease_epoch: u64,
    lease_issued_at_ms: u64,
    lease_expires_at_ms: u64,
    evaluated_at_ms: u64,
    transport_method: String,
    transport_path: String,
    transport_timestamp: i64,
    transport_nonce: String,
    transport_payload_sha256: String,
    transport_payload_bytes: usize,
    transport_replay_checked: bool,
    lease_proof_current: bool,
    lease_active: bool,
    command_binding_valid: bool,
    transport_binding_valid: bool,
    admission_ready: bool,
    rejection_reasons: Vec<String>,
    preview_only: bool,
    authority: Authority,
}

fn read_fixture() -> Value {
    reject_duplicate_json_keys(FIXTURE)
        .expect("Runner transport admission fixture has unique keys");
    serde_json::from_slice(FIXTURE).expect("decode Runner transport admission fixture")
}

fn decode(value: &Value) -> Result<Observation, &'static str> {
    let observation: Observation =
        serde_json::from_value(value.clone()).map_err(|_| "invalid Runner transport admission")?;
    if observation.schema_version != "forge.runner-transport-admission/v1" {
        return Err("schema drift");
    }
    if observation.evaluation_mode != "fenced_runner_transport_admission_preview"
        || observation.owner.issuer != "https://id.example"
        || observation.owner.subject != "user-1"
        || observation.owner.tenant_id != "tenant-1"
        || observation.conversation_id != "conversation-1"
        || observation.run_id != "run-1"
        || observation.attempt_id != "attempt-1"
        || observation.attempt_state != "accepted"
        || !observation.attempt_state_admissible
        || observation.command_id != "command-1"
        || !valid_digest(&observation.command_sha256)
        || observation.target_id != "runner-1"
        || observation.lease_epoch != 1
        || observation.lease_expires_at_ms <= observation.lease_issued_at_ms
        || observation.evaluated_at_ms != 300
        || observation.transport_method != "POST"
        || observation.transport_path != "/api/v1/runners/runner-1/dispatch"
        || observation.transport_timestamp <= 0
        || observation.transport_nonce.is_empty()
        || !valid_digest(&observation.transport_payload_sha256)
        || observation.transport_payload_bytes == 0
        || !observation.transport_replay_checked
        || !observation.lease_proof_current
        || !observation.lease_active
        || !observation.command_binding_valid
        || !observation.transport_binding_valid
        || !observation.admission_ready
        || !observation.rejection_reasons.is_empty()
        || !observation.preview_only
        || observation.authority != Authority::default()
    {
        return Err("Runner transport admission semantic validation failed");
    }
    Ok(observation)
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[test]
fn runner_transport_admission_fixture_is_strict_and_metadata_only() {
    let observation = decode(&read_fixture()).expect("valid Runner transport admission fixture");
    assert!(!format!("{observation:?}").contains("fencing_token"));
}

#[test]
fn runner_transport_admission_fixture_rejects_authority_and_unknown_fields() {
    let fixture = read_fixture();
    let mut authority = fixture.clone();
    authority["authority"]["dispatch_performed"] = Value::Bool(true);
    assert!(decode(&authority).is_err());

    let mut unknown = fixture;
    let object = unknown.as_object_mut().unwrap();
    object.insert("unexpected".into(), Value::Null);
    assert!(serde_json::from_value::<Observation>(unknown).is_err());
}

#[test]
fn runner_transport_admission_fixture_rejects_duplicate_keys_before_decode() {
    let source = String::from_utf8_lossy(FIXTURE);
    let duplicate = source.replacen(
        "\"admission_ready\": true",
        "\"admission_ready\": true, \"admission_ready\": true",
        1,
    );
    assert!(reject_duplicate_json_keys(duplicate.as_bytes()).is_err());
}

fn reject_duplicate_json_keys(raw: &[u8]) -> Result<(), String> {
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    decoder
        .deserialize_any(ScanVisitor)
        .map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())
}

struct ScanSeed;
struct ScanVisitor;

impl<'de> DeserializeSeed<'de> for ScanSeed {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<(), D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(ScanVisitor)
    }
}

impl<'de> Visitor<'de> for ScanVisitor {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON without duplicate object keys")
    }

    fn visit_map<A>(self, mut map: A) -> Result<(), A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut keys = BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(de::Error::custom("duplicate JSON key"));
            }
            map.next_value_seed(ScanSeed)?;
        }
        Ok(())
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<(), A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence.next_element_seed(ScanSeed)?.is_some() {}
        Ok(())
    }

    fn visit_bool<E>(self, _: bool) -> Result<(), E>
    where
        E: de::Error,
    {
        Ok(())
    }

    fn visit_i64<E>(self, _: i64) -> Result<(), E>
    where
        E: de::Error,
    {
        Ok(())
    }

    fn visit_u64<E>(self, _: u64) -> Result<(), E>
    where
        E: de::Error,
    {
        Ok(())
    }

    fn visit_f64<E>(self, _: f64) -> Result<(), E>
    where
        E: de::Error,
    {
        Ok(())
    }

    fn visit_str<E>(self, _: &str) -> Result<(), E>
    where
        E: de::Error,
    {
        Ok(())
    }

    fn visit_string<E>(self, _: String) -> Result<(), E>
    where
        E: de::Error,
    {
        Ok(())
    }

    fn visit_unit<E>(self) -> Result<(), E>
    where
        E: de::Error,
    {
        Ok(())
    }
}
