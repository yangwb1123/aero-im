//! Lossless inventory observation v2 is still a caller-supplied offline value.
//!
//! This receiver test deliberately keeps revision, Runner counters,
//! reservation declarations, and GPU rows as unverified metadata. It does not
//! read storage, ingest heartbeats, authenticate a Runner, reserve capacity,
//! select a target, or grant execution authority.

#![allow(dead_code)]

use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt;

const FIXTURE: &[u8] = include_bytes!("testdata/forge-device-inventory-observation-v2.json");
const SCHEMA_VERSION: &str = "forge.device-inventory-observation/v2";
const EVALUATION_MODE: &str = "offline_static_only";
const NOTICE: &str = "Every owner, instance, state, timestamp, resource, GPU, reservation, residency, trust, sandbox, and concurrency value is an unverified caller declaration. This read-only observation selects no target and grants no execution authority.";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_TOKEN_BYTES: usize = 128;
const MAX_OWNER_BYTES: usize = 512;
const MAX_ARRAY_ITEMS: usize = 32;
const MAX_GPU_COUNT: usize = 32;
const MAX_CPU_CORES: u64 = 4_096;
const MIN_LEASE_TTL_MS: u64 = 1_000;
const MAX_LEASE_TTL_MS: u64 = 600_000;
const MAX_CAPABILITY_BYTES: u64 = 1 << 60;

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
    id: String,
    vendor: String,
    memory_bytes: u64,
    available_memory_bytes: u64,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Device {
    device_id: String,
    owner: Owner,
    approval_state: String,
    cordon_state: String,
    reservation_state: String,
    liveness: String,
    snapshot_observed_at_ms: u64,
    lease_expires_at_ms: u64,
    os: String,
    architecture: String,
    available_cpu_cores: u64,
    available_memory_bytes: u64,
    available_storage_bytes: u64,
    runtimes: Vec<String>,
    gpus: Vec<Gpu>,
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
    revision: u64,
    generation: u64,
    heartbeat_sequence: u64,
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
        return Err("invalid inventory observation v2 envelope".into());
    }

    let expected_owner = Owner {
        issuer: "issuer".into(),
        subject: "user".into(),
        tenant_id: "tenant".into(),
    };
    if fixture.owner_declaration != expected_owner {
        return Err("owner drift".into());
    }

    let mut seen_devices = BTreeSet::new();
    let mut seen_instances = BTreeSet::new();
    for (index, candidate) in fixture.devices.iter().enumerate() {
        if candidate.revision == 0
            || candidate.generation == 0
            || candidate.heartbeat_sequence == 0
            || candidate.revision > MAX_SAFE_INTEGER
            || candidate.generation > MAX_SAFE_INTEGER
            || candidate.heartbeat_sequence > MAX_SAFE_INTEGER
            || !valid_identifier(&candidate.instance_id, true)
            || !valid_device(&candidate.device, &fixture.owner_declaration)
            || !seen_devices.insert(candidate.device.device_id.clone())
            || !seen_instances.insert(candidate.instance_id.clone())
            || (index > 0
                && (fixture.devices[index - 1].device.device_id > candidate.device.device_id
                    || (fixture.devices[index - 1].device.device_id == candidate.device.device_id
                        && fixture.devices[index - 1].instance_id >= candidate.instance_id)))
        {
            return Err(format!("invalid inventory v2 candidate {index}"));
        }
    }

    assert_candidate(
        &fixture.devices[0],
        "runner-a",
        1,
        1,
        1,
        "device-a",
        &expected_owner,
        "approved",
        "clear",
        "reserved",
        "online",
        2,
    )?;
    assert_candidate(
        &fixture.devices[1],
        "runner-b",
        2,
        2,
        4,
        "device-b",
        &expected_owner,
        "pending",
        "cordoned",
        "none",
        "offline",
        0,
    )?;
    Ok(())
}

