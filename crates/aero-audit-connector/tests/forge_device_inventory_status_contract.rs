//! Inventory status is a pure projection of caller-supplied values.
//!
//! This receiver checks the shared fixture and recomputes every result locally.
//! It does not read a clock, persist a heartbeat, make inventory authoritative,
//! reserve capacity, select a target, dispatch work, or grant execution authority.

#![allow(dead_code)]

use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt;

const FIXTURE: &[u8] = include_bytes!("testdata/forge-device-inventory-status-contract-v1.json");
const DEFAULT_STALE_AFTER_MS: u64 = 90_000;
const MAX_STALE_AFTER_MS: u64 = 24 * 60 * 60 * 1000;

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
struct Observation {
    approval_state: String,
    cordon_state: String,
    liveness: String,
    reservation_state: String,
    snapshot_observed_at_ms: u64,
    lease_expires_at_ms: u64,
    evaluated_at_ms: u64,
}

#[derive(Debug, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Expected {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    fresh: Option<bool>,
    #[serde(default)]
    declared_eligible: Option<bool>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    input: Observation,
    expected: Expected,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: String,
    evaluation_mode: String,
    stale_after_ms: u64,
    authority: Authority,
    cases: Vec<Case>,
}

#[derive(Debug, PartialEq, Eq)]
struct Projection {
    status: &'static str,
    fresh: bool,
    declared_eligible: bool,
}

fn decode<T: DeserializeOwned>(raw: &[u8]) -> Result<T, String> {
    reject_duplicate_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value = T::deserialize(&mut decoder).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    Ok(value)
}

fn project(value: &Observation, stale_after_ms: u64) -> Result<Projection, &'static str> {
    if value.evaluated_at_ms == 0 {
        return Err("invalid_evaluation_time");
    }
    if stale_after_ms == 0 || stale_after_ms > MAX_STALE_AFTER_MS {
        return Err("invalid_stale_after");
    }
    if value.snapshot_observed_at_ms > value.evaluated_at_ms {
        return Err("snapshot_from_future");
    }
    if value.lease_expires_at_ms < value.snapshot_observed_at_ms {
        return Err("lease_before_snapshot");
    }
    if !valid(&value.approval_state, &["approved", "pending", "revoked"]) {
        return Err("unknown_approval");
    }
    if !valid(&value.cordon_state, &["clear", "cordoned"]) {
        return Err("unknown_cordon");
    }
    if !valid(&value.liveness, &["online", "offline"]) {
        return Err("unknown_liveness");
    }
    if !valid(&value.reservation_state, &["none", "reserved"]) {
        return Err("unknown_reservation");
    }

    let age = value.evaluated_at_ms - value.snapshot_observed_at_ms;
    let fresh = age <= stale_after_ms && value.lease_expires_at_ms > value.evaluated_at_ms;
    let mut status = "online";
    if value.approval_state == "revoked" {
        status = "revoked";
    } else if value.cordon_state == "cordoned" {
        status = "cordoned";
    } else if value.liveness == "offline" {
        status = "offline";
    } else if !fresh {
        status = "stale";
    } else if value.approval_state == "pending" {
        status = "pending";
    } else if value.reservation_state == "reserved" {
        status = "reserved";
    }
    Ok(Projection {
        status,
        fresh,
        declared_eligible: status == "online",
    })
}

fn valid(value: &str, allowed: &[&str]) -> bool {
    allowed.iter().any(|candidate| value == *candidate)
}

