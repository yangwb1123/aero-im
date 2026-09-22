//! The v2 placement comparison is a strict, offline value. It keeps the
//! lossless observation and deterministic decisions visible without granting
//! identity, inventory, selection, reservation, scheduling, dispatch,
//! execution, or Audit authority.

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::collections::BTreeSet;
use std::fmt;

const FIXTURE: &[u8] =
    include_bytes!("testdata/forge-device-inventory-placement-evaluation-v2.json");
const SCHEMA_VERSION: &str = "forge.device-inventory-placement-evaluation/v2";
const EVALUATION_MODE: &str = "offline_static_only";
const SOURCE_SCHEMA_VERSION: &str = "forge.device-inventory-observation/v2";
const NOTICE: &str = "Every owner, state, timestamp, resource, GPU, reservation, residency, trust, sandbox, and concurrency value is an unverified caller declaration. This read-only comparison selects no target and grants no execution authority.";
const OBSERVATION_NOTICE: &str = "Every owner, instance, state, timestamp, resource, GPU, reservation, residency, trust, sandbox, and concurrency value is an unverified caller declaration. This read-only observation selects no target and grants no execution authority.";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

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
struct Observation {
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

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct GpuRequirement {
    required: bool,
    min_memory_bytes: u64,
    runtime: String,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Requirements {
    os: String,
    architecture: String,
    min_cpu_cores: u64,
    min_memory_bytes: u64,
    min_storage_bytes: u64,
    runtime: String,
    gpu: GpuRequirement,
    data_residency_zones: Vec<String>,
    minimum_trust_zone: String,
    sandbox_floor: String,
    concurrency_slots: u64,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Expected {
    revision: u64,
    generation: u64,
    heartbeat_sequence: u64,
    device_id: String,
    instance_id: String,
    reservation_state: String,
    gpu_count: u64,
    available_gpu_memory_bytes: u64,
    matches_requirements: bool,
    exclusion_reasons: Vec<String>,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
struct Authority {
    identity_verified: bool,
    heartbeat_persisted: bool,
    inventory_authoritative: bool,
    placement_selected: bool,
    reservation_created: bool,
    execution_authorized: bool,
    dispatch_performed: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: String,
    evaluation_mode: String,
    source_schema_version: String,
    evaluation_owner: Owner,
    evaluated_at_ms: u64,
    notice: String,
    requirements: Requirements,
    observation: Observation,
    expected: Vec<Expected>,
    eligible_candidate_count: u64,
    selected_device_id: Option<String>,
    selected_instance_id: Option<String>,
    authority: Authority,
}

fn decode<T: serde::de::DeserializeOwned>(raw: &[u8]) -> Result<T, String> {
    reject_duplicate_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value = T::deserialize(&mut decoder).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    Ok(value)
}

fn validate(fixture: &Fixture) -> Result<(), String> {
    let owner = Owner {
        issuer: "issuer".into(),
        subject: "user".into(),
        tenant_id: "tenant".into(),
    };
    if fixture.schema_version != SCHEMA_VERSION
        || fixture.evaluation_mode != EVALUATION_MODE
        || fixture.source_schema_version != SOURCE_SCHEMA_VERSION
        || fixture.evaluation_owner != owner
        || fixture.evaluated_at_ms != 200_000
        || fixture.evaluated_at_ms > MAX_SAFE_INTEGER
        || fixture.notice != NOTICE
        || fixture.eligible_candidate_count != 0
        || fixture.selected_device_id.is_some()
        || fixture.selected_instance_id.is_some()
        || fixture.authority != Authority::default()
    {
        return Err("invalid v2 placement evaluation envelope".into());
    }
    if fixture.observation.schema_version != SOURCE_SCHEMA_VERSION
        || fixture.observation.evaluation_mode != EVALUATION_MODE
        || fixture.observation.evaluated_at_ms != fixture.evaluated_at_ms
        || fixture.observation.owner_declaration != owner
        || !fixture.observation.owner_declaration_unverified
        || !fixture.observation.inventory_declarations_unverified
        || fixture.observation.notice != OBSERVATION_NOTICE
        || fixture.observation.execution_authorized
        || fixture.observation.reservation_created
        || fixture.observation.dispatch_performed
        || fixture.observation.devices.len() != 2
    {
        return Err("invalid v2 placement observation".into());
    }
    if fixture.requirements
        != (Requirements {
            os: "linux".into(),
            architecture: "amd64".into(),
            min_cpu_cores: 4,
            min_memory_bytes: 4_294_967_296,
            min_storage_bytes: 10_737_418_240,
            runtime: "go".into(),
            gpu: GpuRequirement {
                required: true,
                min_memory_bytes: 4_294_967_296,
                runtime: String::new(),
            },
            data_residency_zones: vec!["us-west".into()],
            minimum_trust_zone: "standard".into(),
            sandbox_floor: "container".into(),
            concurrency_slots: 1,
        })
    {
        return Err("invalid v2 placement requirements".into());
    }
    let reasons = [
        vec![
            "concurrency_capacity_insufficient",
            "data_residency_zone_mismatch",
            "declared_lease_expired",
            "device_reserved",
            "sandbox_floor_unmet",
            "snapshot_stale",
            "trust_zone_unconfirmed",
        ],
        vec![
            "approval_pending",
            "concurrency_capacity_insufficient",
            "data_residency_zone_mismatch",
            "declared_lease_expired",
            "declared_offline",
            "device_cordoned",
            "gpu_missing",
            "sandbox_floor_unmet",
            "snapshot_stale",
            "trust_zone_unconfirmed",
        ],
    ];
    let expected_ids = [
        (
            "device-a",
            "runner-a",
            "reserved",
            1,
            1,
            1,
            2,
            17_179_869_184,
        ),
        ("device-b", "runner-b", "none", 2, 2, 4, 0, 0),
    ];
    if fixture.expected.len() != expected_ids.len() {
        return Err("invalid v2 placement decision count".into());
    }
    for (index, expected) in fixture.expected.iter().enumerate() {
        let (
            device_id,
            instance_id,
            reservation,
            revision,
            generation,
            heartbeat,
            gpu_count,
            gpu_bytes,
        ) = expected_ids[index];
        if expected.device_id != device_id
            || expected.instance_id != instance_id
            || expected.reservation_state != reservation
            || expected.revision != revision
            || expected.generation != generation
            || expected.heartbeat_sequence != heartbeat
            || expected.gpu_count != gpu_count
            || expected.available_gpu_memory_bytes != gpu_bytes
            || expected.matches_requirements
            || expected
                .exclusion_reasons
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                != reasons[index]
        {
            return Err(format!("invalid v2 placement decision {index}"));
        }
        let candidate = &fixture.observation.devices[index];
        if candidate.device.device_id != expected.device_id
            || candidate.instance_id != expected.instance_id
            || candidate.revision != expected.revision
            || candidate.generation != expected.generation
            || candidate.heartbeat_sequence != expected.heartbeat_sequence
            || candidate.device.reservation_state != expected.reservation_state
            || candidate.device.gpus.len() as u64 != expected.gpu_count
        {
            return Err(format!(
                "v2 placement decision {index} is not bound to observation"
            ));
        }
    }
    Ok(())
}

#[test]
fn canonical_v2_placement_evaluation_is_strict_and_authority_free() {
    let fixture: Fixture = decode(FIXTURE).expect("decode v2 placement evaluation");
    validate(&fixture).expect("validate v2 placement evaluation");

    let raw = std::str::from_utf8(FIXTURE).expect("UTF-8 fixture");
    let end = raw.rfind('}').expect("root object");
    let unknown = format!("{},\"unexpected\":true}}", &raw[..end]);
    assert!(decode::<Fixture>(unknown.as_bytes()).is_err());

    let authority = raw.replacen(
        "\"inventory_authoritative\": false",
        "\"inventory_authoritative\": true",
        1,
    );
    let authority_fixture: Fixture = decode(authority.as_bytes()).expect("authority mutation JSON");
    assert!(validate(&authority_fixture).is_err());

    let selection = raw.replacen(
        "\"selected_device_id\": null",
        "\"selected_device_id\": \"device-a\"",
        1,
    );
    let selection_fixture: Fixture = decode(selection.as_bytes()).expect("selection mutation JSON");
    assert!(validate(&selection_fixture).is_err());

    let binding = raw.replacen(
        "\"device_id\": \"device-a\"",
        "\"device_id\": \"device-c\"",
        1,
    );
    let binding_fixture: Fixture = decode(binding.as_bytes()).expect("binding mutation JSON");
    assert!(validate(&binding_fixture).is_err());

    let duplicate = raw.replacen(
        "\"schema_version\": \"forge.device-inventory-placement-evaluation/v2\",",
        "\"schema_version\": \"forge.device-inventory-placement-evaluation/v2\",\"schema_version\": \"forge.device-inventory-placement-evaluation/v2\",",
        1,
    );
    assert!(decode::<Fixture>(duplicate.as_bytes()).is_err());
    assert!(decode::<Fixture>(format!("{} true", raw).as_bytes()).is_err());
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
