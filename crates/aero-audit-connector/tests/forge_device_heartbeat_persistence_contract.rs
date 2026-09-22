//! Forge heartbeat persistence is consumed as a bounded compare-and-swap
//! observation only. This receiver does not persist a heartbeat, authenticate
//! a device, make inventory authoritative, reserve capacity, dispatch work,
//! or publish Audit authority.

use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::collections::BTreeSet;
use std::fmt;

const FIXTURE: &[u8] =
    include_bytes!("testdata/forge-device-heartbeat-persistence-contract-v1.json");

#[derive(Debug, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Authority {
    identity_verified: bool,
    heartbeat_persisted: bool,
    inventory_authoritative: bool,
    reservation_created: bool,
    execution_authorized: bool,
    dispatch_performed: bool,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Device {
    device_id: String,
    tenant_id: String,
    approval_state: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Heartbeat {
    device_id: String,
    instance_id: String,
    generation: u64,
    sequence: u64,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct State {
    revision: u64,
    device_id: String,
    instance_id: String,
    generation: u64,
    heartbeat_sequence: u64,
    server_observed_at_ms: u64,
    capability_lease_expires_at_ms: u64,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Expected {
    accepted: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    revision: Option<u64>,
    #[serde(default)]
    generation: Option<u64>,
    #[serde(default)]
    heartbeat_sequence: Option<u64>,
    #[serde(default)]
    server_observed_at_ms: Option<u64>,
    #[serde(default)]
    capability_lease_expires_at_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    expected_revision: u64,
    #[serde(default)]
    device_approval_state: Option<String>,
    current: Option<State>,
    heartbeat: Heartbeat,
    server_observed_at_ms: u64,
    lease_ttl_ms: u64,
    expected: Expected,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: String,
    evaluation_mode: String,
    authority: Authority,
    device: Device,
    cases: Vec<Case>,
}

fn decode<T: DeserializeOwned>(raw: &[u8]) -> Result<T, String> {
    reject_duplicate_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value = T::deserialize(&mut decoder).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    Ok(value)
}

fn validate(fixture: &Fixture) -> Result<(), &'static str> {
    if fixture.schema_version != "forge.device-heartbeat-persistence-contract/v1"
        || fixture.evaluation_mode != "pure_compare_and_swap_plan"
        || fixture.authority != Authority::default()
        || fixture.device
            != (Device {
                device_id: "device-a".into(),
                tenant_id: "tenant-1".into(),
                approval_state: "approved".into(),
            })
        || fixture.cases.len() != 10
    {
        return Err("envelope");
    }
    let mut names = BTreeSet::new();
    for case in &fixture.cases {
        if case.name.is_empty() || !names.insert(case.name.clone()) {
            return Err("case name");
        }
        if case.heartbeat.device_id.is_empty()
            || case.heartbeat.instance_id.is_empty()
            || case.heartbeat.generation == 0
            || case.heartbeat.sequence == 0
            || case.server_observed_at_ms == 0
            || case.lease_ttl_ms == 0
        {
            return Err("heartbeat input");
        }
        if let Some(approval) = case.device_approval_state.as_deref() {
            if !matches!(approval, "approved" | "revoked") {
                return Err("approval state");
            }
        }
        if let Some(current) = &case.current {
            if (current.revision == 0 && case.name != "zero_persisted_revision_rejected")
                || current.device_id.is_empty()
                || current.instance_id.is_empty()
                || current.generation == 0
                || current.heartbeat_sequence == 0
                || current.capability_lease_expires_at_ms <= current.server_observed_at_ms
            {
                return Err("persisted state");
            }
        }
        if case.expected.accepted {
            if case.expected.error.is_some()
                || case.expected.revision != Some(case.expected_revision + 1)
                || case.expected.generation != Some(case.heartbeat.generation)
                || case.expected.heartbeat_sequence != Some(case.heartbeat.sequence)
                || case.expected.server_observed_at_ms != Some(case.server_observed_at_ms)
                || case.expected.capability_lease_expires_at_ms
                    != Some(case.server_observed_at_ms + case.lease_ttl_ms)
            {
                return Err("accepted expectation");
            }
        } else if case.expected.error.is_none() || case.expected.revision.is_some() {
            return Err("rejected expectation");
        }
    }
    if fixture.cases[0].name != "initial_insert"
        || !fixture.cases[0].expected.accepted
        || fixture.cases[0].current.is_some()
        || fixture.cases[0].expected.revision != Some(1)
        || fixture.cases[1].name != "next_sequence_same_revision_chain"
        || !fixture.cases[1].expected.accepted
        || fixture.cases[1].expected.revision != Some(2)
    {
        return Err("CAS chain");
    }
    Ok(())
}

fn reject_duplicate_keys(raw: &[u8]) -> Result<(), String> {
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

#[test]
fn canonical_heartbeat_persistence_is_bounded_and_authority_free() {
    let fixture: Fixture = decode(FIXTURE).expect("canonical heartbeat persistence fixture");
    validate(&fixture).expect("heartbeat persistence fixture is valid");
    assert_eq!(fixture.cases[0].heartbeat.instance_id, "runner-a");
    assert_eq!(fixture.cases[1].expected.revision, Some(2));
}

#[test]
fn heartbeat_persistence_rejects_unknown_duplicate_authority_and_foreign_device_mutations() {
    let original: serde_json::Value = serde_json::from_slice(FIXTURE).unwrap();
    fn unknown(root: &mut serde_json::Value) {
        root.as_object_mut()
            .unwrap()
            .insert("unexpected".into(), true.into());
    }
    fn authority(root: &mut serde_json::Value) {
        root["authority"]["heartbeat_persisted"] = true.into();
    }
    fn foreign_device(root: &mut serde_json::Value) {
        root["device"]["device_id"] = "device-foreign".into();
    }
    for (name, mutation) in [
        ("unknown", unknown as fn(&mut serde_json::Value)),
        ("authority", authority as fn(&mut serde_json::Value)),
        (
            "foreign_device",
            foreign_device as fn(&mut serde_json::Value),
        ),
    ] {
        let mut value = original.clone();
        mutation(&mut value);
        let bytes = serde_json::to_vec(&value).unwrap();
        let decoded: Result<Fixture, _> = decode(&bytes);
        assert!(
            decoded
                .and_then(|fixture| validate(&fixture).map_err(str::to_owned))
                .is_err(),
            "mutation {name} accepted"
        );
    }
    let mut duplicate = String::from_utf8(FIXTURE.to_vec()).unwrap();
    duplicate = duplicate.trim().trim_end_matches('}').to_owned();
    duplicate.push_str(",\"schema_version\":\"forge.device-heartbeat-persistence-contract/v1\"}");
    let decoded: Result<Fixture, _> = decode(duplicate.as_bytes());
    assert!(decoded.is_err(), "duplicate field accepted");
}
