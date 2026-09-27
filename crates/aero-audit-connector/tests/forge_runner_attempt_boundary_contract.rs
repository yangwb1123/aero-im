//! Strict Aero-IM receiver for the pure Attempt lifecycle dispatch boundary.
//!
//! This test consumes display-only metadata. It never persists an Attempt,
//! changes a lease, opens Runner transport, or grants execution authority.

use serde::Deserialize;
use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use std::{collections::BTreeSet, fmt};

const FIXTURE: &[u8] = include_bytes!("testdata/forge-runner-attempt-boundary-v1.json");
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct Owner {
    issuer: String,
    subject: String,
    tenant_id: String,
}

#[derive(Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct Authority {
    attempt_persisted: bool,
    reservation_created: bool,
    execution_authorized: bool,
    dispatch_performed: bool,
    audit_published: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Boundary {
    schema_version: String,
    evaluation_mode: String,
    owner: Owner,
    conversation_id: String,
    run_id: String,
    attempt_id: String,
    command_id: String,
    target_id: String,
    lease_epoch: u64,
    current_attempt_state: String,
    next_attempt_state: String,
    transition: String,
    execution_boundary_ready: bool,
    attempt_transition_valid: bool,
    attempt_transition_dispatchable: bool,
    attempt_boundary_ready: bool,
    rejection_reasons: Vec<String>,
    preview_only: bool,
    authority: Authority,
}

#[test]
fn runner_attempt_boundary_is_bounded_and_authority_free() {
    let value = decode(FIXTURE).expect("canonical Attempt boundary fixture");
    validate(&value).expect("canonical Attempt boundary");
    assert_eq!(value.owner.issuer, "https://id.example");
    assert_eq!(value.owner.subject, "user-1");
    assert_eq!(value.owner.tenant_id, "tenant-1");
    assert_eq!(value.conversation_id, "conversation-1");
    assert_eq!(value.run_id, "run-1");
    assert_eq!(value.attempt_id, "attempt-1");
    assert_eq!(value.command_id, "command-1");
    assert_eq!(value.target_id, "runner-1");
    assert_eq!(value.lease_epoch, 1);
    assert!(value.attempt_boundary_ready);
}

#[test]
fn runner_attempt_boundary_rejects_wire_and_semantic_drift() {
    let source = String::from_utf8_lossy(FIXTURE);
    let duplicate = format!(
        "{},\"schema_version\":\"forge.runner-attempt-boundary/v1\"}}",
        source.trim_end_matches('}')
    );
    assert!(decode(duplicate.as_bytes()).is_err());
    let unknown = source.replacen(
        "\"evaluation_mode\": \"attempt_lifecycle_dispatch_boundary_preview\",",
        "\"evaluation_mode\": \"attempt_lifecycle_dispatch_boundary_preview\", \"unexpected\": true,",
        1,
    );
    assert!(decode(unknown.as_bytes()).is_err());
    assert!(decode(format!("{source} {{}}").as_bytes()).is_err());

    let mut readiness: Value = serde_json::from_slice(FIXTURE).expect("fixture value");
    readiness["attempt_boundary_ready"] = Value::Bool(false);
    let readiness: Boundary = serde_json::from_value(readiness).expect("readiness mutation");
    assert!(validate(&readiness).is_err());

    let mut authority: Value = serde_json::from_slice(FIXTURE).expect("fixture value");
    authority["authority"]["dispatch_performed"] = Value::Bool(true);
    let authority: Boundary = serde_json::from_value(authority).expect("authority mutation");
    assert!(validate(&authority).is_err());

    let mut non_ready: Value = serde_json::from_slice(FIXTURE).expect("fixture value");
    non_ready["execution_boundary_ready"] = Value::Bool(false);
    non_ready["attempt_boundary_ready"] = Value::Bool(false);
    non_ready["rejection_reasons"] = serde_json::json!(["execution_boundary_not_ready"]);
    let non_ready: Boundary = serde_json::from_value(non_ready).expect("non-ready mutation");
    validate(&non_ready).expect("canonical non-ready rejection");
    let mut reason_drift = non_ready;
    reason_drift.rejection_reasons = vec!["unexpected_reason".into()];
    assert!(validate(&reason_drift).is_err());
}

