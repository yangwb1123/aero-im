//! Forge execution evidence is a read-only, content-free compatibility seam.
//!
//! This test consumes the canonical Catalyst fixture from a standalone Aero
//! IM checkout. It proves that bounded Run/attempt/receipt metadata can cross
//! the typed audit payload boundary without becoming content or authority.
//! It does not enqueue, publish, or authorize any Forge action.

use aero_common::model::audit::{AuditActor, AuditClaimPayload, AuditTarget};
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};
use std::fmt;

const FIXTURE: &[u8] = include_bytes!("testdata/forge-run-execution-evidence-v1.json");
const MAX_JSON_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ForgeRunExecutionEvidence {
    api_version: String,
    evaluation_mode: String,
    owner_ref: String,
    conversation_id: String,
    run_id: String,
    prompt_id: String,
    run_status: String,
    attempt_id: String,
    target_id: String,
    command_id: String,
    command_sha256: String,
    disposition_kind: String,
    receipt_observed_at_ms: i64,
    uncertain: bool,
    reconciliation_required: bool,
    metadata_observed: bool,
    content_included: bool,
    authority: ForgeRunExecutionEvidenceAuthority,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ForgeRunExecutionEvidenceAuthority {
    identity_verified: bool,
    owner_authorized: bool,
    run_authoritative: bool,
    receipt_persisted: bool,
    reservation_created: bool,
    execution_authorized: bool,
    dispatch_performed: bool,
    audit_published: bool,
}

fn decode(raw: &[u8]) -> Result<ForgeRunExecutionEvidence, String> {
    reject_duplicate_json_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value =
        ForgeRunExecutionEvidence::deserialize(&mut decoder).map_err(|error| error.to_string())?;
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

fn fixture() -> ForgeRunExecutionEvidence {
    decode(FIXTURE).expect("canonical Forge execution evidence fixture")
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
    assert_eq!(value.api_version, "forge.run.execution-evidence.v1");
    assert_eq!(value.evaluation_mode, "pure_run_execution_evidence_binding");
    assert!(is_lower_hex_256(&value.owner_ref));
    assert!(is_lower_hex_256(&value.command_sha256));
    for identifier in [
        &value.conversation_id,
        &value.run_id,
        &value.prompt_id,
        &value.run_status,
        &value.attempt_id,
        &value.target_id,
        &value.command_id,
        &value.disposition_kind,
    ] {
        assert!(!identifier.is_empty());
        assert!(!identifier.bytes().any(|byte| byte.is_ascii_whitespace()));
    }
    assert!((0..=MAX_JSON_SAFE_INTEGER).contains(&value.receipt_observed_at_ms));
    assert_eq!(value.disposition_kind, "completed");
    assert!(!value.uncertain);
    assert!(!value.reconciliation_required);
    assert!(value.metadata_observed);
    assert!(!value.content_included);
    assert_eq!(
        value.authority,
        ForgeRunExecutionEvidenceAuthority::default()
    );
}

#[test]
fn unknown_content_and_duplicate_keys_fail_closed() {
    let mut shape: Map<String, Value> = serde_json::from_slice(FIXTURE).expect("object fixture");
    shape.insert("output".into(), Value::String("raw output".into()));
    let unknown = serde_json::to_vec(&shape).expect("unknown mutation");
    assert!(decode(&unknown).is_err());

    let root_duplicate = format!(
        "{},\"run_id\":\"run-001\"}}",
        String::from_utf8_lossy(FIXTURE).trim_end_matches('}')
    );
    assert!(decode(root_duplicate.as_bytes()).is_err());

    let nested_duplicate = String::from_utf8_lossy(FIXTURE).replace(
        "    \"audit_published\": false\n  }",
        "    \"audit_published\": false,\n    \"audit_published\": false\n  }",
    );
    assert_ne!(nested_duplicate.as_bytes(), FIXTURE);
    assert!(decode(nested_duplicate.as_bytes()).is_err());
}

#[test]
fn execution_evidence_can_cross_aero_audit_payload_without_content() {
    let value = fixture();
    let payload: Value = serde_json::from_slice(FIXTURE).expect("execution evidence payload");
    let event = AuditClaimPayload::new(
        "forge-execution-evidence-event-001".into(),
        "forge-runtime".into(),
        "2026-09-17T12:00:00Z".into(),
        AuditActor::participant(value.owner_ref),
        vec![AuditTarget::resource(value.run_id.clone())],
        value.run_id,
        "forge.run.execution-evidence".into(),
        payload,
    );
    let encoded = serde_json::to_value(event).expect("audit payload serialization");
    assert_eq!(encoded["payload"]["content_included"], Value::Bool(false));
    assert_eq!(
        encoded["payload"]["authority"]["audit_published"],
        Value::Bool(false)
    );
    for forbidden in ["prompt", "result", "output", "tool", "token", "artifact"] {
        assert!(
            encoded["payload"].get(forbidden).is_none(),
            "{forbidden} crossed execution evidence boundary"
        );
    }
}
