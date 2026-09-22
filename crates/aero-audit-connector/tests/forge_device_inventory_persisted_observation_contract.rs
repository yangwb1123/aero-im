//! The persisted inventory observation is a bounded projection of already
//! restored values. It is not device authentication, heartbeat persistence,
//! inventory authority, reservation, or execution authority.

#![allow(dead_code)]

use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};
use std::fmt;

const FIXTURE: &[u8] =
    include_bytes!("testdata/forge-device-inventory-persisted-observation-v1.json");

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
struct Gpu {
    id: String,
    vendor: String,
    memory_bytes: u64,
    available_memory_bytes: u64,
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
    gpus: Vec<Gpu>,
    runtimes: Vec<String>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeviceState {
    device_id: String,
    owner: Owner,
    approval_state: String,
    cordon_state: String,
    reservation_state: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunnerState {
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
    device: DeviceState,
    runner: RunnerState,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GpuDeclaration {
    present: bool,
    memory_bytes: u64,
    runtime: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedDevice {
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
    gpu: GpuDeclaration,
    data_residency_zones: Vec<String>,
    trust_zone: String,
    sandbox_levels: Vec<String>,
    concurrency_limit: u64,
    active_concurrency: u64,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedCandidate {
    instance_id: String,
    device: ExpectedDevice,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Expected {
    schema_version: String,
    evaluation_mode: String,
    evaluated_at_ms: u64,
    owner_declaration: Owner,
    owner_declaration_unverified: bool,
    inventory_declarations_unverified: bool,
    notice: String,
    devices: Vec<ExpectedCandidate>,
    execution_authorized: bool,
    reservation_created: bool,
    dispatch_performed: bool,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: String,
    evaluation_mode: String,
    evaluation_owner: Owner,
    evaluated_at_ms: u64,
    authority: Authority,
    states: Vec<State>,
    expected: Expected,
}

fn decode<T: DeserializeOwned>(raw: &[u8]) -> Result<T, String> {
    reject_duplicate_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value = T::deserialize(&mut decoder).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    Ok(value)
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
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(ScanVisitor)
    }
}
impl<'de> Visitor<'de> for ScanVisitor {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("JSON without duplicate object keys")
    }
    fn visit_map<A>(self, mut map: A) -> Result<(), A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut keys = std::collections::BTreeSet::new();
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

fn authority_free(fixture: &Fixture) -> bool {
    fixture.authority == Authority::default()
}
fn validate(fixture: &Fixture) -> Result<(), &'static str> {
    if fixture.schema_version != "forge.device-inventory-persisted-observation/v1"
        || fixture.evaluation_mode != "pure_persisted_inventory_to_observation"
        || fixture.evaluated_at_ms != 1500
        || !authority_free(fixture)
    {
        return Err("envelope");
    }
    let owner = &fixture.evaluation_owner;
    if owner.issuer != "issuer"
        || owner.subject != "user"
        || owner.tenant_id != "tenant"
        || fixture.states.len() != 2
    {
        return Err("owner/states");
    }
    for state in &fixture.states {
        if state.revision == 0
            || state.device.owner != *owner
            || state.device.device_id.is_empty()
            || state.runner.device_id != state.device.device_id
            || state.runner.instance_id.is_empty()
            || state.runner.generation == 0
            || state.runner.heartbeat_sequence == 0
            || state.runner.capability_lease_expires_at_ms <= state.runner.server_observed_at_ms
            || state.device.reservation_state != "none"
            || state.runner.capabilities.available_cpu_cores > state.runner.capabilities.cpu_cores
            || state.runner.capabilities.available_memory_bytes
                > state.runner.capabilities.memory_bytes
            || state.runner.capabilities.available_storage_bytes
                > state.runner.capabilities.storage_bytes
        {
            return Err("state");
        }
    }
    let expected = &fixture.expected;
    if expected.schema_version != "forge.device-inventory-observation/v1"
        || expected.evaluation_mode != "offline_static_only"
        || expected.evaluated_at_ms != fixture.evaluated_at_ms
        || expected.owner_declaration != *owner
        || !expected.owner_declaration_unverified
        || !expected.inventory_declarations_unverified
        || expected.notice.is_empty()
        || expected.devices.len() != 2
        || expected.execution_authorized
        || expected.reservation_created
        || expected.dispatch_performed
    {
        return Err("expected");
    }
    for (index, candidate) in expected.devices.iter().enumerate() {
        if candidate.device.owner != *owner
            || candidate.instance_id.is_empty()
            || candidate.device.device_id.is_empty()
            || candidate.device.gpu.present
            || candidate.device.trust_zone != "unknown"
            || candidate.device.concurrency_limit != 0
            || candidate.device.active_concurrency != 0
        {
            return Err("candidate");
        }
        if index > 0 && expected.devices[index - 1].device.device_id >= candidate.device.device_id {
            return Err("sort");
        }
    }
    Ok(())
}

#[test]
fn canonical_persisted_inventory_observation_is_bound_and_authority_free() {
    let fixture: Fixture = decode(FIXTURE).expect("persisted inventory observation fixture");
    validate(&fixture).expect("valid persisted inventory observation");
    assert_eq!(fixture.states[0].device.device_id, "device-b");
    assert_eq!(fixture.states[1].device.device_id, "device-a");
    assert_eq!(fixture.expected.devices[0].device.device_id, "device-a");
    assert_eq!(fixture.expected.devices[1].device.device_id, "device-b");
}

#[test]
fn persisted_inventory_observation_rejects_unknown_duplicate_foreign_and_authority_mutation() {
    let mut unknown: Map<String, Value> = serde_json::from_slice(FIXTURE).unwrap();
    unknown.insert("unexpected".into(), Value::Bool(true));
    assert!(decode::<Fixture>(&serde_json::to_vec(&unknown).unwrap()).is_err());
    let mut authority: Map<String, Value> = serde_json::from_slice(FIXTURE).unwrap();
    authority["authority"]["inventory_authoritative"] = Value::Bool(true);
    let value: Fixture = decode(&serde_json::to_vec(&authority).unwrap()).unwrap();
    assert!(validate(&value).is_err());
    let mut foreign: Map<String, Value> = serde_json::from_slice(FIXTURE).unwrap();
    foreign["evaluation_owner"]["subject"] = Value::String("foreign".into());
    let value: Fixture = decode(&serde_json::to_vec(&foreign).unwrap()).unwrap();
    assert!(validate(&value).is_err());
    let duplicate = format!(
        ",\"schema_version\":\"forge.device-inventory-persisted-observation/v1\"}}{}",
        String::from_utf8_lossy(FIXTURE).trim_end_matches('}')
    );
    assert!(decode::<Fixture>(duplicate.as_bytes()).is_err());
}
