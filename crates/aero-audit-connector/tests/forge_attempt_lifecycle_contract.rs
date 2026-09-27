//! Strict Aero-IM receiver for the authority-neutral Attempt lifecycle graph.
//!
//! This test consumes display-only lifecycle metadata. It never persists an
//! Attempt, changes a lease, opens Runner transport, or grants execution
//! authority.

use serde::Deserialize;
use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use std::{collections::BTreeSet, fmt};

const FIXTURE: &[u8] = include_bytes!("testdata/forge-attempt-lifecycle-v1.json");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: String,
    evaluation_mode: String,
    authority: Authority,
    cases: Vec<Case>,
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
struct Case {
    name: String,
    from: String,
    to: String,
    accepted: bool,
    #[serde(default)]
    error: Option<String>,
}

#[test]
fn attempt_lifecycle_receiver() {
    let value = decode(FIXTURE).expect("canonical Attempt lifecycle fixture");
    validate(&value).expect("canonical Attempt lifecycle");
    assert_eq!(value.schema_version, "forge.attempt-lifecycle/v1");
    assert_eq!(value.evaluation_mode, "pure_attempt_lifecycle_only");
    assert_eq!(value.cases.len(), 19);

    let source = String::from_utf8_lossy(FIXTURE);
    let compact = source.trim();
    let unknown = format!(r#"{},"unexpected":true}}"#, compact.trim_end_matches('}'));
    assert!(decode(unknown.as_bytes()).is_err());
    let duplicate = format!(
        r#"{},"schema_version":"forge.attempt-lifecycle/v1"}}"#,
        compact.trim_end_matches('}')
    );
    assert!(decode(duplicate.as_bytes()).is_err());
    assert!(decode(format!("{compact} {{}}").as_bytes()).is_err());

    let mut authority: Value = serde_json::from_slice(FIXTURE).expect("fixture value");
    authority["authority"]["dispatch_performed"] = Value::Bool(true);
    let authority = serde_json::to_vec(&authority).expect("authority mutation");
    let value = decode(&authority).expect("authority mutation remains wire-valid");
    assert!(validate(&value).is_err());

    let mut acceptance: Value = serde_json::from_slice(FIXTURE).expect("fixture value");
    acceptance["cases"][0]["accepted"] = Value::Bool(false);
    let acceptance = serde_json::to_vec(&acceptance).expect("acceptance mutation");
    let value = decode(&acceptance).expect("acceptance mutation remains wire-valid");
    assert!(validate(&value).is_err());
}

fn decode(raw: &[u8]) -> Result<Fixture, String> {
    reject_duplicate_json_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value = Fixture::deserialize(&mut decoder).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    Ok(value)
}

fn validate(value: &Fixture) -> Result<(), String> {
    if value.schema_version != "forge.attempt-lifecycle/v1"
        || value.evaluation_mode != "pure_attempt_lifecycle_only"
        || value.authority != Authority::default()
        || value.cases.len() != 19
    {
        return Err("invalid Attempt lifecycle envelope".into());
    }

    let mut names = BTreeSet::new();
    for case in &value.cases {
        if case.name.is_empty() || !names.insert(&case.name) {
            return Err("duplicate or empty Attempt lifecycle case name".into());
        }
        let (accepted, rejection) = edge(&case.from, &case.to);
        if case.accepted != accepted {
            return Err("Attempt lifecycle acceptance drift".into());
        }
        if accepted {
            if case.error.is_some() {
                return Err("accepted Attempt lifecycle edge carries rejection".into());
            }
        } else if case.error.as_deref() != Some(rejection) {
            return Err("Attempt lifecycle rejection drift".into());
        }
    }
    Ok(())
}

fn edge(from: &str, to: &str) -> (bool, &'static str) {
    if !valid_state(from) || !valid_state(to) {
        return (false, "pc_state_invalid");
    }
    let accepted = match from {
        "requested" => to == "accepted",
        "accepted" => matches!(to, "starting" | "interrupted" | "failed" | "uncertain"),
        "starting" => matches!(to, "running" | "interrupted" | "failed" | "uncertain"),
        "running" => matches!(to, "interrupted" | "completed" | "failed" | "uncertain"),
        _ => false,
    };
    (accepted, "pc_transition_invalid")
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
