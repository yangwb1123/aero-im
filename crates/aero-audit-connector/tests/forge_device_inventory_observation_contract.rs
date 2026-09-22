//! Inventory observation v1 is a caller-supplied, offline value contract.
//!
//! The receiver validates the same resource declaration shape as Forge Core,
//! but does not discover devices, persist a row, authenticate an owner, select
//! a target, or grant reservation or execution authority.

#![allow(dead_code)]

use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt;

const FIXTURE: &[u8] = include_bytes!("testdata/forge-device-inventory-observation-v1.json");
const SCHEMA_VERSION: &str = "forge.device-inventory-observation/v1";
const EVALUATION_MODE: &str = "offline_static_only";
const NOTICE: &str = "Every owner, instance, state, timestamp, resource, residency, trust, sandbox, and concurrency value is an unverified caller declaration. This read-only observation selects no target and grants no execution authority.";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_TOKEN_BYTES: usize = 128;
const MAX_OWNER_BYTES: usize = 512;
const MAX_ARRAY_ITEMS: usize = 32;

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Owner {
    issuer: String,
    subject: String,
    tenant_id: String,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Gpu {
    present: bool,
    memory_bytes: u64,
    runtime: String,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Device {
    device_id: String,
    owner: Owner,
    approval_state: String,
    cordon_state: String,
    liveness: String,
    snapshot_observed_at_ms: u64,
    lease_expires_at_ms: u64,
    os: String,
    architecture: String,
    available_cpu_cores: u64,
    available_memory_bytes: u64,
    available_storage_bytes: u64,
    runtimes: Vec<String>,
    gpu: Gpu,
    data_residency_zones: Vec<String>,
    trust_zone: String,
    sandbox_levels: Vec<String>,
    concurrency_limit: u64,
    active_concurrency: u64,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Candidate {
    instance_id: String,
    device: Device,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: String,
    evaluation_mode: String,
    evaluated_at_ms: u64,
    owner_declaration: Owner,
    owner_declaration_unverified: bool,
    inventory_declarations_unverified: bool,
    notice: String,
    devices: Vec<Candidate>,
    execution_authorized: bool,
    reservation_created: bool,
    dispatch_performed: bool,
}

fn decode<T: DeserializeOwned>(raw: &[u8]) -> Result<T, String> {
    reject_duplicate_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value = T::deserialize(&mut decoder).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    Ok(value)
}

fn validate_fixture(fixture: &Fixture) -> Result<(), String> {
    if fixture.schema_version != SCHEMA_VERSION
        || fixture.evaluation_mode != EVALUATION_MODE
        || fixture.evaluated_at_ms != 200_000
        || !fixture.owner_declaration_unverified
        || !fixture.inventory_declarations_unverified
        || fixture.notice != NOTICE
        || fixture.execution_authorized
        || fixture.reservation_created
        || fixture.dispatch_performed
        || fixture.devices.len() != 2
        || !valid_owner(&fixture.owner_declaration)
    {
        return Err("invalid inventory observation envelope".into());
    }

    let mut devices = BTreeSet::new();
    let mut instances = BTreeSet::new();
    for (index, candidate) in fixture.devices.iter().enumerate() {
        if !valid_identifier(&candidate.instance_id)
            || !valid_device(&candidate.device)
            || candidate.device.owner != fixture.owner_declaration
            || !devices.insert(candidate.device.device_id.clone())
            || !instances.insert(candidate.instance_id.clone())
            || (index > 0
                && fixture.devices[index - 1].device.device_id >= candidate.device.device_id)
        {
            return Err(format!("invalid inventory candidate {index}"));
        }
    }

    let owner = Owner {
        issuer: "https://id.example".into(),
        subject: "user-1".into(),
        tenant_id: "tenant-1".into(),
    };
    if fixture.owner_declaration != owner {
        return Err("owner drift".into());
    }
    assert_candidate(
        &fixture.devices[0],
        "runner-a",
        "device-a",
        &owner,
        true,
        "clear",
        "online",
        8,
        16_384,
        8_192,
        &["oci"],
        &["us-west"],
        "standard",
        &["container", "microvm"],
        4,
        1,
    )?;
    assert_candidate(
        &fixture.devices[1],
        "runner-b",
        "device-b",
        &owner,
        false,
        "clear",
        "online",
        8,
        16_384,
        8_192,
        &["oci"],
        &["us-west"],
        "standard",
        &["container"],
        4,
        0,
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn assert_candidate(
    candidate: &Candidate,
    instance_id: &str,
    device_id: &str,
    owner: &Owner,
    approved: bool,
    cordon: &str,
    liveness: &str,
    cpu: u64,
    memory: u64,
    storage: u64,
    runtimes: &[&str],
    zones: &[&str],
    trust: &str,
    sandboxes: &[&str],
    concurrency: u64,
    active: u64,
) -> Result<(), String> {
    let device = &candidate.device;
    if candidate.instance_id != instance_id
        || device.device_id != device_id
        || device.owner != *owner
        || device.approval_state != if approved { "approved" } else { "pending" }
        || device.cordon_state != cordon
        || device.liveness != liveness
        || device.snapshot_observed_at_ms != 150_000
        || device.lease_expires_at_ms != 210_000
        || device.os != "linux"
        || device.architecture != "amd64"
        || device.available_cpu_cores != cpu
        || device.available_memory_bytes != memory
        || device.available_storage_bytes != storage
        || device.runtimes
            != runtimes
                .iter()
                .map(|value| (*value).into())
                .collect::<Vec<String>>()
        || device.data_residency_zones
            != zones
                .iter()
                .map(|value| (*value).into())
                .collect::<Vec<String>>()
        || device.trust_zone != trust
        || device.sandbox_levels
            != sandboxes
                .iter()
                .map(|value| (*value).into())
                .collect::<Vec<String>>()
        || device.concurrency_limit != concurrency
        || device.active_concurrency != active
        || device.gpu
            != (Gpu {
                present: false,
                memory_bytes: 0,
                runtime: String::new(),
            })
    {
        return Err(format!("candidate {device_id} drift"));
    }
    Ok(())
}

fn valid_device(device: &Device) -> bool {
    valid_identifier(&device.device_id)
        && valid_owner(&device.owner)
        && matches!(
            device.approval_state.as_str(),
            "approved" | "pending" | "revoked" | "unknown"
        )
        && matches!(
            device.cordon_state.as_str(),
            "clear" | "cordoned" | "unknown"
        )
        && matches!(device.liveness.as_str(), "online" | "offline" | "unknown")
        && device.snapshot_observed_at_ms <= MAX_SAFE_INTEGER
        && device.lease_expires_at_ms <= MAX_SAFE_INTEGER
        && valid_token(&device.os)
        && valid_token(&device.architecture)
        && device.available_cpu_cores <= u32::MAX as u64
        && device.available_memory_bytes <= MAX_SAFE_INTEGER
        && device.available_storage_bytes <= MAX_SAFE_INTEGER
        && valid_unique(&device.runtimes, valid_token)
        && valid_gpu(&device.gpu)
        && valid_unique(&device.data_residency_zones, valid_zone)
        && matches!(
            device.trust_zone.as_str(),
            "unknown" | "untrusted" | "low" | "standard" | "high" | "restricted"
        )
        && valid_unique(&device.sandbox_levels, |value| {
            matches!(value, "process" | "container" | "microvm")
        })
        && device.concurrency_limit <= u16::MAX as u64
        && device.active_concurrency <= u16::MAX as u64
}

fn valid_gpu(gpu: &Gpu) -> bool {
    if gpu.present {
        gpu.memory_bytes <= MAX_SAFE_INTEGER
            && (gpu.runtime.is_empty() || valid_token(&gpu.runtime))
    } else {
        gpu.memory_bytes == 0 && gpu.runtime.is_empty()
    }
}

fn valid_owner(owner: &Owner) -> bool {
    [
        owner.issuer.as_str(),
        owner.subject.as_str(),
        owner.tenant_id.as_str(),
    ]
    .iter()
    .all(|value| {
        !value.is_empty()
            && value.len() <= MAX_OWNER_BYTES
            && value.trim() == *value
            && !value.chars().any(char::is_control)
    })
}

fn valid_identifier(value: &str) -> bool {
    if value.is_empty() || value.len() > MAX_TOKEN_BYTES {
        return false;
    }
    value.chars().enumerate().all(|(index, character)| {
        character.is_ascii_alphanumeric()
            || (index > 0 && matches!(character, '.' | '_' | ':' | '-'))
    })
}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TOKEN_BYTES
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || ".:_+/-".contains(character))
}

fn valid_zone(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || ".-_".contains(character))
}

fn valid_unique<F>(values: &[String], valid: F) -> bool
where
    F: Fn(&str) -> bool,
{
    values.len() <= MAX_ARRAY_ITEMS
        && values.iter().all(|value| valid(value))
        && values.iter().collect::<BTreeSet<_>>().len() == values.len()
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
fn canonical_inventory_observation_is_strict_unverified_and_sorted() {
    let fixture: Fixture = decode(FIXTURE).expect("canonical inventory observation fixture");
    validate_fixture(&fixture).expect("valid inventory observation fixture");
    assert_eq!(fixture.devices[0].device.device_id, "device-a");
    assert_eq!(fixture.devices[1].device.device_id, "device-b");
}

#[test]
fn inventory_observation_rejects_unknown_duplicate_binding_and_authority_mutations() {
    let mut mutations: Vec<(&str, Box<dyn Fn(&mut Value)>)> = vec![
        (
            "unknown_root",
            Box::new(|root| root["unexpected"] = true.into()),
        ),
        (
            "unknown_device",
            Box::new(|root| root["devices"][0]["device"]["unexpected"] = true.into()),
        ),
        (
            "selected_target",
            Box::new(|root| root["selected_target_id"] = "device-a".into()),
        ),
        (
            "owner_flags",
            Box::new(|root| root["owner_declaration_unverified"] = false.into()),
        ),
        (
            "inventory_flag",
            Box::new(|root| root["inventory_declarations_unverified"] = false.into()),
        ),
        (
            "foreign_owner",
            Box::new(|root| {
                root["devices"][0]["device"]["owner"]["tenant_id"] = "tenant-foreign".into()
            }),
        ),
        (
            "duplicate_device",
            Box::new(|root| root["devices"][1]["device"]["device_id"] = "device-a".into()),
        ),
        (
            "duplicate_instance",
            Box::new(|root| root["devices"][1]["instance_id"] = "runner-a".into()),
        ),
        (
            "unsorted",
            Box::new(|root| {
                let devices = root["devices"].as_array_mut().expect("devices");
                devices.swap(0, 1);
            }),
        ),
        (
            "invalid_identifier",
            Box::new(|root| root["devices"][0]["instance_id"] = "runner/a".into()),
        ),
        (
            "resource_bound",
            Box::new(|root| {
                root["devices"][0]["device"]["available_memory_bytes"] =
                    (MAX_SAFE_INTEGER + 1).into()
            }),
        ),
        (
            "cpu_width_bound",
            Box::new(|root| {
                root["devices"][0]["device"]["available_cpu_cores"] = (u32::MAX as u64 + 1).into()
            }),
        ),
        (
            "concurrency_width_bound",
            Box::new(|root| {
                root["devices"][0]["device"]["concurrency_limit"] = (u16::MAX as u64 + 1).into()
            }),
        ),
        (
            "active_concurrency_width_bound",
            Box::new(|root| {
                root["devices"][0]["device"]["active_concurrency"] = (u16::MAX as u64 + 1).into()
            }),
        ),
        (
            "gpu_mutation",
            Box::new(|root| root["devices"][0]["device"]["gpu"]["memory_bytes"] = 1.into()),
        ),
    ];
    for key in [
        "execution_authorized",
        "reservation_created",
        "dispatch_performed",
    ] {
        mutations.push((key, Box::new(move |root| root[key] = true.into())));
    }
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
        .expect("root close")
        .len();
    duplicate.truncate(end);
    duplicate.push_str(",\"schema_version\":\"forge.device-inventory-observation/v1\"}\n");
    assert!(decode::<Fixture>(duplicate.as_bytes()).is_err());
}