fn assert_candidate(
    candidate: &Candidate,
    instance_id: &str,
    revision: u64,
    generation: u64,
    heartbeat_sequence: u64,
    device_id: &str,
    owner: &Owner,
    approval: &str,
    cordon: &str,
    reservation: &str,
    liveness: &str,
    gpu_count: usize,
) -> Result<(), String> {
    let device = &candidate.device;
    if candidate.instance_id != instance_id
        || candidate.revision != revision
        || candidate.generation != generation
        || candidate.heartbeat_sequence != heartbeat_sequence
        || device.device_id != device_id
        || device.owner != *owner
        || device.approval_state != approval
        || device.cordon_state != cordon
        || device.reservation_state != reservation
        || device.liveness != liveness
        || device.snapshot_observed_at_ms != 100_000
        || device.lease_expires_at_ms != 200_000
        || device.os != "linux"
        || device.architecture != "amd64"
        || device.available_cpu_cores != 7
        || device.available_memory_bytes != 8_589_934_592
        || device.available_storage_bytes != 53_687_091_200
        || device.runtimes != ["go", "rust"]
        || !device.data_residency_zones.is_empty()
        || device.trust_zone != "unknown"
        || !device.sandbox_levels.is_empty()
        || device.concurrency_limit != 0
        || device.active_concurrency != 0
        || device.gpus.len() != gpu_count
    {
        return Err(format!("candidate {device_id} drift"));
    }
    if device_id == "device-a"
        && device.gpus
            != [
                Gpu {
                    id: "gpu-a".into(),
                    vendor: "NVIDIA".into(),
                    memory_bytes: 17_179_869_184,
                    available_memory_bytes: 12_884_901_888,
                },
                Gpu {
                    id: "gpu-b".into(),
                    vendor: "NVIDIA".into(),
                    memory_bytes: 8_589_934_592,
                    available_memory_bytes: 4_294_967_296,
                },
            ]
    {
        return Err("device-a GPU declaration drift".into());
    }
    if device_id == "device-b" && !device.gpus.is_empty() {
        return Err("device-b GPU declaration drift".into());
    }
    Ok(())
}

fn valid_device(device: &Device, owner: &Owner) -> bool {
    device.owner == *owner
        && valid_identifier(&device.device_id, false)
        && valid_owner(&device.owner)
        && matches!(
            device.approval_state.as_str(),
            "pending" | "approved" | "revoked"
        )
        && matches!(device.cordon_state.as_str(), "clear" | "cordoned")
        && matches!(device.reservation_state.as_str(), "none" | "reserved")
        && matches!(device.liveness.as_str(), "online" | "offline")
        && device.snapshot_observed_at_ms <= MAX_SAFE_INTEGER
        && device.lease_expires_at_ms <= MAX_SAFE_INTEGER
        && device
            .lease_expires_at_ms
            .checked_sub(device.snapshot_observed_at_ms)
            .is_some_and(|ttl| (MIN_LEASE_TTL_MS..=MAX_LEASE_TTL_MS).contains(&ttl))
        && valid_canonical_tag(&device.os)
        && valid_canonical_tag(&device.architecture)
        && device.available_cpu_cores <= MAX_CPU_CORES
        && device.available_memory_bytes <= MAX_SAFE_INTEGER
        && device.available_storage_bytes <= MAX_SAFE_INTEGER
        && valid_sorted_unique(&device.runtimes, valid_canonical_tag)
        && device.gpus.len() <= MAX_GPU_COUNT
        && valid_gpus(&device.gpus)
        && device.data_residency_zones.is_empty()
        && device.trust_zone == "unknown"
        && device.sandbox_levels.is_empty()
        && device.concurrency_limit == 0
        && device.active_concurrency == 0
}

