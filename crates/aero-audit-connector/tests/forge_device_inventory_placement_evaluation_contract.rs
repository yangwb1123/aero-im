//! The persisted-inventory placement evaluation is a deterministic, offline
//! value. It carries no inventory, selection, reservation, dispatch,
//! execution, or Audit authority.

use serde::Deserialize;

const FIXTURE: &[u8] =
    include_bytes!("testdata/forge-device-inventory-placement-evaluation-v1.json");
const SCHEMA_VERSION: &str = "forge.device-inventory-placement-evaluation/v1";
const EVALUATION_MODE: &str = "pure_persisted_inventory_offline_evaluation";
const SOURCE_FIXTURE: &str = "forge-device-inventory-placement-input-v1.json";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Debug, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
struct GpuRequirement {
    required: bool,
    min_memory_bytes: u64,
    runtime: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
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

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
struct Authority {
    placement_evaluated: bool,
    placement_selected: bool,
    reservation_created: bool,
    execution_authorized: bool,
    dispatch_performed: bool,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Expected {
    accepted: bool,
    error: String,
    revision: u64,
    device_id: String,
    instance_id: String,
    matches_requirements: bool,
    exclusion_reasons: Vec<String>,
    owner_declaration_unverified: bool,
    device_attributes_unverified: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: String,
    evaluation_mode: String,
    source_fixture: String,
    source_case: String,
    evaluated_at_ms: u64,
    policy_requirements: Requirements,
    authority: Authority,
    expected: Expected,
}

fn decode(raw: &[u8]) -> Result<Fixture, serde_json::Error> {
    serde_json::from_slice(raw)
}

fn validate(fixture: &Fixture) {
    assert_eq!(fixture.schema_version, SCHEMA_VERSION);
    assert_eq!(fixture.evaluation_mode, EVALUATION_MODE);
    assert_eq!(fixture.source_fixture, SOURCE_FIXTURE);
    assert_eq!(fixture.source_case, "online");
    assert_eq!(fixture.evaluated_at_ms, 200_500);
    assert!(fixture.evaluated_at_ms <= MAX_SAFE_INTEGER);
    assert_eq!(fixture.authority, Authority::default());
    assert_eq!(
        fixture.policy_requirements,
        Requirements {
            os: "linux".into(),
            architecture: "amd64".into(),
            min_cpu_cores: 4,
            min_memory_bytes: 4096,
            min_storage_bytes: 1024,
            runtime: "oci".into(),
            gpu: GpuRequirement::default(),
            data_residency_zones: vec!["us-west".into()],
            minimum_trust_zone: "standard".into(),
            sandbox_floor: "container".into(),
            concurrency_slots: 1,
        }
    );
    assert_eq!(
        fixture.expected,
        Expected {
            accepted: true,
            error: String::new(),
            revision: 7,
            device_id: "device-a".into(),
            instance_id: "runner-a".into(),
            matches_requirements: false,
            exclusion_reasons: vec![
                "concurrency_capacity_insufficient".into(),
                "data_residency_zone_mismatch".into(),
                "sandbox_floor_unmet".into(),
                "trust_zone_unconfirmed".into(),
            ],
            owner_declaration_unverified: true,
            device_attributes_unverified: true,
        }
    );
}

fn with_root_suffix(raw: &str, suffix: &str) -> String {
    let end = raw.rfind('}').expect("root object");
    format!("{}{}", &raw[..end], suffix)
}

fn is_valid(fixture: &Fixture) -> bool {
    std::panic::catch_unwind(|| validate(fixture)).is_ok()
}

#[test]
fn canonical_placement_evaluation_is_strict_and_authority_free() {
    let fixture = decode(FIXTURE).expect("decode placement evaluation fixture");
    validate(&fixture);

    let raw = std::str::from_utf8(FIXTURE).expect("UTF-8 fixture");
    let unknown = with_root_suffix(raw, ",\"unexpected\":true}");
    assert!(decode(unknown.as_bytes()).is_err());

    let authority = raw.replacen(
        "\"dispatch_performed\": false",
        "\"dispatch_performed\": true",
        1,
    );
    let authority_fixture = decode(authority.as_bytes()).expect("authority mutation remains JSON");
    assert!(!is_valid(&authority_fixture));

    let source = raw.replacen(
        "\"source_case\": \"online\"",
        "\"source_case\": \"stale\"",
        1,
    );
    let source_fixture = decode(source.as_bytes()).expect("source mutation remains JSON");
    assert!(!is_valid(&source_fixture));

    let duplicate = raw.replacen(
        "\"schema_version\": \"forge.device-inventory-placement-evaluation/v1\",",
        "\"schema_version\": \"forge.device-inventory-placement-evaluation/v1\",\"schema_version\": \"forge.device-inventory-placement-evaluation/v1\",",
        1,
    );
    assert!(decode(duplicate.as_bytes()).is_err());
    assert!(decode(format!("{} true", raw).as_bytes()).is_err());
}
