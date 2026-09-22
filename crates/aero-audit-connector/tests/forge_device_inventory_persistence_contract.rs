//! The persisted inventory image is a bounded CAS/projection contract only.
//! It does not persist a device row, authenticate a Runner, make inventory
//! authoritative, reserve capacity, dispatch work, or publish Audit claims.

#![allow(dead_code)]

use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt;

const FIXTURE: &[u8] = include_bytes!("testdata/forge-device-inventory-persistence-v1.json");

#[derive(Debug, Deserialize, PartialEq, Eq, Clone)]
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
    heartbeat_persisted: bool,
    inventory_authoritative: bool,
    reservation_created: bool,
    execution_authorized: bool,
    dispatch_performed: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Capabilities {
    os: String,
    architecture: String,
    cpu_cores: u64,
    available_cpu_cores: u64,
    memory_bytes: u64,
    available_memory_bytes: u64,
    storage_bytes: u64,
    available_storage_bytes: u64,
    gpus: Vec<Value>,
    runtimes: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Device {
    device_id: String,
    owner: Owner,
    approval_state: String,
    cordon_state: String,
    reservation_state: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Runner {
    device_id: String,
    instance_id: String,
    generation: u64,
    heartbeat_sequence: u64,
    server_observed_at_ms: u64,
    capability_lease_expires_at_ms: u64,
    liveness: String,
    capabilities: Capabilities,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    revision: u64,
    device: Device,
    runner: Runner,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Replacement {
    heartbeat_sequence: u64,
    server_observed_at_ms: u64,
    capability_lease_expires_at_ms: u64,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Expected {
    accepted: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    revision: Option<u64>,
    #[serde(default)]
    device_id: Option<String>,
    #[serde(default)]
    instance_id: Option<String>,
    #[serde(default)]
    heartbeat_sequence: Option<u64>,
    #[serde(default)]
    server_observed_at_ms: Option<u64>,
    #[serde(default)]
    capability_lease_expires_at_ms: Option<u64>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    fresh: Option<bool>,
    #[serde(default)]
    declared_eligible: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    operation: String,
    #[serde(default)]
    evaluation_owner: Option<String>,
    #[serde(default)]
    evaluated_at_ms: Option<u64>,
    #[serde(default)]
    expected_revision: Option<u64>,
    #[serde(default)]
    state_revision: Option<u64>,
    #[serde(default)]
    runner_device_id: Option<String>,
    #[serde(default)]
    device_approval_state: Option<String>,
    #[serde(default)]
    device_cordon_state: Option<String>,
    #[serde(default)]
    runner_liveness: Option<String>,
    #[serde(default)]
    replacement: Replacement,
    expected: Expected,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: String,
    evaluation_mode: String,
    stale_after_ms: u64,
    authority: Authority,
    state: State,
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
    if fixture.schema_version != "forge.device-inventory-persistence/v1"
        || fixture.evaluation_mode != "pure_persisted_inventory_cas_projection"
        || fixture.stale_after_ms != 90_000
        || fixture.authority != Authority::default()
        || fixture.state.revision != 3
        || fixture.cases.len() != 12
    {
        return Err("envelope");
    }
    let owner = Owner {
        issuer: "issuer".into(),
        subject: "user".into(),
        tenant_id: "tenant".into(),
    };
    let state = &fixture.state;
    if state.device.device_id != "device-a"
        || state.device.owner != owner
        || state.device.approval_state != "approved"
        || state.device.cordon_state != "clear"
        || state.device.reservation_state != "none"
        || state.runner.device_id != "device-a"
        || state.runner.instance_id != "runner-a"
        || state.runner.generation != 1
        || state.runner.heartbeat_sequence != 1
        || state.runner.server_observed_at_ms != 1000
        || state.runner.capability_lease_expires_at_ms != 5000
        || state.runner.liveness != "online"
        || state.runner.capabilities.os != "linux"
        || state.runner.capabilities.architecture != "amd64"
        || state.runner.capabilities.cpu_cores != 8
        || state.runner.capabilities.available_cpu_cores != 7
        || state.runner.capabilities.memory_bytes != 17_179_869_184
        || state.runner.capabilities.available_memory_bytes != 8_589_934_592
        || state.runner.capabilities.storage_bytes != 107_374_182_400
        || state.runner.capabilities.available_storage_bytes != 53_687_091_200
        || !state.runner.capabilities.gpus.is_empty()
        || state.runner.capabilities.runtimes != ["go", "rust"]
    {
        return Err("state");
    }
    let names = [
        "project_online",
        "commit_replacement",
        "revision_conflict",
        "runner_device_mismatch",
        "owner_mismatch",
        "invalid_evaluation_owner",
        "stale_projection",
        "pending_projection",
        "cordoned_projection",
        "offline_projection",
        "revoked_projection",
        "revision_overflow",
    ];
    let mut seen = BTreeSet::new();
    for (index, case) in fixture.cases.iter().enumerate() {
        if case.name != names[index] || !seen.insert(case.name.clone()) {
            return Err("case order");
        }
        if case.expected.accepted {
            if case.expected.error.is_some() {
                return Err("accepted error");
            }
        } else if case.expected.error.is_none()
            || case.expected.revision.is_some()
            || case.expected.device_id.is_some()
            || case.expected.instance_id.is_some()
            || case.expected.heartbeat_sequence.is_some()
            || case.expected.server_observed_at_ms.is_some()
            || case.expected.capability_lease_expires_at_ms.is_some()
            || case.expected.status.is_some()
            || case.expected.fresh.is_some()
            || case.expected.declared_eligible.is_some()
        {
            return Err("rejected expectation");
        }
        match case.name.as_str() {
            "project_online" => {
                validate_projection_case(case, 1500, "online", true, true, None, None, None)?;
            }
            "stale_projection" => {
                validate_projection_case(case, 100001, "stale", false, false, None, None, None)?;
            }
            "pending_projection" => {
                validate_projection_case(
                    case,
                    1500,
                    "pending",
                    true,
                    false,
                    Some("pending"),
                    None,
                    None,
                )?;
            }
            "cordoned_projection" => {
                validate_projection_case(
                    case,
                    1500,
                    "cordoned",
                    true,
                    false,
                    None,
                    Some("cordoned"),
                    None,
                )?;
            }
            "offline_projection" => {
                validate_projection_case(
                    case,
                    1500,
                    "offline",
                    true,
                    false,
                    None,
                    None,
                    Some("offline"),
                )?;
            }
            "revoked_projection" => {
                validate_projection_case(
                    case,
                    1500,
                    "revoked",
                    true,
                    false,
                    Some("revoked"),
                    None,
                    None,
                )?;
            }
            "commit_replacement" => {
                if case.operation != "commit"
                    || case.expected_revision != Some(3)
                    || !case.expected.accepted
                    || case.expected.revision != Some(4)
                    || case.replacement.heartbeat_sequence != 2
                    || case.replacement.server_observed_at_ms != 2000
                    || case.replacement.capability_lease_expires_at_ms != 6000
                    || case.expected.heartbeat_sequence != Some(2)
                    || case.expected.server_observed_at_ms != Some(2000)
                    || case.expected.capability_lease_expires_at_ms != Some(6000)
                    || case.expected.device_id.is_some()
                    || case.expected.instance_id.is_some()
                    || case.expected.status.is_some()
                    || case.expected.fresh.is_some()
                    || case.expected.declared_eligible.is_some()
                {
                    return Err("replacement case");
                }
            }
            "revision_conflict" => {
                if case.operation != "commit"
                    || case.expected_revision != Some(2)
                    || case.expected.error.as_deref() != Some("revision_conflict")
                {
                    return Err("conflict case");
                }
            }
            "runner_device_mismatch" => {
                if case.operation != "restore"
                    || case.runner_device_id.as_deref() != Some("device-b")
                    || case.expected.error.as_deref() != Some("runner_device_mismatch")
                {
                    return Err("runner mismatch case");
                }
            }
            "owner_mismatch" => {
                if case.operation != "project"
                    || case.evaluation_owner.as_deref() != Some("foreign")
                    || case.expected.error.as_deref() != Some("owner_mismatch")
                {
                    return Err("owner case");
                }
            }
            "invalid_evaluation_owner" => {
                if case.operation != "project"
                    || case.evaluation_owner.as_deref() != Some("invalid")
                    || case.expected.error.as_deref() != Some("invalid_evaluation_owner")
                {
                    return Err("evaluation owner case");
                }
            }
            "revision_overflow" => {
                if case.operation != "commit"
                    || case.state_revision != Some(u64::MAX)
                    || case.expected_revision != Some(u64::MAX)
                    || case.expected.error.as_deref() != Some("revision_overflow")
                {
                    return Err("overflow case");
                }
            }
            _ => return Err("unknown case"),
        }
    }
    Ok(())
}

fn validate_projection_case(
    case: &Case,
    evaluated_at: u64,
    status: &str,
    fresh: bool,
    eligible: bool,
    approval: Option<&str>,
    cordon: Option<&str>,
    liveness: Option<&str>,
) -> Result<(), &'static str> {
    if case.operation != "project"
        || case.evaluation_owner.as_deref() != Some("same")
        || case.evaluated_at_ms != Some(evaluated_at)
        || !case.expected.accepted
        || case.expected.revision != Some(3)
        || case.expected.device_id.as_deref() != Some("device-a")
        || case.expected.instance_id.as_deref() != Some("runner-a")
        || case.expected.status.as_deref() != Some(status)
        || case.expected.fresh != Some(fresh)
        || case.expected.declared_eligible != Some(eligible)
        || case.expected.heartbeat_sequence.is_some()
        || case.expected.server_observed_at_ms.is_some()
        || case.expected.capability_lease_expires_at_ms.is_some()
        || case.device_approval_state.as_deref() != approval
        || case.device_cordon_state.as_deref() != cordon
        || case.runner_liveness.as_deref() != liveness
    {
        return Err("projection case");
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
fn canonical_inventory_persistence_is_bounded_and_authority_free() {
    let fixture: Fixture = decode(FIXTURE).expect("inventory persistence fixture");
    validate(&fixture).expect("valid inventory persistence fixture");
    assert_eq!(fixture.cases[0].expected.status.as_deref(), Some("online"));
    assert_eq!(fixture.cases[6].expected.status.as_deref(), Some("stale"));
}

#[test]
fn inventory_persistence_rejects_unknown_duplicate_binding_revision_and_authority_mutations() {
    let mutations: [(&str, fn(&mut Value)); 5] = [
        ("unknown", |root| {
            root.as_object_mut()
                .unwrap()
                .insert("unexpected".into(), true.into());
        }),
        ("authority", |root| {
            root["authority"]["inventory_authoritative"] = true.into();
        }),
        ("foreign_owner", |root| {
            root["state"]["device"]["owner"]["subject"] = "foreign".into();
        }),
        ("foreign_runner", |root| {
            root["state"]["runner"]["device_id"] = "device-b".into();
        }),
        ("invalid_revision", |root| {
            root["state"]["revision"] = 0.into();
        }),
    ];
    for (name, mutation) in mutations {
        let mut value: Value = serde_json::from_slice(FIXTURE).unwrap();
        mutation(&mut value);
        let decoded: Result<Fixture, _> = decode(&serde_json::to_vec(&value).unwrap());
        if let Ok(fixture) = decoded {
            assert!(
                validate(&fixture).is_err(),
                "{name} mutation unexpectedly accepted"
            );
        }
    }
    let mut duplicate = FIXTURE.to_vec();
    duplicate.pop();
    duplicate
        .extend_from_slice(br#",\"schema_version\":\"forge.device-inventory-persistence/v1\"}"#);
    assert!(decode::<Fixture>(&duplicate).is_err());
}