fn validate_fixture(fixture: &Fixture) -> Result<(), String> {
    if fixture.schema_version != "forge.device-inventory-status-contract/v1"
        || fixture.evaluation_mode != "pure_projection_only"
        || fixture.stale_after_ms != DEFAULT_STALE_AFTER_MS
        || fixture.authority != Authority::default()
        || fixture.cases.len() != 11
    {
        return Err("invalid envelope".into());
    }

    let names = [
        "approved_online_is_declared_eligible",
        "pending_approval",
        "revoked_wins_over_other_states",
        "cordoned_wins_over_liveness",
        "offline_wins_over_freshness",
        "old_snapshot_is_stale",
        "expired_lease_is_stale",
        "reserved_is_not_declared_eligible",
        "future_snapshot_rejected",
        "invalid_lease_window_rejected",
        "unknown_liveness_rejected",
    ];
    let mut seen = BTreeSet::new();
    for (index, case) in fixture.cases.iter().enumerate() {
        if case.name != names[index] || !seen.insert(case.name.as_str()) {
            return Err(format!("unexpected case order at {index}"));
        }

        let result = project(&case.input, fixture.stale_after_ms);
        if let Some(error) = case.expected.error.as_deref() {
            if case.expected.status.is_some()
                || case.expected.fresh.is_some()
                || case.expected.declared_eligible.is_some()
            {
                return Err(format!("rejected case {} has accepted fields", case.name));
            }
            if result.as_ref().err().copied() != Some(error) {
                return Err(format!("case {} error mismatch", case.name));
            }
            continue;
        }

        let projection = result.map_err(|error| format!("case {}: {error}", case.name))?;
        if case.expected.status.as_deref() != Some(projection.status)
            || case.expected.fresh != Some(projection.fresh)
            || case.expected.declared_eligible != Some(projection.declared_eligible)
        {
            return Err(format!("case {} projection mismatch", case.name));
        }
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
fn canonical_inventory_status_projection_is_strict_and_authority_free() {
    let fixture: Fixture = decode(FIXTURE).expect("canonical inventory status fixture");
    validate_fixture(&fixture).expect("valid inventory status fixture");
    assert_eq!(fixture.cases[0].expected.status.as_deref(), Some("online"));
    assert_eq!(fixture.cases[5].expected.status.as_deref(), Some("stale"));
    assert_eq!(
        fixture.cases[10].expected.error.as_deref(),
        Some("unknown_liveness")
    );
}

#[test]
fn inventory_status_projection_rejects_unknown_duplicate_and_authority_mutations() {
    let mutations: [(&str, fn(&mut Value)); 9] = [
        ("unknown_root", |root| {
            root.as_object_mut()
                .expect("root object")
                .insert("unexpected".into(), true.into());
        }),
        ("unknown_input", |root| {
            root["cases"][0]["input"]["unexpected"] = true.into();
        }),
        ("identity_verified", |root| {
            root["authority"]["identity_verified"] = true.into();
        }),
        ("heartbeat_persisted", |root| {
            root["authority"]["heartbeat_persisted"] = true.into();
        }),
        ("inventory_authoritative", |root| {
            root["authority"]["inventory_authoritative"] = true.into();
        }),
        ("reservation_created", |root| {
            root["authority"]["reservation_created"] = true.into();
        }),
        ("execution_authorized", |root| {
            root["authority"]["execution_authorized"] = true.into();
        }),
        ("dispatch_performed", |root| {
            root["authority"]["dispatch_performed"] = true.into();
        }),
        ("eligible_is_not_authority", |root| {
            root["cases"][1]["expected"]["declared_eligible"] = true.into();
        }),
    ];
    for (name, mutation) in mutations {
        let mut value: Value = serde_json::from_slice(FIXTURE).expect("fixture JSON");
        mutation(&mut value);
        let decoded: Result<Fixture, _> = decode(&serde_json::to_vec(&value).expect("JSON"));
        if let Ok(fixture) = decoded {
            assert!(
                validate_fixture(&fixture).is_err(),
                "mutation {name} unexpectedly accepted"
            );
        }
    }

    let mut duplicate = String::from_utf8(FIXTURE.to_vec()).expect("fixture UTF-8");
    let end = duplicate
        .trim_end()
        .strip_suffix('}')
        .expect("root object close")
        .len();
    duplicate.truncate(end);
    duplicate.push_str(",\"schema_version\":\"forge.device-inventory-status-contract/v1\"}\n");
    assert!(decode::<Fixture>(duplicate.as_bytes()).is_err());
}
