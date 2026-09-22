//! Forge Runner dispatch-plan preview is a bounded comparison observation.
//!
//! This receiver does not authenticate a Runner, persist a Run or Attempt,
//! select a target, reserve capacity, dispatch work, publish Audit, or grant
//! execution authority.

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer as _};
use serde_json::{Map, Value};
use std::fmt;

const FIXTURE: &[u8] = include_bytes!("testdata/forge-runner-dispatch-plan-preview-v1.json");
const SCHEMA_VERSION: &str = "forge.runner-dispatch-plan-preview/v1";
const EVALUATION_MODE: &str = "pure_dispatch_plan_preview_only";
const MAX_CANDIDATES: usize = 128;
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Owner {
    issuer: String,
    subject: String,
    tenant_id: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Candidate {
    target_id: String,
    attributes_unverified: bool,
    matches_requirements: bool,
    lease_target_match: bool,
    lease_active: bool,
    attempt_state_admissible: bool,
    declarative_ready: bool,
    reasons: Vec<String>,
}

#[derive(Debug, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Authority {
    device_identity_verified: bool,
    attempt_persisted: bool,
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
    owner_declaration: Owner,
    conversation_id: String,
    run_id: String,
    attempt_id: String,
    attempt_state: String,
    attempt_state_admissible: bool,
    command_id: String,
    command_sha256: String,
    intent_target_id: String,
    lease_epoch: u64,
    lease_active: bool,
    evaluated_at_ms: i64,
    candidate_count: usize,
    declarative_ready_count: usize,
    candidates: Vec<Candidate>,
    selected_target_id: Option<String>,
    preview_only: bool,
    reservation_created: bool,
    execution_authorized: bool,
    dispatch_performed: bool,
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
    if value.schema_version != SCHEMA_VERSION
        || value.evaluation_mode != EVALUATION_MODE
        || !valid_owner(&value.owner_declaration)
        || !valid_identifier(&value.conversation_id)
        || !valid_identifier(&value.run_id)
        || !valid_identifier(&value.attempt_id)
        || !valid_identifier(&value.command_id)
        || !valid_digest(&value.command_sha256)
        || !valid_identifier(&value.intent_target_id)
        || !valid_attempt_state(&value.attempt_state)
        || value.attempt_state_admissible != dispatchable_attempt_state(&value.attempt_state)
        || value.lease_epoch == 0
        || value.evaluated_at_ms <= 0
        || value.evaluated_at_ms > MAX_SAFE_INTEGER
        || value.selected_target_id.is_some()
        || !value.preview_only
        || value.reservation_created
        || value.execution_authorized
        || value.dispatch_performed
        || value.authority != Authority::default()
        || value.candidate_count != value.candidates.len()
        || value.candidate_count > MAX_CANDIDATES
    {
        return Err("invalid bounded Runner dispatch-plan preview".into());
    }

    let mut ready_count = 0;
    for (index, candidate) in value.candidates.iter().enumerate() {
        if !valid_identifier(&candidate.target_id)
            || !candidate.attributes_unverified
            || candidate.lease_target_match != (candidate.target_id == value.intent_target_id)
            || candidate.lease_active != value.lease_active
            || candidate.attempt_state_admissible != value.attempt_state_admissible
            || candidate.declarative_ready
                != (candidate.matches_requirements
                    && candidate.lease_target_match
                    && candidate.lease_active
                    && candidate.attempt_state_admissible)
            || !valid_reasons(&candidate.reasons)
            || (candidate.declarative_ready && !candidate.reasons.is_empty())
        {
            return Err(format!("invalid Runner dispatch-plan candidate {index}"));
        }
        if index > 0 && value.candidates[index - 1].target_id >= candidate.target_id {
            return Err("Runner dispatch-plan candidates are not sorted uniquely".into());
        }
        if candidate.declarative_ready {
            ready_count += 1;
        }
    }
    if ready_count != value.declarative_ready_count {
        return Err("Runner dispatch-plan ready count mismatch".into());
    }
    Ok(())
}

fn valid_owner(owner: &Owner) -> bool {
    valid_owner_part(&owner.issuer)
        && valid_owner_part(&owner.subject)
        && valid_owner_part(&owner.tenant_id)
}

fn valid_owner_part(value: &str) -> bool {
    !value.is_empty() && value.trim() == value && !value.chars().any(char::is_control)
}

