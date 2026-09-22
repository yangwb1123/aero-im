//! The P3b lifecycle image is an interoperability value contract only. It is
//! never an enrollment, heartbeat listener, inventory store, or authority.

use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};
use std::fmt;

const FIXTURE: &[u8] =
    include_bytes!("testdata/forge-device-enrollment-heartbeat-lifecycle-persistence-v1.json");

#[derive(Debug, Deserialize, PartialEq, Eq)]
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
    challenge_consumed: bool,
    heartbeat_persisted: bool,
    inventory_authoritative: bool,
    reservation_created: bool,
    execution_authorized: bool,
    dispatch_performed: bool,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Device {
    device_id: String,
    approval_state: String,
    credential_state: String,
    cordon_state: String,
    reservation_state: String,
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
struct Gpu {
    id: String,
    vendor: String,
    memory_bytes: u64,
    available_memory_bytes: u64,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Heartbeat {
    revision: u64,
    device_id: String,
    instance_id: String,
    generation: u64,
    heartbeat_sequence: u64,
    server_observed_at_ms: u64,
    capability_lease_expires_at_ms: u64,
    liveness: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Inventory {
    revision: u64,
    device_id: String,
    runner_device_id: String,
    runner_instance_id: String,
    runner_generation: u64,
    runner_heartbeat_sequence: u64,
    runner_server_observed_at_ms: u64,
    runner_capability_lease_expires_at_ms: u64,
    runner_liveness: String,
    reserved: bool,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedImage {
    accepted: bool,
    revision: u64,
    heartbeat_revision: u64,
    inventory_revision: u64,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Image {
    name: String,
    revision: u64,
    heartbeat: Heartbeat,
    inventory: Inventory,
    expected: ExpectedImage,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedCase {
    accepted: bool,
    error: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    outer_revision: u64,
    heartbeat_revision: u64,
    inventory_revision: u64,
    runner_device_id: Option<String>,
    runner_generation: Option<u64>,
    owner_subject: Option<String>,
    expected: ExpectedCase,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: String,
    evaluation_mode: String,
    notice: String,
    authority: Authority,
    owner: Owner,
    device: Device,
    capabilities: Capabilities,
    images: Vec<Image>,
    cases: Vec<Case>,
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

#[test]
fn canonical_lifecycle_persistence_image_is_bound_and_authority_free() {
    let fixture: Fixture = decode(FIXTURE).expect("lifecycle persistence fixture");
    assert_eq!(
        fixture.schema_version,
        "forge.device-enrollment-heartbeat-lifecycle-persistence/v1"
    );
    assert_eq!(fixture.evaluation_mode, "pure_joined_value_replacement");
    assert!(!fixture.notice.is_empty());
    assert!(authority_free(&fixture));
    assert!(
        !fixture.owner.issuer.is_empty()
            && !fixture.owner.subject.is_empty()
            && !fixture.owner.tenant_id.is_empty()
    );
    assert_eq!(fixture.device.approval_state, "approved");
    assert_eq!(fixture.device.credential_state, "active");
    assert_eq!(fixture.images.len(), 2);
    for image in &fixture.images {
        assert!(image.revision > 0);
        assert_eq!(image.revision, image.heartbeat.revision);
        assert_eq!(image.revision, image.inventory.revision);
        assert_eq!(image.revision, image.expected.revision);
        assert_eq!(image.revision, image.expected.heartbeat_revision);
        assert_eq!(image.revision, image.expected.inventory_revision);
        assert_eq!(image.heartbeat.device_id, fixture.device.device_id);
        assert_eq!(image.inventory.device_id, fixture.device.device_id);
        assert_eq!(image.inventory.runner_device_id, image.heartbeat.device_id);
        assert_eq!(
            image.inventory.runner_instance_id,
            image.heartbeat.instance_id
        );
        assert_eq!(
            image.inventory.runner_generation,
            image.heartbeat.generation
        );
        assert_eq!(
            image.inventory.runner_heartbeat_sequence,
            image.heartbeat.heartbeat_sequence
        );
        assert!(image.heartbeat.generation > 0 && image.heartbeat.heartbeat_sequence > 0);
        assert!(
            image.heartbeat.capability_lease_expires_at_ms > image.heartbeat.server_observed_at_ms
        );
        assert!(!image.inventory.reserved);
    }
    assert_eq!(fixture.cases.len(), 7);
    assert!(fixture.cases.iter().all(|case| !case.name.is_empty()
        && !case.expected.accepted
        && !case.expected.error.is_empty()));
}

#[test]
fn lifecycle_persistence_rejects_unknown_duplicate_and_authority_mutation() {
    let mut unknown: Map<String, Value> = serde_json::from_slice(FIXTURE).unwrap();
    unknown.insert("unexpected".into(), Value::Bool(true));
    assert!(decode::<Fixture>(&serde_json::to_vec(&unknown).unwrap()).is_err());
    let mut authority: Map<String, Value> = serde_json::from_slice(FIXTURE).unwrap();
    authority["authority"]["inventory_authoritative"] = Value::Bool(true);
    let value: Fixture = decode(&serde_json::to_vec(&authority).unwrap()).unwrap();
    assert!(!authority_free(&value));
    let duplicate = format!(
        "{},\"schema_version\":\"forge.device-enrollment-heartbeat-lifecycle-persistence/v1\"}}",
        String::from_utf8_lossy(FIXTURE).trim_end_matches('}')
    );
    assert!(decode::<Fixture>(duplicate.as_bytes()).is_err());
}