fn valid_gpus(gpus: &[Gpu]) -> bool {
    let mut seen = BTreeSet::new();
    let mut previous = None;
    let mut available_total = 0_u64;
    for gpu in gpus {
        if previous.is_some_and(|value: &str| value >= gpu.id.as_str())
            || !seen.insert(gpu.id.clone())
            || !valid_identifier(&gpu.id, false)
            || !valid_vendor(&gpu.vendor)
            || gpu.memory_bytes == 0
            || gpu.memory_bytes > MAX_SAFE_INTEGER
            || gpu.memory_bytes > MAX_CAPABILITY_BYTES
            || gpu.available_memory_bytes > gpu.memory_bytes
            || gpu.available_memory_bytes > MAX_SAFE_INTEGER
        {
            return false;
        }
        available_total = match available_total.checked_add(gpu.available_memory_bytes) {
            Some(value) if value <= MAX_SAFE_INTEGER => value,
            _ => return false,
        };
        previous = Some(gpu.id.as_str());
    }
    true
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

fn valid_identifier(value: &str, allow_runner_punctuation: bool) -> bool {
    if value.is_empty() || value.len() > MAX_TOKEN_BYTES {
        return false;
    }
    value.chars().enumerate().all(|(index, character)| {
        character.is_ascii_alphanumeric()
            || (index > 0
                && if allow_runner_punctuation {
                    matches!(character, '.' | '_' | ':' | '-' | '+' | '/')
                } else {
                    matches!(character, '.' | '_' | ':' | '-')
                })
    })
}

fn valid_canonical_tag(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.chars().all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || ".-_+".contains(character)
        })
}

fn valid_vendor(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn valid_unique<F>(values: &[String], valid: F) -> bool
where
    F: Fn(&str) -> bool,
{
    values.len() <= MAX_ARRAY_ITEMS
        && values.iter().all(|value| valid(value))
        && values.iter().collect::<BTreeSet<_>>().len() == values.len()
}

fn valid_sorted_unique<F>(values: &[String], valid: F) -> bool
where
    F: Fn(&str) -> bool,
{
    values.len() <= MAX_ARRAY_ITEMS
        && values.iter().all(|value| valid(value))
        && values.windows(2).all(|window| window[0] < window[1])
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
fn canonical_inventory_observation_v2_is_lossless_unverified_and_sorted() {
    let fixture: Fixture = decode(FIXTURE).expect("canonical inventory observation v2 fixture");
    validate_fixture(&fixture).expect("valid inventory observation v2 fixture");
    assert_eq!(fixture.devices[0].device.device_id, "device-a");
    assert_eq!(fixture.devices[1].device.device_id, "device-b");
}

#[test]
fn inventory_observation_v2_rejects_unknown_duplicate_binding_and_authority_mutations() {
    let mut mutations: Vec<(&str, Box<dyn Fn(&mut Value)>)> = vec![
        (
            "unknown_root",
            Box::new(|root| root["unexpected"] = true.into()),
        ),
        (
            "unknown_gpu",
            Box::new(|root| root["devices"][0]["device"]["gpus"][0]["unexpected"] = true.into()),
        ),
        (
            "selected_target",
            Box::new(|root| root["selected_target_id"] = "device-a".into()),
        ),
        (
            "schedulable_claim",
            Box::new(|root| root["devices"][0]["schedulable"] = true.into()),
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
            Box::new(|root| root["devices"][0]["device"]["owner"]["tenant_id"] = "foreign".into()),
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
            Box::new(|root| root["devices"].as_array_mut().unwrap().swap(0, 1)),
        ),
        (
            "unsafe_revision",
            Box::new(|root| root["devices"][0]["revision"] = (MAX_SAFE_INTEGER + 1).into()),
        ),
        (
            "invalid_lease",
            Box::new(|root| root["devices"][0]["device"]["lease_expires_at_ms"] = 100_500.into()),
        ),
        (
            "invalid_reservation",
            Box::new(|root| root["devices"][0]["device"]["reservation_state"] = "none".into()),
        ),
        (
            "gpu_memory",
            Box::new(|root| {
                root["devices"][0]["device"]["gpus"][0]["available_memory_bytes"] =
                    (17_179_869_184_u64 + 1).into()
            }),
        ),
        (
            "gpu_order",
            Box::new(|root| {
                root["devices"][0]["device"]["gpus"]
                    .as_array_mut()
                    .unwrap()
                    .swap(0, 1)
            }),
        ),
        (
            "runtime_order",
            Box::new(|root| {
                root["devices"][0]["device"]["runtimes"] = serde_json::json!(["rust", "go"])
            }),
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
    duplicate.push_str(",\"schema_version\":\"forge.device-inventory-observation/v2\"}\n");
    assert!(decode::<Fixture>(duplicate.as_bytes()).is_err());
}
