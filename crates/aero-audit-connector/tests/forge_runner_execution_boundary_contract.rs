use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::{collections::BTreeSet, fmt};

const FIXTURE: &[u8] = include_bytes!("testdata/forge-runner-execution-boundary-v1.json");

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
    command_persisted: bool,
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
    mode: String,
    owner: Owner,
    conversation_id: String,
    run_id: String,
    attempt_id: String,
    attempt_state: String,
    command_id: String,
    command_sha256: String,
    target_id: String,
    lease_epoch: u64,
    activation_allowed: bool,
    runner_authority_accepted: bool,
    dispatch_admission_ready: bool,
    transport_admission_ready: bool,
    effect_state: String,
    effect_state_startable: bool,
    cancellation_clear: bool,
    execution_boundary_ready: bool,
    rejection_reasons: Vec<String>,
    preview_only: bool,
    authority: Authority,
}

fn read_fixture() -> Value {
    reject_duplicate_json_keys(FIXTURE).expect("execution boundary fixture has unique keys");
    serde_json::from_slice(FIXTURE).expect("decode execution boundary fixture")
}

fn decode(value: &Value) -> Result<Observation, &'static str> {
    let observation: Observation =
        serde_json::from_value(value.clone()).map_err(|_| "invalid execution boundary")?;
    if observation.schema_version != "forge.runner-execution-boundary/v1"
        || observation.evaluation_mode != "p4_runner_authority_execution_boundary_preview"
        || observation.mode != "execute"
        || observation.owner.issuer != "https://id.example"
        || observation.owner.subject != "user-1"
        || observation.owner.tenant_id != "tenant-1"
        || observation.conversation_id != "conversation-1"
        || observation.run_id != "run-1"
        || observation.attempt_id != "attempt-1"
        || observation.attempt_state != "accepted"
        || observation.command_id != "command-1"
        || observation.command_sha256
            != "3c0540dea301f478947bde3d4305fd55e925a19553f73f55c47676f0b766dbeb"
        || observation.target_id != "runner-a"
        || observation.lease_epoch != 3
        || !observation.activation_allowed
        || !observation.runner_authority_accepted
        || !observation.dispatch_admission_ready
        || !observation.transport_admission_ready
        || observation.effect_state != "not_started"
        || !observation.effect_state_startable
        || !observation.cancellation_clear
        || !observation.execution_boundary_ready
        || !observation.rejection_reasons.is_empty()
        || !observation.preview_only
        || observation.authority != Authority::default()
    {
        return Err("execution boundary semantic validation failed");
    }
    Ok(observation)
}

#[test]
fn runner_execution_boundary_fixture_is_strict_and_metadata_only() {
    let observation = decode(&read_fixture()).expect("valid execution boundary fixture");
    assert_eq!(observation.target_id, "runner-a");
    assert!(observation.execution_boundary_ready);
}

#[test]
fn runner_execution_boundary_fixture_rejects_authority_unknown_and_readiness_drift() {
    let fixture = read_fixture();
    let mut authority = fixture.clone();
    authority["authority"]["execution_authorized"] = Value::Bool(true);
    assert!(decode(&authority).is_err());

    let mut unknown = fixture.clone();
    unknown
        .as_object_mut()
        .unwrap()
        .insert("unexpected".into(), Value::Null);
    assert!(serde_json::from_value::<Observation>(unknown).is_err());

    let mut readiness = fixture;
    readiness["execution_boundary_ready"] = Value::Bool(false);
    assert!(decode(&readiness).is_err());
}

#[test]
fn runner_execution_boundary_fixture_rejects_duplicate_keys_before_decode() {
    let source = String::from_utf8_lossy(FIXTURE);
    let duplicate = source.replacen(
        "\"execution_boundary_ready\": true",
        "\"execution_boundary_ready\": true, \"execution_boundary_ready\": true",
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
