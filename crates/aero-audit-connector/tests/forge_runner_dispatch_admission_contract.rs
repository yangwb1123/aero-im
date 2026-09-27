//! The lease-bound Runner admission value is an owner/Attempt recheck only.
//! Aero-IM consumes the projection for interoperability and never turns it
//! into lease, dispatch, execution, or Audit authority.

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::{collections::BTreeSet, fmt};

const FIXTURE: &[u8] = include_bytes!("testdata/forge-runner-dispatch-admission-v1.json");
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Owner {
    issuer: String,
    subject: String,
    tenant_id: String,
}
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Authority {
    device_identity_verified: bool,
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
    lease_proof_current: bool,
    lease_active: bool,
    command_binding_valid: bool,
    admission_ready: bool,
    rejection_reasons: Vec<String>,
    preview_only: bool,
    authority: Authority,
}

fn decode(raw: &[u8]) -> Result<Observation, String> {
    reject_duplicate_json_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value = Observation::deserialize(&mut decoder).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    validate(&value)?;
    Ok(value)
}

fn validate(value: &Observation) -> Result<(), String> {
    if value.schema_version != "forge.runner-dispatch-admission/v1"
        || value.evaluation_mode != "durable_lease_bound_dispatch_admission_preview"
        || !valid_owner(&value.owner)
        || !valid_identifier(&value.conversation_id)
        || !valid_identifier(&value.run_id)
        || !valid_identifier(&value.attempt_id)
        || !valid_identifier(&value.command_id)
        || !valid_digest(&value.command_sha256)
        || !valid_identifier(&value.target_id)
        || !valid_state(&value.attempt_state)
        || value.attempt_state_admissible != dispatchable(&value.attempt_state)
        || value.lease_epoch == 0
        || value.lease_issued_at_ms == 0
        || value.lease_expires_at_ms <= value.lease_issued_at_ms
        || value.lease_expires_at_ms > MAX_SAFE_INTEGER
        || value.evaluated_at_ms == 0
        || value.evaluated_at_ms > MAX_SAFE_INTEGER
        || value.admission_ready
            != (value.command_binding_valid
                && value.lease_proof_current
                && value.lease_active
                && value.attempt_state_admissible)
        || !valid_reasons(
            &value.rejection_reasons,
            value.command_binding_valid,
            value.lease_proof_current,
            value.lease_active,
            value.attempt_state_admissible,
        )
        || !value.preview_only
        || value.authority != Authority::default()
    {
        return Err("invalid Runner dispatch admission".into());
    }
    Ok(())
}
fn valid_owner(owner: &Owner) -> bool {
    [
        owner.issuer.as_str(),
        owner.subject.as_str(),
        owner.tenant_id.as_str(),
    ]
    .into_iter()
    .all(valid_owner_part)
}
fn valid_owner_part(value: &str) -> bool {
    !value.is_empty() && value.trim() == value && !value.chars().any(char::is_control)
}
fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.chars().enumerate().all(|(index, c)| {
            c.is_ascii_alphanumeric()
                || (index > 0 && matches!(c, '.' | '_' | ':' | '+' | '/' | '-'))
        })
}
fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}
fn valid_state(value: &str) -> bool {
    matches!(
        value,
        "requested"
            | "accepted"
            | "starting"
            | "running"
            | "interrupted"
            | "completed"
            | "failed"
            | "uncertain"
    )
}
fn dispatchable(value: &str) -> bool {
    matches!(value, "accepted" | "starting" | "running")
}
fn valid_reasons(
    values: &[String],
    command: bool,
    current: bool,
    active: bool,
    state: bool,
) -> bool {
    let mut expected = Vec::new();
    if !command {
        expected.push("command_binding_invalid")
    };
    if !current {
        expected.push("lease_proof_not_current")
    };
    if !active {
        expected.push("lease_inactive_at_evaluated_time")
    };
    if !state {
        expected.push("attempt_state_not_dispatchable")
    }
    values.iter().map(String::as_str).eq(expected.into_iter())
}

#[test]
fn canonical_admission_is_bounded_and_authority_free() {
    let value = decode(FIXTURE).expect("canonical admission fixture");
    assert_eq!(value.owner.subject, "user-1");
    assert_eq!(value.target_id, "runner-a");
    assert!(value.admission_ready);
    assert_eq!(value.authority, Authority::default());
}

#[test]
fn unknown_duplicate_and_authority_mutations_fail_closed() {
    let mut root: serde_json::Map<String, Value> = serde_json::from_slice(FIXTURE).unwrap();
    root.insert("unexpected".into(), Value::Bool(true));
    assert!(decode(&serde_json::to_vec(&root).unwrap()).is_err());
    let duplicate = format!(
        "{},\"schema_version\":\"forge.runner-dispatch-admission/v1\"}}",
        String::from_utf8_lossy(FIXTURE).trim_end_matches('}')
    );
    assert!(decode(duplicate.as_bytes()).is_err());
    let authority = String::from_utf8_lossy(FIXTURE).replace(
        "\"execution_authorized\": false",
        "\"execution_authorized\": true",
    );
    assert!(decode(authority.as_bytes()).is_err());
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
