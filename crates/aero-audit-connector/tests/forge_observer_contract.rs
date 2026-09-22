//! Forge observer evidence is a read-only, content-free compatibility seam.
//!
//! This test consumes the canonical Catalyst fixture from a standalone Aero
//! IM checkout. It proves that the audit relay's opaque payload boundary can
//! carry bounded Conversation/Run metadata without accepting content or
//! authority. It does not enqueue, publish, or authorize any Forge action.

use aero_common::model::audit::{AuditActor, AuditClaimPayload, AuditTarget};
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};
use std::fmt;

const FIXTURE: &[u8] = include_bytes!("testdata/forge-run-observed-v1.json");
const MAX_JSON_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ForgeRunObserved {
    api_version: String,
    owner_ref: String,
    conversation_id: String,
    run_id: String,
    prompt_id: String,
    created_at_ms: i64,
    latest_sequence: i64,
    status: String,
    metadata_observed: bool,
    content_included: bool,
    authority: ForgeRunObservedAuthority,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ForgeRunObservedAuthority {
    identity_verified: bool,
    owner_authorized: bool,
    run_authoritative: bool,
    persistence_attested: bool,
    content_provenance_verified: bool,
    reservation_created: bool,
    execution_authorized: bool,
    dispatch_performed: bool,
}

fn decode(raw: &[u8]) -> Result<ForgeRunObserved, String> {
    reject_duplicate_json_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value = ForgeRunObserved::deserialize(&mut decoder).map_err(|error| error.to_string())?;
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

fn fixture() -> ForgeRunObserved {
    decode(FIXTURE).expect("canonical Forge observer fixture")
}

#[test]
fn canonical_fixture_is_content_free_and_authority_free() {
    let value = fixture();
    assert_eq!(value.api_version, "forge.run.observed.v1");
    assert_eq!(value.owner_ref.len(), 64);
    assert!(value.owner_ref.bytes().all(|byte| byte.is_ascii_hexdigit()));
    for identifier in [
        &value.conversation_id,
        &value.run_id,
        &value.prompt_id,
        &value.status,
    ] {
        assert!(!identifier.is_empty());
        assert!(!identifier.bytes().any(|byte| byte.is_ascii_whitespace()));
    }
    assert!((0..=MAX_JSON_SAFE_INTEGER).contains(&value.created_at_ms));
    assert!((1..=MAX_JSON_SAFE_INTEGER).contains(&value.latest_sequence));
    assert!(value.metadata_observed);
    assert!(!value.content_included);
    assert_eq!(value.authority, ForgeRunObservedAuthority::default());
}

#[test]
fn unknown_content_and_duplicate_keys_fail_closed() {
    let mut shape: Map<String, Value> = serde_json::from_slice(FIXTURE).expect("object fixture");
    shape.insert("prompt".into(), Value::String("raw prompt".into()));
    let unknown = serde_json::to_vec(&shape).expect("unknown mutation");
    assert!(decode(&unknown).is_err());

    let root_duplicate = format!(
        "{},\"api_version\":\"forge.run.observed.v1\"}}",
        String::from_utf8_lossy(FIXTURE).trim_end_matches('}')
    );
    assert!(decode(root_duplicate.as_bytes()).is_err());

    let nested_duplicate = String::from_utf8_lossy(FIXTURE).replace(
        "    \"dispatch_performed\": false\n  }",
        "    \"dispatch_performed\": false,\n    \"dispatch_performed\": false\n  }",
    );
    assert_ne!(nested_duplicate.as_bytes(), FIXTURE);
    assert!(decode(nested_duplicate.as_bytes()).is_err());
}

#[test]
fn observer_metadata_can_cross_aero_audit_payload_without_content() {
    let value = fixture();
    let payload: Value = serde_json::from_slice(FIXTURE).expect("observer payload");
    let event = AuditClaimPayload::new(
        "forge-observer-event-001".into(),
        "forge-runtime".into(),
        "2026-09-16T12:00:00Z".into(),
        AuditActor::participant(value.owner_ref),
        vec![AuditTarget::resource(value.run_id.clone())],
        value.run_id,
        "forge.run.observed".into(),
        payload,
    );
    let encoded = serde_json::to_value(event).expect("audit payload serialization");
    assert_eq!(encoded["payload"]["content_included"], Value::Bool(false));
    for forbidden in ["prompt", "result", "output", "tool", "token", "artifact"] {
        assert!(
            encoded["payload"].get(forbidden).is_none(),
            "{forbidden} crossed observer boundary"
        );
    }
}
