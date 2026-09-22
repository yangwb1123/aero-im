//! The placement batch is a caller-declared offline comparison. The receiver
//! preserves its cases for compatibility but never treats them as inventory
//! authority, target selection, reservation, scheduling, dispatch, execution,
//! or an Audit fact.

use serde::Deserialize;
use std::collections::BTreeSet;

const FIXTURE: &[u8] =
    include_bytes!("testdata/forge-device-inventory-placement-batch-evaluation-v1.json");
const SCHEMA_VERSION: &str = "forge.device-inventory-placement-batch-evaluation/v1";
const EVALUATION_MODE: &str = "pure_persisted_inventory_placement_dry_run";
const SOURCE_FIXTURE: &str = "forge-device-inventory-placement-input-v1.json";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Owner {
    issuer: String,
    subject: String,
    tenant_id: String,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq, Default)]
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
    device_id: String,
    instance_id: String,
    matches_requirements: bool,
    exclusion_reasons: Vec<String>,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct BatchCase {
    name: String,
    source_case: String,
    device_id: String,
    instance_id: String,
    #[serde(default)]
    snapshot_observed_at_ms: Option<u64>,
    #[serde(default)]
    capability_lease_expires_at_ms: Option<u64>,
    expected: Expected,
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

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ErrorCase {
    name: String,
    error: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: String,
    evaluation_mode: String,
    source_fixture: String,
    evaluation_owner: Owner,
    evaluated_at_ms: u64,
    requirements: Requirements,
    cases: Vec<BatchCase>,
    empty_inputs_allowed: bool,
    selected_device_id: Option<String>,
    selected_instance_id: Option<String>,
    authority: Authority,
    error_cases: Vec<ErrorCase>,
}

fn decode(raw: &[u8]) -> Result<Fixture, serde_json::Error> {
    serde_json::from_slice(raw)
}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
        })
}

fn validate(fixture: &Fixture) {
    assert_eq!(fixture.schema_version, SCHEMA_VERSION);
    assert_eq!(fixture.evaluation_mode, EVALUATION_MODE);
    assert_eq!(fixture.source_fixture, SOURCE_FIXTURE);
    assert_eq!(fixture.evaluated_at_ms, 200_500);
    assert!(fixture.evaluated_at_ms <= MAX_SAFE_INTEGER);
    assert_eq!(
        fixture.evaluation_owner,
        Owner {
            issuer: "https://id.example".into(),
            subject: "user-1".into(),
            tenant_id: "tenant-1".into(),
        }
    );
    assert_eq!(
        fixture.requirements,
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
    assert!(fixture.empty_inputs_allowed);
    assert_eq!(fixture.selected_device_id, None);
    assert_eq!(fixture.selected_instance_id, None);
    assert_eq!(fixture.authority, Authority::default());
    assert_eq!(fixture.cases.len(), 6);

    let mut devices = BTreeSet::new();
    let mut instances = BTreeSet::new();
    for (index, item) in fixture.cases.iter().enumerate() {
        assert!(valid_token(&item.name), "case {index}");
        assert!(valid_token(&item.source_case), "source case {index}");
        assert!(valid_token(&item.device_id), "device {index}");
        assert!(valid_token(&item.instance_id), "instance {index}");
        assert!(
            devices.insert(item.device_id.clone()),
            "duplicate device {index}"
        );
        assert!(
            instances.insert(item.instance_id.clone()),
            "duplicate instance {index}"
        );
        assert_eq!(item.expected.revision, 7, "revision {index}");
        assert_eq!(
            item.expected.device_id, item.device_id,
            "device binding {index}"
        );
        assert_eq!(
            item.expected.instance_id, item.instance_id,
            "instance binding {index}"
        );
        assert!(!item.expected.matches_requirements, "selection {index}");
        assert!(
            !item.expected.exclusion_reasons.is_empty(),
            "reasons {index}"
        );
        assert!(item
            .expected
            .exclusion_reasons
            .windows(2)
            .all(|pair| pair[0] < pair[1]));
        assert!(item
            .expected
            .exclusion_reasons
            .iter()
            .all(|reason| valid_token(reason)));
    }
    assert_eq!(
        fixture
            .error_cases
            .iter()
            .map(|case| (case.name.as_str(), case.error.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("owner_mismatch", "owner_mismatch"),
            (
                "duplicate_device",
                "invalid_persisted_inventory_placement_input"
            ),
            (
                "duplicate_instance",
                "invalid_persisted_inventory_placement_input"
            ),
            (
                "invalid_evaluated_at",
                "invalid_persisted_inventory_placement_input"
            ),
        ]
    );
}

fn with_root_suffix(raw: &str, suffix: &str) -> String {
    let end = raw.rfind('}').expect("root object");
    format!("{}{}", &raw[..end], suffix)
}

#[test]
fn canonical_placement_batch_is_strict_and_authority_free() {
    let fixture = decode(FIXTURE).expect("decode placement batch fixture");
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
    assert_ne!(authority_fixture.authority, Authority::default());

    let selected = raw.replacen(
        "\"selected_device_id\": null",
        "\"selected_device_id\": \"device-a\"",
        1,
    );
    let selected_fixture = decode(selected.as_bytes()).expect("selected mutation remains JSON");
    assert!(selected_fixture.selected_device_id.is_some());

    let duplicate = raw.replacen(
        "\"schema_version\": \"forge.device-inventory-placement-batch-evaluation/v1\",",
        "\"schema_version\": \"forge.device-inventory-placement-batch-evaluation/v1\",\"schema_version\": \"forge.device-inventory-placement-batch-evaluation/v1\",",
        1,
    );
    assert!(decode(duplicate.as_bytes()).is_err());
    assert!(decode(format!("{} true", raw).as_bytes()).is_err());
}
