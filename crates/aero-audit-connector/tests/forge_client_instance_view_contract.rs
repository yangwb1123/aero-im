//! Client-instance session/resource views are display-only compatibility
//! values. This receiver-side test proves Aero-IM can inspect the same bounded
//! shape without turning it into identity, audit, or execution authority.

use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};
use std::fmt;

const SESSION_FIXTURE: &[u8] =
    include_bytes!("testdata/forge-client-instance-session-view-v1.json");
const RESOURCE_FIXTURE: &[u8] =
    include_bytes!("testdata/forge-client-instance-resource-view-v1.json");

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Owner {
    issuer: String,
    subject: String,
    tenant_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Instance {
    instance_id: String,
    client_kind: String,
    session_ids: Vec<String>,
    observed_at_ms: i64,
    status: String,
}

#[derive(Debug, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Authority {
    owner_authenticated: bool,
    session_read_authorized: bool,
    prompt_write_authorized: bool,
    device_identity_verified: bool,
    reservation_created: bool,
    execution_authorized: bool,
    dispatch_performed: bool,
    audit_published: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionView {
    schema_version: String,
    evaluation_mode: String,
    owner_declaration: Owner,
    owner_declaration_unverified: bool,
    instances: Vec<Instance>,
    read_only: bool,
    authority: Authority,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Device {
    device_id: String,
    runner_instance_id: String,
    owner: Owner,
    revision: u64,
    generation: u64,
    heartbeat_sequence: u64,
    observed_at_ms: i64,
    approval_state: String,
    cordon_state: String,
    reservation_state: String,
    liveness: String,
    os: String,
    architecture: String,
    cpu_cores: u32,
    available_cpu_cores: u32,
    memory_bytes: u64,
    available_memory_bytes: u64,
    storage_bytes: u64,
    available_storage_bytes: u64,
    gpu_count: u32,
    available_gpu_memory_bytes: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResourceView {
    schema_version: String,
    evaluation_mode: String,
    owner_declaration: Owner,
    owner_declaration_unverified: bool,
    instances: Vec<Instance>,
    devices: Vec<Device>,
    device_attributes_unverified: bool,
    read_only: bool,
    authority: Authority,
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
        D: serde::Deserializer<'de>,
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

fn decode<T: DeserializeOwned>(raw: &[u8]) -> Result<T, String> {
    reject_duplicate_json_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value = T::deserialize(&mut decoder).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    Ok(value)
}

fn instance_rows_valid(owner: &Owner, rows: &[Instance]) -> bool {
    rows.len() == 5
        && rows
            .iter()
            .map(|row| row.client_kind.as_str())
            .eq(["app", "cli", "mobile", "tui", "web"])
        && rows
            .windows(2)
            .all(|pair| pair[0].instance_id < pair[1].instance_id)
        && rows.iter().all(|row| {
            !row.instance_id.is_empty()
                && !row.client_kind.is_empty()
                && row.observed_at_ms > 0
                && !row.status.is_empty()
                && row.session_ids.windows(2).all(|pair| pair[0] < pair[1])
                && row.session_ids.iter().all(|id| !id.is_empty())
                && !owner.issuer.is_empty()
                && !owner.subject.is_empty()
                && !owner.tenant_id.is_empty()
        })
}

fn session_is_display_only(value: &SessionView) -> bool {
    value.schema_version == "forge.client-instance-session-view/v1"
        && value.evaluation_mode == "owner_bound_session_view_only"
        && value.owner_declaration_unverified
        && value.read_only
        && value.authority == Authority::default()
        && instance_rows_valid(&value.owner_declaration, &value.instances)
}

fn resource_is_display_only(value: &ResourceView) -> bool {
    value.schema_version == "forge.client-instance-resource-view/v1"
        && value.evaluation_mode == "owner_bound_instance_resource_view_only"
        && value.owner_declaration_unverified
        && value.device_attributes_unverified
        && value.read_only
        && value.authority == Authority::default()
        && instance_rows_valid(&value.owner_declaration, &value.instances)
        && value.devices.len() == 2
        && value.devices.windows(2).all(|pair| {
            pair[0].device_id < pair[1].device_id
                && pair[0].runner_instance_id < pair[1].runner_instance_id
        })
        && value.devices.iter().all(|device| {
            device.owner == value.owner_declaration
                && !device.device_id.is_empty()
                && !device.runner_instance_id.is_empty()
                && device.revision > 0
                && device.generation > 0
                && device.heartbeat_sequence > 0
                && device.observed_at_ms > 0
                && !device.approval_state.is_empty()
                && !device.cordon_state.is_empty()
                && !device.reservation_state.is_empty()
                && !device.liveness.is_empty()
                && !device.os.is_empty()
                && !device.architecture.is_empty()
                && device.available_cpu_cores <= device.cpu_cores
                && device.available_memory_bytes <= device.memory_bytes
                && device.available_storage_bytes <= device.storage_bytes
                && device.available_gpu_memory_bytes <= 9_007_199_254_740_991
        })
}

#[test]
fn canonical_client_instance_views_are_display_only() {
    let session: SessionView = decode(SESSION_FIXTURE).expect("session fixture");
    assert!(session_is_display_only(&session));
    let resource: ResourceView = decode(RESOURCE_FIXTURE).expect("resource fixture");
    assert!(resource_is_display_only(&resource));
    assert_eq!(resource.devices[0].device_id, "device-a");
    assert_eq!(resource.devices[0].gpu_count, 2);
}

#[test]
fn client_instance_views_reject_unknown_duplicate_authority_and_foreign_owner() {
    let mut unknown: Map<String, Value> = serde_json::from_slice(SESSION_FIXTURE).unwrap();
    unknown.insert("prompt".into(), Value::String("raw prompt".into()));
    assert!(decode::<SessionView>(&serde_json::to_vec(&unknown).unwrap()).is_err());

    let duplicate = format!(
        "{},\"schema_version\":\"forge.client-instance-session-view/v1\"}}",
        String::from_utf8_lossy(SESSION_FIXTURE).trim_end_matches('}')
    );
    assert!(decode::<SessionView>(duplicate.as_bytes()).is_err());

    let mut authority: Map<String, Value> = serde_json::from_slice(SESSION_FIXTURE).unwrap();
    authority["authority"]["prompt_write_authorized"] = Value::Bool(true);
    let value: SessionView = decode(&serde_json::to_vec(&authority).unwrap()).unwrap();
    assert!(!session_is_display_only(&value));

    let mut foreign: Map<String, Value> = serde_json::from_slice(RESOURCE_FIXTURE).unwrap();
    foreign["devices"][0]["owner"]["subject"] = Value::String("other-user".into());
    let value: ResourceView = decode(&serde_json::to_vec(&foreign).unwrap()).unwrap();
    assert!(!resource_is_display_only(&value));
}