fn valid_identifier(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    value.chars().enumerate().all(|(index, character)| {
        character.is_ascii_alphanumeric()
            || (index > 0 && matches!(character, '.' | '_' | ':' | '-' | '+' | '/'))
    })
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn valid_attempt_state(value: &str) -> bool {
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

fn dispatchable_attempt_state(value: &str) -> bool {
    matches!(value, "accepted" | "starting" | "running")
}

fn valid_reasons(values: &[String]) -> bool {
    values.iter().enumerate().all(|(index, value)| {
        !value.is_empty()
            && !value.chars().any(char::is_control)
            && (index == 0 || values[index - 1] < *value)
    })
}

#[test]
fn canonical_dispatch_plan_is_bounded_and_authority_free() {
    let value = decode(FIXTURE).expect("canonical Runner dispatch-plan preview fixture");
    assert_eq!(value.schema_version, SCHEMA_VERSION);
    assert_eq!(value.evaluation_mode, EVALUATION_MODE);
    assert_eq!(value.owner_declaration.issuer, "https://id.example");
    assert_eq!(value.owner_declaration.subject, "user-1");
    assert_eq!(value.owner_declaration.tenant_id, "tenant-1");
    assert_eq!(value.attempt_state, "accepted");
    assert!(value.attempt_state_admissible);
    assert_eq!(value.intent_target_id, "runner-1");
    assert!(value.lease_active);
    assert_eq!(value.candidate_count, 2);
    assert_eq!(value.declarative_ready_count, 1);
    assert_eq!(value.candidates[0].target_id, "runner-1");
    assert!(value.candidates[0].declarative_ready);
    assert_eq!(value.candidates[1].target_id, "runner-2");
    assert!(!value.candidates[1].declarative_ready);
    assert!(value.selected_target_id.is_none());
    assert!(value.preview_only);
    assert_eq!(value.authority, Authority::default());
}

#[test]
fn unknown_duplicate_owner_candidate_selection_and_authority_mutations_fail_closed() {
    let mut root: Map<String, Value> = serde_json::from_slice(FIXTURE).expect("object fixture");
    root.insert("unexpected".into(), Value::Bool(true));
    assert!(decode(&serde_json::to_vec(&root).expect("unknown mutation")).is_err());

    let duplicate = format!(
        "{},\"schema_version\":\"forge.runner-dispatch-plan-preview/v1\"}}",
        String::from_utf8_lossy(FIXTURE).trim_end_matches('}')
    );
    assert!(decode(duplicate.as_bytes()).is_err());

    let nested_duplicate = String::from_utf8_lossy(FIXTURE).replace(
        "    \"audit_published\": false\n  }",
        "    \"audit_published\": false,\n    \"audit_published\": false\n  }",
    );
    assert_ne!(nested_duplicate.as_bytes(), FIXTURE);
    assert!(decode(nested_duplicate.as_bytes()).is_err());

    let owner =
        String::from_utf8_lossy(FIXTURE).replace("\"subject\": \"user-1\"", "\"subject\": \"\"");
    assert!(decode(owner.as_bytes()).is_err());

    let order = String::from_utf8_lossy(FIXTURE)
        .replace("\"target_id\": \"runner-1\"", "\"target_id\": \"runner-3\"");
    assert!(decode(order.as_bytes()).is_err());

    let readiness = String::from_utf8_lossy(FIXTURE).replace(
        "\"declarative_ready\": true",
        "\"declarative_ready\": false",
    );
    assert!(decode(readiness.as_bytes()).is_err());

    let mut selected: Map<String, Value> = serde_json::from_slice(FIXTURE).expect("object fixture");
    selected.insert(
        "selected_target_id".into(),
        Value::String("runner-1".into()),
    );
    assert!(decode(&serde_json::to_vec(&selected).expect("selection mutation")).is_err());

    let mut authority: Map<String, Value> =
        serde_json::from_slice(FIXTURE).expect("object fixture");
    let mut authority_value = authority.remove("authority").expect("authority object");
    if let Value::Object(ref mut fields) = authority_value {
        fields.insert("audit_published".into(), Value::Bool(true));
    } else {
        panic!("authority is not an object");
    }
    authority.insert("authority".into(), authority_value);
    assert!(decode(&serde_json::to_vec(&authority).expect("authority mutation")).is_err());
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

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
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

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut keys = std::collections::BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(de::Error::custom(format!("duplicate JSON key {key:?}")));
            }
            map.next_value_seed(ScanSeed)?;
        }
        Ok(())
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence.next_element_seed(ScanSeed)?.is_some() {}
        Ok(())
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(())
    }

    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(())
    }

    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(())
    }

    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(())
    }

    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(())
    }

    fn visit_string<E>(self, _: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(())
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(())
    }
}
