//! The persisted inventory to placement-input value is a pure, unverified
//! boundary. This receiver preserves its state/error cases without granting
//! inventory, selection, reservation, scheduling, dispatch, execution, or
//! Audit authority.

use serde::Deserialize;
use std::collections::BTreeSet;

const FIXTURE: &[u8] = include_bytes!("testdata/forge-device-inventory-placement-input-v1.json");
const SCHEMA_VERSION: &str = "forge.device-inventory-placement-input/v1";
const EVALUATION_MODE: &str = "pure_persisted_inventory_to_placement_input";

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Owner {
    issuer: String,
    subject: String,
    tenant_id: String,
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
struct Policy {
    data_residency_zones: Vec<String>,
    minimum_trust_zone: String,
    sandbox_floor: String,
    concurrency_slots: u64,
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

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Device {
    device_id: String,
    owner: Owner,
    approval_state: String,
    cordon_state: String,
    reservation_state: String,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Runner {
    device_id: String,
    instance_id: String,
    generation: u64,
    heartbeat_sequence: u64,
    server_observed_at_ms: u64,
    capability_lease_expires_at_ms: u64,
    liveness: String,
    capabilities: Capabilities,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct State {
    revision: u64,
    device: Device,
    runner: Runner,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
#[serde(default)]
struct Expected {
    accepted: bool,
    error: String,
    revision: u64,
    device_id: String,
    instance_id: String,
    generation: u64,
    heartbeat_sequence: u64,
    approval_state: String,
    cordon_state: String,
    reservation_state: String,
    liveness: String,
    snapshot_observed_at_ms: i64,
    lease_expires_at_ms: i64,
    owner_declaration_unverified: bool,
    policy_attributes_unverified: bool,
    data_residency_zones: Vec<String>,
    trust_zone: String,
    sandbox_levels: Vec<String>,
    concurrency_limit: u64,
    active_concurrency: u64,
    policy_requirements_met: bool,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    #[serde(default)]
    evaluation_owner: Option<Owner>,
    #[serde(default)]
    runner_device_id: String,
    #[serde(default)]
    approval_state: String,
    #[serde(default)]
    cordon_state: String,
    #[serde(default)]
    liveness: String,
    #[serde(default)]
    server_observed_at_ms: Option<u64>,
    #[serde(default)]
    capability_lease_expires_at_ms: Option<u64>,
    expected: Expected,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: String,
    evaluation_mode: String,
    evaluation_owner: Owner,
    policy_requirements: Policy,
    authority: Authority,
    state: State,
    cases: Vec<Case>,
}

fn decode(raw: &[u8]) -> Result<Fixture, serde_json::Error> {
    serde_json::from_slice(raw)
}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

fn validate(fixture: &Fixture) {
    let owner = Owner {
        issuer: "https://id.example".into(),
        subject: "user-1".into(),
        tenant_id: "tenant-1".into(),
    };
    assert_eq!(fixture.schema_version, SCHEMA_VERSION);
    assert_eq!(fixture.evaluation_mode, EVALUATION_MODE);
    assert_eq!(fixture.evaluation_owner, owner);
    assert_eq!(fixture.authority, Authority::default());
    assert_eq!(
        fixture.policy_requirements,
        Policy {
            data_residency_zones: vec!["us-west".into()],
            minimum_trust_zone: "standard".into(),
            sandbox_floor: "container".into(),
            concurrency_slots: 1,
        }
    );
    assert_eq!(fixture.cases.len(), 12);
    assert_eq!(fixture.state.revision, 7);
    assert_eq!(fixture.state.device.device_id, "device-a");
    assert_eq!(fixture.state.device.owner, owner);
    assert_eq!(fixture.state.device.approval_state, "approved");
    assert_eq!(fixture.state.device.cordon_state, "clear");
    assert_eq!(fixture.state.device.reservation_state, "none");
    assert_eq!(fixture.state.runner.device_id, "device-a");
    assert_eq!(fixture.state.runner.instance_id, "runner-a");
    assert_eq!(fixture.state.runner.generation, 3);
    assert_eq!(fixture.state.runner.heartbeat_sequence, 12);
    assert_eq!(fixture.state.runner.server_observed_at_ms, 200_000);
    assert_eq!(fixture.state.runner.capability_lease_expires_at_ms, 260_000);
    assert_eq!(fixture.state.runner.liveness, "online");
    assert_eq!(
        fixture.state.runner.capabilities,
        Capabilities {
            os: "linux".into(),
            architecture: "amd64".into(),
            cpu_cores: 8,
            available_cpu_cores: 6,
            memory_bytes: 16_384,
            available_memory_bytes: 12_288,
            storage_bytes: 8_192,
            available_storage_bytes: 4_096,
            gpus: vec![],
            runtimes: vec!["oci".into()],
        }
    );

    let names = [
        "online",
        "stale",
        "expired",
        "pending",
        "cordoned",
        "revoked",
        "offline",
        "owner_mismatch",
        "runner_device_binding_mismatch",
        "missing_policy_attributes_fail_closed",
        "unsafe_snapshot_observed_at_ms",
        "unsafe_capability_lease_expires_at_ms",
    ];
    let mut seen = BTreeSet::new();
    for (index, item) in fixture.cases.iter().enumerate() {
        assert_eq!(item.name, names[index]);
        assert!(valid_token(&item.name));
        assert!(seen.insert(item.name.clone()));
        let accepted = index < 7 || index == 9;
        if accepted {
            let approval = if item.approval_state.is_empty() {
                "approved"
            } else {
                item.approval_state.as_str()
            };
            let cordon = if item.cordon_state.is_empty() {
                "clear"
            } else {
                item.cordon_state.as_str()
            };
            let liveness = if item.liveness.is_empty() {
                "online"
            } else {
                item.liveness.as_str()
            };
            let observed = item
                .server_observed_at_ms
                .unwrap_or(fixture.state.runner.server_observed_at_ms);
            let lease = item
                .capability_lease_expires_at_ms
                .unwrap_or(fixture.state.runner.capability_lease_expires_at_ms);
            let expected = &item.expected;
            assert!(expected.accepted);
            assert!(expected.error.is_empty());
            assert_eq!(expected.revision, 7);
            assert_eq!(expected.device_id, "device-a");
            assert_eq!(expected.instance_id, "runner-a");
            assert_eq!(expected.generation, 3);
            assert_eq!(expected.heartbeat_sequence, 12);
            assert_eq!(expected.approval_state, approval);
            assert_eq!(expected.cordon_state, cordon);
            assert_eq!(expected.reservation_state, "none");
            assert_eq!(expected.liveness, liveness);
            assert_eq!(expected.snapshot_observed_at_ms, observed as i64);
            assert_eq!(expected.lease_expires_at_ms, lease as i64);
            assert!(expected.owner_declaration_unverified);
            assert!(expected.policy_attributes_unverified);
            assert!(expected.data_residency_zones.is_empty());
            assert_eq!(expected.trust_zone, "unknown");
            assert!(expected.sandbox_levels.is_empty());
            assert_eq!(expected.concurrency_limit, 0);
            assert_eq!(expected.active_concurrency, 0);
            assert!(!expected.policy_requirements_met);
        } else {
            let error = match index {
                7 => "owner_mismatch",
                8 => "runner_device_mismatch",
                _ => "invalid_persisted_inventory_placement_input",
            };
            assert!(!item.expected.accepted);
            assert_eq!(item.expected.error, error);
            assert_eq!(item.expected.revision, 0);
            assert!(item.expected.device_id.is_empty());
            assert!(item.expected.instance_id.is_empty());
        }
    }
}

fn with_root_suffix(raw: &str, suffix: &str) -> String {
    let end = raw.rfind('}').expect("root object");
    format!("{}{}", &raw[..end], suffix)
}

#[test]
fn canonical_placement_input_is_strict_and_authority_free() {
    let fixture = decode(FIXTURE).expect("decode placement input fixture");
    validate(&fixture);
    let raw = std::str::from_utf8(FIXTURE).expect("UTF-8 fixture");

    let unknown = with_root_suffix(raw, ",\"unexpected\":true}");
    assert!(decode(unknown.as_bytes()).is_err());

    let authority = raw.replacen(
        "\"dispatch_performed\": false",
        "\"dispatch_performed\": true",
        1,
    );
    let authority_fixture = decode(authority.as_bytes()).expect("authority JSON");
    assert_ne!(authority_fixture.authority, Authority::default());

    let owner = raw.replacen("\"subject\": \"user-1\"", "\"subject\": \"other-user\"", 1);
    let owner_fixture = decode(owner.as_bytes()).expect("owner JSON");
    assert_ne!(owner_fixture.evaluation_owner.subject, "user-1");

    let duplicate = raw.replacen(
        "\"schema_version\": \"forge.device-inventory-placement-input/v1\",",
        "\"schema_version\": \"forge.device-inventory-placement-input/v1\",\"schema_version\": \"forge.device-inventory-placement-input/v1\",",
        1,
    );
    assert!(decode(duplicate.as_bytes()).is_err());
    assert!(decode(format!("{} true", raw).as_bytes()).is_err());
}
