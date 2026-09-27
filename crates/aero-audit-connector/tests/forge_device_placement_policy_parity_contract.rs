//! Device placement policy parity is a strict, read-only owner-bound value.
//! It evaluates persisted declarations only; it carries no selection,
//! reservation, dispatch, lease, Runner, or execution authority.

use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::collections::BTreeSet;
use std::fmt;

const FIXTURE: &[u8] = include_bytes!("testdata/forge-device-placement-policy-parity-v1.json");
const SCHEMA_VERSION: &str = "forge.device-placement-policy-parity-test/v1";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct Owner {
    issuer: String,
    subject: String,
    tenant_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct GpuRequirement {
    required: bool,
    min_memory_bytes: u64,
    runtime: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct GpuDeclaration {
    present: bool,
    memory_bytes: u64,
    runtime: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
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
    gpu: GpuDeclaration,
    data_residency_zones: Vec<String>,
    trust_zone: String,
    sandbox_levels: Vec<String>,
    concurrency_limit: u64,
    active_concurrency: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct Candidate {
    instance_id: String,
    device: Device,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct Expected {
    device_id: String,
    matches_requirements: bool,
    exclusion_reasons: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: String,
    evaluated_at_ms: u64,
    max_snapshot_age_ms: u64,
    owner: Owner,
    requirements: Requirements,
    candidates: Vec<Candidate>,
    expected: Vec<Expected>,
}

fn decode<T: DeserializeOwned>(raw: &[u8]) -> Result<T, String> {
    reject_duplicate_json_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value = T::deserialize(&mut decoder).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    Ok(value)
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

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
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

fn validate(fixture: &Fixture) -> Result<(), &'static str> {
    if fixture.schema_version != SCHEMA_VERSION
        || fixture.evaluated_at_ms > MAX_SAFE_INTEGER
        || fixture.max_snapshot_age_ms == 0
        || !valid_owner(&fixture.owner)
        || fixture.candidates.len() != 9
        || fixture.expected.len() != 9
    {
        return Err("invalid policy parity envelope");
    }

    if fixture.requirements
        != (Requirements {
            os: "linux".into(),
            architecture: "x86_64".into(),
            min_cpu_cores: 4,
            min_memory_bytes: 8192,
            min_storage_bytes: 4096,
            runtime: "oci".into(),
            gpu: GpuRequirement {
                required: false,
                min_memory_bytes: 0,
                runtime: String::new(),
            },
            data_residency_zones: vec!["us-west".into()],
            minimum_trust_zone: "standard".into(),
            sandbox_floor: "container".into(),
            concurrency_slots: 1,
        })
    {
        return Err("invalid policy requirements");
    }

    let instances = [
        "runner-a", "runner-b", "runner-c", "runner-d", "runner-e", "runner-f", "runner-g",
        "runner-h", "runner-i",
    ];
    let devices = [
        "candidate-a",
        "candidate-b",
        "candidate-c",
        "candidate-d",
        "candidate-e",
        "candidate-f",
        "candidate-g",
        "candidate-h",
        "candidate-i",
    ];
    let matches = [true, false, false, false, false, false, false, false, true];
    let reasons: [&[&str]; 9] = [
        &[],
        &[
            "concurrency_capacity_insufficient",
            "cpu_cores_insufficient",
            "data_residency_zone_mismatch",
            "memory_insufficient",
            "runtime_missing",
            "sandbox_floor_unmet",
            "storage_insufficient",
            "trust_zone_below_minimum",
        ],
        &["approval_pending"],
        &["device_revoked"],
        &["device_cordoned"],
        &["declared_offline"],
        &["snapshot_declared_from_future"],
        &["declared_lease_expired", "snapshot_stale"],
        &[],
    ];
    let mut seen_instances = BTreeSet::new();
    let mut seen_devices = BTreeSet::new();
    for (index, candidate) in fixture.candidates.iter().enumerate() {
        if candidate.instance_id != instances[index]
            || candidate.device.device_id != devices[index]
            || candidate.device.owner != fixture.owner
            || !valid_owner(&candidate.device.owner)
            || !seen_instances.insert(candidate.instance_id.clone())
            || !seen_devices.insert(candidate.device.device_id.clone())
            || !valid_device(&candidate.device)
        {
            return Err("invalid candidate binding");
        }
        let expected = &fixture.expected[index];
        if expected.device_id != candidate.device.device_id
            || expected.matches_requirements != matches[index]
            || expected.exclusion_reasons
                != reasons[index]
                    .iter()
                    .map(|reason| (*reason).to_string())
                    .collect::<Vec<_>>()
        {
            return Err("invalid expected placement result");
        }
    }
    Ok(())
}

fn valid_device(device: &Device) -> bool {
    valid_identifier(&device.device_id)
        && device.snapshot_observed_at_ms <= MAX_SAFE_INTEGER
        && device.lease_expires_at_ms <= MAX_SAFE_INTEGER
        && device.available_cpu_cores <= MAX_SAFE_INTEGER
        && device.available_memory_bytes <= MAX_SAFE_INTEGER
        && device.available_storage_bytes <= MAX_SAFE_INTEGER
        && device.concurrency_limit > 0
        && device.active_concurrency <= device.concurrency_limit
        && !device.runtimes.is_empty()
        && !device.data_residency_zones.is_empty()
        && !device.sandbox_levels.is_empty()
}

fn valid_owner(owner: &Owner) -> bool {
    valid_owner_part(&owner.issuer)
        && valid_owner_part(&owner.subject)
        && valid_owner_part(&owner.tenant_id)
}

fn valid_owner_part(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && value.trim() == value
        && !value
            .chars()
            .any(|character| character == '\0' || character == '\r' || character == '\n')
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.chars().enumerate().all(|(index, character)| {
            character.is_ascii_alphanumeric()
                || (index > 0 && matches!(character, '.' | '_' | ':' | '+' | '/' | '-'))
        })
}

fn with_root_suffix(raw: &str, suffix: &str) -> String {
    let end = raw.rfind('}').expect("root object");
    format!("{}{}", &raw[..end], suffix)
}

#[test]
fn canonical_policy_parity_is_strict_owner_bound_and_authority_free() {
    let fixture = decode::<Fixture>(FIXTURE).expect("decode placement policy parity fixture");
    validate(&fixture).expect("validate placement policy parity fixture");

    let raw = std::str::from_utf8(FIXTURE).expect("UTF-8 fixture");
    let unknown = with_root_suffix(raw, ",\"authority\":{\"execution_authorized\":true}}");
    assert!(decode::<Fixture>(unknown.as_bytes()).is_err());

    let duplicate = raw.replacen(
        "\"schema_version\": \"forge.device-placement-policy-parity-test/v1\",",
        "\"schema_version\": \"forge.device-placement-policy-parity-test/v1\",\"schema_version\": \"forge.device-placement-policy-parity-test/v1\",",
        1,
    );
    assert!(decode::<Fixture>(duplicate.as_bytes()).is_err());
    assert!(decode::<Fixture>(format!("{} {{}}", raw).as_bytes()).is_err());
}

#[test]
fn policy_parity_rejects_owner_and_binding_mutations() {
    let fixture = decode::<Fixture>(FIXTURE).expect("decode placement policy parity fixture");

    let mut root_owner = fixture.clone();
    root_owner.owner.subject = "user-foreign".into();
    assert!(validate(&root_owner).is_err());

    let mut candidate_owner = fixture.clone();
    candidate_owner.candidates[1].device.owner.subject = "user-foreign".into();
    assert!(validate(&candidate_owner).is_err());

    let mut instance_binding = fixture.clone();
    instance_binding.candidates[0].instance_id.clear();
    assert!(validate(&instance_binding).is_err());

    let mut device_binding = fixture.clone();
    device_binding.candidates[0].device.device_id = "candidate-foreign".into();
    assert!(validate(&device_binding).is_err());

    let mut expected_device = fixture.clone();
    expected_device.expected[0].device_id = "candidate-foreign".into();
    assert!(validate(&expected_device).is_err());

    let mut expected_match = fixture.clone();
    expected_match.expected[0].matches_requirements = false;
    assert!(validate(&expected_match).is_err());

    let mut reason_drift = fixture;
    reason_drift.expected[1].exclusion_reasons[0] = "unexpected_reason".into();
    assert!(validate(&reason_drift).is_err());
}
