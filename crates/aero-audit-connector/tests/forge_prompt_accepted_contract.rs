//! Forge's accepted-Prompt audit envelope is a read-only compatibility seam.
//!
//! This test consumes Catalyst's canonical minimized event and proves that it
//! can cross Aero-IM's typed audit payload boundary without carrying Prompt
//! content or execution authority. It does not enqueue, publish, or authorize
//! Forge work.

use aero_common::model::audit::{AuditActor, AuditClaimPayload, AuditTarget};
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};
use std::fmt;

const FIXTURE: &[u8] = include_bytes!("testdata/forge-prompt-accepted-audit-v1.json");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ForgePromptAccepted {
    event_id: String,
    tenant_id: String,
    source_system: String,
    event_type: String,
    schema_id: String,
    schema_version: i64,
    occurred_at: String,
    operation_id: String,
    actor: ForgePromptAcceptedActor,
    aggregate_type: String,
    aggregate_id: String,
    aggregate_version: i64,
    action: String,
    outcome: String,
    payload: ForgePromptAcceptedPayload,
    data_classification: String,
    retention_class: String,
    idempotency_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ForgePromptAcceptedActor {
    id: String,
    r#type: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ForgePromptAcceptedPayload {
    content_included: bool,
}

fn decode(raw: &[u8]) -> Result<ForgePromptAccepted, String> {
    reject_duplicate_json_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value =
        ForgePromptAccepted::deserialize(&mut decoder).map_err(|error| error.to_string())?;
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

impl<'de> DeserializeSeed<'de> for ScanSeed {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(ScanVisitor)
    }
}

struct ScanVisitor;

impl<'de> Visitor<'de> for ScanVisitor {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut keys = std::collections::BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(de::Error::custom(format!(
                    "duplicate JSON object key {key:?}"
                )));
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

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(())
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(())
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(())
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(())
    }

    fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(())
    }

    fn visit_string<E>(self, _value: String) -> Result<Self::Value, E>
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

fn fixture() -> ForgePromptAccepted {
    decode(FIXTURE).expect("canonical Forge accepted-Prompt fixture")
}

fn is_lower_hex_256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[test]
fn canonical_fixture_is_content_free_and_authority_free() {
    let value = fixture();
    assert_eq!(value.source_system, "forge-runtime");
    assert_eq!(value.event_type, "forge.prompt.accepted.v1");
    assert_eq!(value.schema_id, value.event_type);
    assert_eq!(value.schema_version, 1);
    assert_eq!(value.aggregate_type, "forge_conversation");
    assert_eq!(value.action, "append_prompt");
    assert_eq!(value.outcome, "accepted");
    assert_eq!(value.actor.r#type, "user");
    assert!(!value.payload.content_included);
    assert!(is_lower_hex_256(&value.event_id));
    assert!(is_lower_hex_256(&value.actor.id));
    assert_eq!(value.idempotency_key, value.event_id);
    assert!(!value.occurred_at.is_empty());
    assert!(!value.tenant_id.is_empty());
    assert!(!value.operation_id.is_empty());
    assert!(!value.aggregate_id.is_empty());
    assert!(value.aggregate_version > 0);
    assert_eq!(value.data_classification, "internal");
    assert_eq!(value.retention_class, "standard");
}

#[test]
fn unknown_content_and_duplicate_keys_fail_closed() {
    let mut shape: Map<String, Value> = serde_json::from_slice(FIXTURE).expect("object fixture");
    shape.insert("prompt".into(), Value::String("raw prompt".into()));
    let unknown = serde_json::to_vec(&shape).expect("unknown mutation");
    assert!(decode(&unknown).is_err());

    let root_duplicate = format!(
        "{},\"event_id\":\"different\"}}",
        String::from_utf8_lossy(FIXTURE).trim_end_matches('}')
    );
    assert!(decode(root_duplicate.as_bytes()).is_err());

    let nested_duplicate = String::from_utf8_lossy(FIXTURE).replace(
        "    \"content_included\": false\n  }",
        "    \"content_included\": false,\n    \"content_included\": false\n  }",
    );
    assert_ne!(nested_duplicate.as_bytes(), FIXTURE);
    assert!(decode(nested_duplicate.as_bytes()).is_err());
}

#[test]
fn accepted_prompt_can_cross_aero_audit_payload_without_content() {
    let value = fixture();
    let payload: Value = serde_json::from_slice(FIXTURE).expect("accepted-Prompt payload");
    let event = AuditClaimPayload::new(
        "forge-prompt-accepted-event-001".into(),
        "forge-runtime".into(),
        "2026-09-13T12:00:00Z".into(),
        AuditActor::participant(value.actor.id),
        vec![AuditTarget::resource(value.aggregate_id.clone())],
        value.aggregate_id,
        "forge.prompt.accepted".into(),
        payload,
    );
    let encoded = serde_json::to_value(event).expect("audit payload serialization");
    assert_eq!(
        encoded["payload"]["payload"]["content_included"],
        Value::Bool(false)
    );
    for forbidden in [
        "prompt",
        "output",
        "result",
        "token",
        "credential",
        "artifact",
    ] {
        assert!(
            encoded["payload"]["payload"].get(forbidden).is_none(),
            "{forbidden} crossed accepted-Prompt boundary"
        );
    }
}