fn decode(raw: &[u8]) -> Result<Boundary, String> {
    reject_duplicate_json_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value = Boundary::deserialize(&mut decoder).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    validate(&value)?;
    Ok(value)
}

fn validate(value: &Boundary) -> Result<(), String> {
    if value.schema_version != "forge.runner-attempt-boundary/v1"
        || value.evaluation_mode != "attempt_lifecycle_dispatch_boundary_preview"
        || value.lease_epoch == 0
        || value.lease_epoch > MAX_SAFE_INTEGER
        || !value.preview_only
        || value.authority != Authority::default()
        || !valid_text(&value.owner.issuer, 512)
        || !valid_text(&value.owner.subject, 512)
        || !valid_text(&value.owner.tenant_id, 512)
        || !valid_identifier(&value.conversation_id)
        || !valid_identifier(&value.run_id)
        || !valid_identifier(&value.attempt_id)
        || !valid_identifier(&value.command_id)
        || !valid_identifier(&value.target_id)
        || !valid_state(&value.current_attempt_state)
        || !valid_state(&value.next_attempt_state)
        || !valid_transition(&value.transition)
    {
        return Err("invalid Attempt boundary envelope".into());
    }
    let transition_valid = match (
        value.current_attempt_state.as_str(),
        value.transition.as_str(),
    ) {
        ("accepted", "begin_starting") | ("starting", "observe_running") => true,
        _ => false,
    };
    if value.attempt_transition_valid != transition_valid
        || value.next_attempt_state != transition_target(&value.transition)
    {
        return Err("invalid Attempt transition".into());
    }
    let dispatchable = transition_valid
        && ((value.current_attempt_state == "accepted" && value.next_attempt_state == "starting")
            || (value.current_attempt_state == "starting"
                && value.next_attempt_state == "running"));
    if value.attempt_transition_dispatchable != dispatchable
        || value.attempt_boundary_ready != (value.execution_boundary_ready && dispatchable)
    {
        return Err("invalid Attempt dispatch boundary".into());
    }
    if value.rejection_reasons
        != rejection_reasons(
            value.execution_boundary_ready,
            transition_valid,
            dispatchable,
        )
    {
        return Err("Attempt boundary rejection reasons drift".into());
    }
    if value.execution_boundary_ready && !value.rejection_reasons.is_empty() {
        return Err("ready Attempt boundary has rejection reasons".into());
    }
    Ok(())
}

fn rejection_reasons(
    boundary_ready: bool,
    transition_valid: bool,
    dispatchable: bool,
) -> Vec<String> {
    let mut reasons = Vec::with_capacity(3);
    if !boundary_ready {
        reasons.push("execution_boundary_not_ready".into());
    }
    if !transition_valid {
        reasons.push("attempt_transition_invalid".into());
    } else if !dispatchable {
        reasons.push("attempt_transition_not_dispatchable".into());
    }
    reasons
}

fn valid_text(value: &str, limit: usize) -> bool {
    !value.is_empty()
        && value.len() <= limit
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn valid_identifier(value: &str) -> bool {
    valid_text(value, 128)
        && value.chars().enumerate().all(|(index, character)| {
            character.is_ascii_alphanumeric() || (index > 0 && ".:_+/-".contains(character))
        })
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

fn valid_transition(value: &str) -> bool {
    matches!(
        value,
        "accept"
            | "begin_starting"
            | "observe_running"
            | "observe_interrupted"
            | "observe_completed"
            | "observe_failed"
            | "observe_effect_outcome_uncertain"
    )
}

fn transition_target(value: &str) -> &str {
    match value {
        "accept" => "accepted",
        "begin_starting" => "starting",
        "observe_running" => "running",
        "observe_interrupted" => "interrupted",
        "observe_completed" => "completed",
        "observe_failed" => "failed",
        "observe_effect_outcome_uncertain" => "uncertain",
        _ => "",
    }
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
        D: Deserializer<'de>,
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

    fn visit_bool<E>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E>(self, _: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_string<E>(self, _: String) -> Result<(), E> {
        Ok(())
    }
    fn visit_none<E>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E>(self) -> Result<(), E> {
        Ok(())
    }
}
