//! Forge device resource summaries are consumed as bounded, caller-declared
//! observations only. This receiver does not authenticate or enroll a device,
//! persist inventory, select or reserve a target, dispatch work, or publish
//! Audit authority.

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};
use std::fmt;

const FIXTURE: &[u8] = include_bytes!("testdata/forge-device-resource-summary-v1.json");

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
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

impl Authority {
    fn is_zero(&self) -> bool {
        self == &Self::default()
    }
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GPU {
    present: bool,
    memory_bytes: u64,
    runtime: String,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Device {
    device_id: String,
    owner: Owner,
    approval_state: String,
    cordon_state: String,
    liveness: String,
    snapshot_observed_at_ms: i64,
    lease_expires_at_ms: i64,
    os: String,
    architecture: String,
    available_cpu_cores: u64,
    available_memory_bytes: u64,
    available_storage_bytes: u64,
    runtimes: Vec<String>,
    gpu: GPU,
    data_residency_zones: Vec<String>,
    trust_zone: String,
    sandbox_levels: Vec<String>,
    concurrency_limit: u64,
    active_concurrency: u64,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InventoryRow {
    instance_id: String,
    device: Device,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Inventory {
    schema_version: String,
    evaluation_mode: String,
    evaluated_at_ms: i64,
    owner_declaration: Owner,
    owner_declaration_unverified: bool,
    inventory_declarations_unverified: bool,
    notice: String,
    devices: Vec<InventoryRow>,
    execution_authorized: bool,
    reservation_created: bool,
    dispatch_performed: bool,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Decision {
    device_id: String,
    instance_id: String,
    matches_requirements: bool,
    exclusion_reasons: Vec<String>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Placement {
    schema_version: String,
    evaluation_mode: String,
    owner: Owner,
    conversation_id: String,
    run_id: String,
    evaluated_at_ms: i64,
    owner_declaration_unverified: bool,
    device_attributes_unverified: bool,
    decisions: Vec<Decision>,
    selected_device_id: Option<String>,
    selected_instance_id: Option<String>,
    authority: Authority,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Expected {
    schema_version: String,
    evaluation_mode: String,
    conversation_id: String,
    run_id: String,
    evaluated_at_ms: i64,
    owner_declaration_unverified: bool,
    inventory_declarations_unverified: bool,
    placement_declaration_unverified: bool,
    notice: String,
    device_count: usize,
    runner_instance_count: usize,
    available_cpu_cores: u64,
    available_memory_bytes: u64,
    available_storage_bytes: u64,
    available_gpu_count: usize,
    available_gpu_memory_bytes: u64,
    eligible_device_count: usize,
    eligible_instance_count: usize,
    selected_device_id: Option<String>,
    selected_instance_id: Option<String>,
    authority: Authority,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    api_version: String,
    inventory_contract_fixture: String,
    placement_contract_fixture: String,
    owner: Owner,
    inventory: Inventory,
    placement_observation: Placement,
    expected: Expected,
}

fn decode(raw: &[u8]) -> Result<Fixture, String> {
    reject_duplicate_json_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value = Fixture::deserialize(&mut decoder).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    Ok(value)
}

fn semantically_display_only(value: &Fixture) -> bool {
    value.owner.issuer != ""
        && value.owner.subject != ""
        && value.owner.tenant_id != ""
        && value.inventory.owner_declaration == value.owner
        && !value.inventory.execution_authorized
        && !value.inventory.reservation_created
        && !value.inventory.dispatch_performed
        && value.placement_observation.owner == value.owner
        && value.placement_observation.selected_device_id.is_none()
        && value.placement_observation.selected_instance_id.is_none()
        && value.placement_observation.authority.is_zero()
        && value.expected.selected_device_id.is_none()
        && value.expected.selected_instance_id.is_none()
        && value.expected.authority.is_zero()
        && value
            .inventory
            .devices
            .iter()
            .all(|row| row.device.owner == value.owner)
}

#[test]
fn canonical_resource_summary_is_bounded_and_authority_free() {
    let value = decode(FIXTURE).expect("canonical device resource summary fixture");
    assert_eq!(
        value.api_version,
        "forgeos.device-resource-summary-contract/v1"
    );
    assert_eq!(
        value.inventory_contract_fixture,
        "forge-device-inventory-observation-v1"
    );
    assert_eq!(
        value.placement_contract_fixture,
        "forge-session-placement-observation-v1"
    );
    assert_eq!(
        value.inventory.schema_version,
        "forge.device-inventory-observation/v1"
    );
    assert_eq!(value.inventory.evaluation_mode, "offline_static_only");
    assert!(value.inventory.owner_declaration_unverified);
    assert!(value.inventory.inventory_declarations_unverified);
    assert!(!value.inventory.execution_authorized);
    assert!(!value.inventory.reservation_created);
    assert!(!value.inventory.dispatch_performed);
    assert_eq!(value.inventory.devices.len(), 9);
    assert!(value
        .inventory
        .devices
        .windows(2)
        .all(|pair| pair[0].instance_id < pair[1].instance_id
            && pair[0].device.device_id < pair[1].device.device_id));
    assert!(value.inventory.devices.iter().all(|row| {
        row.instance_id != ""
            && row.device.owner == value.owner
            && row.device.snapshot_observed_at_ms > 0
            && row.device.lease_expires_at_ms > 0
            && row.device.active_concurrency <= row.device.concurrency_limit
    }));

    let placement = &value.placement_observation;
    assert_eq!(
        placement.schema_version,
        "forge.session-placement-observation/v1"
    );
    assert_eq!(placement.evaluation_mode, "offline_static_only");
    assert!(placement.owner_declaration_unverified);
    assert!(placement.device_attributes_unverified);
    assert_eq!(placement.decisions.len(), 9);
    assert!(placement.selected_device_id.is_none());
    assert!(placement.selected_instance_id.is_none());
    assert!(placement.authority.is_zero());
    assert!(placement.decisions.windows(2).all(|pair| {
        pair[0].device_id < pair[1].device_id && pair[0].instance_id < pair[1].instance_id
    }));

    let expected = &value.expected;
    assert_eq!(expected.schema_version, "forge.device-resource-summary/v1");
    assert_eq!(expected.evaluation_mode, "offline_static_only");
    assert_eq!(expected.device_count, 9);
    assert_eq!(expected.runner_instance_count, 9);
    assert_eq!(expected.available_cpu_cores, 66);
    assert_eq!(expected.available_memory_bytes, 135_168);
    assert_eq!(expected.available_storage_bytes, 67_584);
    assert_eq!(expected.available_gpu_count, 1);
    assert_eq!(expected.available_gpu_memory_bytes, 4_096);
    assert_eq!(expected.eligible_device_count, 2);
    assert_eq!(expected.eligible_instance_count, 2);
    assert!(semantically_display_only(&value));
}

#[test]
fn unknown_authority_foreign_owner_selection_and_duplicate_fail_closed() {
    let mut unknown: Map<String, Value> = serde_json::from_slice(FIXTURE).expect("object fixture");
    unknown.insert("prompt".into(), Value::String("raw prompt".into()));
    assert!(decode(&serde_json::to_vec(&unknown).expect("unknown mutation")).is_err());

    let mut authority: Map<String, Value> =
        serde_json::from_slice(FIXTURE).expect("object fixture");
    authority["expected"]["authority"]["execution_authorized"] = Value::Bool(true);
    let value = decode(&serde_json::to_vec(&authority).expect("authority mutation"))
        .expect("authority shape remains decodable");
    assert!(!semantically_display_only(&value));

    let mut foreign: Map<String, Value> = serde_json::from_slice(FIXTURE).expect("object fixture");
    foreign["inventory"]["devices"][0]["device"]["owner"]["subject"] =
        Value::String("other-user".into());
    let value = decode(&serde_json::to_vec(&foreign).expect("foreign-owner mutation"))
        .expect("foreign-owner shape remains decodable");
    assert!(!semantically_display_only(&value));

    let mut selected: Map<String, Value> = serde_json::from_slice(FIXTURE).expect("object fixture");
    selected["placement_observation"]["selected_device_id"] = Value::String("candidate-a".into());
    let value = decode(&serde_json::to_vec(&selected).expect("selection mutation"))
        .expect("selection shape remains decodable");
    assert!(!semantically_display_only(&value));

    let duplicate = format!(
        "{},\"api_version\":\"forgeos.device-resource-summary-contract/v1\"}}",
        String::from_utf8_lossy(FIXTURE).trim_end_matches('}')
    );
    assert!(decode(duplicate.as_bytes()).is_err());
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
