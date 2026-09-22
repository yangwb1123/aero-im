//! Forge Runner lease/fencing is consumed as bounded evidence only.
//!
//! This receiver does not authenticate a Runner, persist a lease, reserve a
//! device, dispatch work, or grant execution/Audit authority.

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer as _};
use serde_json::{Map, Value};
use std::fmt;

const FIXTURE: &[u8] = include_bytes!("testdata/forge-runner-lease-fencing-v1.json");

#[derive(Debug, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Authority {
    device_identity_verified: bool,
    command_persisted: bool,
    reservation_created: bool,
    execution_authorized: bool,
    dispatch_performed: bool,
    audit_published: bool,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Grant {
    v: u16,
    attempt_id: String,
    target_id: String,
    epoch: u64,
    fencing_token: String,
    issued_at_ms: u64,
    expires_at_ms: u64,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    attempt_id: String,
    target_id: String,
    epoch: u64,
    fencing_token: String,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Disposition {
    kind: String,
    #[serde(default)]
    receipt_sha256: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Expected {
    #[serde(default)]
    active: Option<bool>,
    #[serde(default)]
    accepted: Option<bool>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    epoch: Option<u64>,
    #[serde(default)]
    issued_at_ms: Option<u64>,
    #[serde(default)]
    expires_at_ms: Option<u64>,
    #[serde(default)]
    replayed: Option<bool>,
    #[serde(default)]
    uncertain: Option<bool>,
    #[serde(default)]
    automatic_retry: Option<bool>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    operation: String,
    #[serde(default)]
    observed_at_ms: Option<u64>,
    #[serde(default)]
    fencing_token: Option<String>,
    #[serde(default)]
    ttl_ms: Option<u64>,
    #[serde(default)]
    proof: Option<Proof>,
    #[serde(default)]
    disposition: Option<Disposition>,
    #[serde(default)]
    seed_disposition: Option<Disposition>,
    #[serde(default)]
    seed_observed_at_ms: Option<u64>,
    expected: Expected,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: String,
    evaluation_mode: String,
    authority: Authority,
    grant: Grant,
    cases: Vec<Case>,
}

fn decode(raw: &[u8]) -> Result<Fixture, String> {
    reject_duplicate_json_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value = Fixture::deserialize(&mut decoder).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    if value.authority != Authority::default() {
        return Err("lease authority is not disabled".into());
    }
    Ok(value)
}

#[test]
fn canonical_fixture_is_bounded_and_authority_free() {
    let value = decode(FIXTURE).expect("canonical Runner lease/fencing fixture");
    assert_eq!(value.schema_version, "forge.runner-lease-fencing/v1");
    assert_eq!(value.evaluation_mode, "pure_lease_fencing_only");
    assert_eq!(value.authority, Authority::default());
    assert_eq!(value.grant.v, 1);
    assert!(!value.grant.attempt_id.is_empty());
    assert!(!value.grant.target_id.is_empty());
    assert!(value.grant.epoch > 0);
    assert!(!value.grant.fencing_token.is_empty());
    assert!(value.grant.expires_at_ms > value.grant.issued_at_ms);
    assert_eq!(value.cases.len(), 16);
    let names: std::collections::BTreeSet<_> =
        value.cases.iter().map(|case| case.name.as_str()).collect();
    for name in [
        "active_at_issue",
        "inactive_at_expiry",
        "renew_valid",
        "renew_token_reused",
        "renew_clock_rollback",
        "renew_expired",
        "proof_attempt_mismatch",
        "proof_target_mismatch",
        "proof_epoch_mismatch",
        "proof_token_mismatch",
        "proof_expired",
        "terminal_completed",
        "terminal_uncertain",
        "terminal_replay",
        "terminal_conflict",
        "uncertain_is_terminal",
    ] {
        assert!(names.contains(name), "missing lease case {name}");
    }
    assert!(value
        .cases
        .iter()
        .all(|case| !case.name.is_empty() && !case.operation.is_empty()));
}

#[test]
fn unknown_authority_and_duplicate_fields_fail_closed() {
    let mut shape: Map<String, Value> = serde_json::from_slice(FIXTURE).expect("object fixture");
    shape.insert("output".into(), Value::String("raw output".into()));
    let unknown = serde_json::to_vec(&shape).expect("unknown mutation");
    assert!(decode(&unknown).is_err());

    let mut authority: Map<String, Value> =
        serde_json::from_slice(FIXTURE).expect("object fixture");
    authority["authority"]["audit_published"] = Value::Bool(true);
    let mutated = serde_json::to_vec(&authority).expect("authority mutation");
    assert!(decode(&mutated).is_err());

    let root_duplicate = format!(
        "{},\"schema_version\":\"forge.runner-lease-fencing/v1\"}}",
        String::from_utf8_lossy(FIXTURE).trim_end_matches('}')
    );
    assert!(decode(root_duplicate.as_bytes()).is_err());

    let nested_duplicate = String::from_utf8_lossy(FIXTURE).replace(
        "    \"audit_published\": false\n  },",
        "    \"audit_published\": false,\n    \"audit_published\": false\n  },",
    );
    assert_ne!(nested_duplicate.as_bytes(), FIXTURE);
    assert!(decode(nested_duplicate.as_bytes()).is_err());
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
