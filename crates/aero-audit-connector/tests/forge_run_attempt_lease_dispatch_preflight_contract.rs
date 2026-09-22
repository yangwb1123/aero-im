//! Forge Run/Attempt/lease preflight is a bounded display-only observation.
//! Aero-IM consumes the canonical value without identity, lease, target, or
//! execution authority.

use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};
use std::fmt;

const FIXTURE: &[u8] =
    include_bytes!("testdata/forge-run-attempt-lease-dispatch-preflight-v1.json");

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Owner {
    issuer: String,
    subject: String,
    tenant_id: String,
}

#[derive(Debug, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Authority {
    identity_verified: bool,
    run_authoritative: bool,
    attempt_persisted: bool,
    lease_issued: bool,
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
    run_status: String,
    run_state_admissible: bool,
    attempt_id: String,
    attempt_state: String,
    attempt_state_admissible: bool,
    command_id: String,
    intent_target_id: String,
    lease_epoch: u64,
    lease_active: bool,
    evaluated_at_ms: i64,
    candidate_count: i64,
    declarative_ready_count: i64,
    declarative_preflight_ready: bool,
    rejection_reasons: Vec<String>,
    selected_target_id: Option<String>,
    preview_only: bool,
    authority: Authority,
}

fn decode<T: DeserializeOwned>(raw: &[u8]) -> Result<T, String> {
    reject_duplicate_json_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value = T::deserialize(&mut decoder).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    Ok(value)
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
    fn visit_map<A>(self, mut map: A) -> Result<(), A::Error>
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

fn observation(raw: &[u8]) -> Result<Observation, String> {
    let value: Observation = decode(raw)?;
    if value.schema_version != "forge.run-attempt-lease-dispatch-preflight/v1"
        || value.evaluation_mode != "pure_run_attempt_lease_dispatch_preflight"
        || value.owner.issuer.is_empty()
        || value.owner.subject.is_empty()
        || value.owner.tenant_id.is_empty()
        || value.run_status != "nonterminal"
        || !value.run_state_admissible
        || value.attempt_state != "accepted"
        || !value.attempt_state_admissible
        || value.lease_epoch == 0
        || value.evaluated_at_ms <= 0
        || value.candidate_count < 0
        || value.declarative_ready_count <= 0
        || !value.declarative_preflight_ready
        || !value.rejection_reasons.is_empty()
        || value.selected_target_id.is_some()
        || !value.preview_only
        || value.authority != Authority::default()
    {
        return Err("preflight crossed display-only boundary".into());
    }
    Ok(value)
}

#[test]
fn canonical_preflight_is_bounded_and_authority_free() {
    let value = observation(FIXTURE).expect("canonical Forge preflight fixture");
    assert!(!value.conversation_id.is_empty());
    assert!(!value.run_id.is_empty());
    assert!(!value.attempt_id.is_empty());
    assert!(!value.command_id.is_empty());
    assert!(!value.intent_target_id.is_empty());
    assert!(value.lease_active);
}

#[test]
fn unknown_duplicate_selection_and_authority_mutations_fail_closed() {
    let mut root: Map<String, Value> = serde_json::from_slice(FIXTURE).expect("object fixture");
    root.insert("unexpected".into(), Value::Bool(true));
    assert!(observation(&serde_json::to_vec(&root).expect("unknown mutation")).is_err());

    let duplicate = format!(
        "{},\"run_id\":\"run-001\"}}",
        String::from_utf8_lossy(FIXTURE).trim_end_matches('}')
    );
    assert!(observation(duplicate.as_bytes()).is_err());

    let mut selected: Map<String, Value> = serde_json::from_slice(FIXTURE).expect("object fixture");
    selected.insert(
        "selected_target_id".into(),
        Value::String("runner-1".into()),
    );
    assert!(observation(&serde_json::to_vec(&selected).expect("selection mutation")).is_err());

    let mut authority: Map<String, Value> =
        serde_json::from_slice(FIXTURE).expect("object fixture");
    authority.insert(
        "authority".into(),
        serde_json::json!({"dispatch_performed": true}),
    );
    assert!(observation(&serde_json::to_vec(&authority).expect("authority mutation")).is_err());
}
