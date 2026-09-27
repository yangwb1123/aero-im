//! Forge Run/Attempt/lease dispatch preflight requests are bounded archival
//! inputs. Aero-IM validates the complete shape without authenticating an
//! instance, selecting a target, issuing a lease, dispatching a Runner, or
//! granting execution/Audit authority.

use serde_json::{Map, Value};

#[path = "support/forge_duplicate_json_keys.rs"]
mod duplicate_json_keys;

const FIXTURE: &[u8] =
    include_bytes!("testdata/forge-run-attempt-lease-dispatch-preflight-request-v1.json");
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Debug, Eq, PartialEq)]
struct Owner {
    issuer: String,
    subject: String,
    tenant_id: String,
}

fn decode(raw: &[u8]) -> Result<Value, String> {
    duplicate_json_keys::reject_duplicate_json_keys(raw)?;
    let value: Value = serde_json::from_slice(raw).map_err(|error| error.to_string())?;
    validate(&value)?;
    Ok(value)
}

fn exact_object<'a>(value: &'a Value, expected: &[&str]) -> Result<&'a Map<String, Value>, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "expected JSON object".to_owned())?;
    if object.len() != expected.len() || expected.iter().any(|key| !object.contains_key(*key)) {
        return Err("unexpected or missing JSON object field".to_owned());
    }
    Ok(object)
}

fn string_field<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str, String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{key} must be a non-empty string"))
}

fn bool_field(object: &Map<String, Value>, key: &str, expected: bool) -> Result<(), String> {
    if object.get(key).and_then(Value::as_bool) == Some(expected) {
        Ok(())
    } else {
        Err(format!("{key} has an unexpected boolean"))
    }
}

fn integer_field(object: &Map<String, Value>, key: &str) -> Result<u64, String> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .filter(|value| *value <= MAX_SAFE_INTEGER)
        .ok_or_else(|| format!("{key} must be a safe unsigned integer"))
}

fn owner(value: &Value) -> Result<Owner, String> {
    let object = exact_object(value, &["issuer", "subject", "tenant_id"])?;
    Ok(Owner {
        issuer: string_field(object, "issuer")?.to_owned(),
        subject: string_field(object, "subject")?.to_owned(),
        tenant_id: string_field(object, "tenant_id")?.to_owned(),
    })
}

fn authority(value: &Value) -> Result<(), String> {
    let object = exact_object(
        value,
        &[
            "device_identity_verified",
            "command_persisted",
            "reservation_created",
            "execution_authorized",
            "dispatch_performed",
            "audit_published",
        ],
    )?;
    for key in [
        "device_identity_verified",
        "command_persisted",
        "reservation_created",
        "execution_authorized",
        "dispatch_performed",
        "audit_published",
    ] {
        bool_field(object, key, false)?;
    }
    Ok(())
}

fn validate(value: &Value) -> Result<(), String> {
    let root = exact_object(
        value,
        &[
            "owner",
            "conversation_id",
            "run_id",
            "run_status",
            "dispatch_plan",
        ],
    )?;
    let root_owner = owner(
        root.get("owner")
            .ok_or_else(|| "owner missing".to_owned())?,
    )?;
    if root_owner
        != (Owner {
            issuer: "https://id.example".into(),
            subject: "user-1".into(),
            tenant_id: "tenant-1".into(),
        })
        || string_field(root, "conversation_id")? != "conversation-001"
        || string_field(root, "run_id")? != "run-001"
        || string_field(root, "run_status")? != "nonterminal"
    {
        return Err("invalid owner or Run binding".into());
    }

    let plan = exact_object(
        root.get("dispatch_plan")
            .ok_or_else(|| "dispatch plan missing".to_owned())?,
        &[
            "attempt_state",
            "placement_request",
            "runner_execution_intent",
            "lease",
        ],
    )?;
    if string_field(plan, "attempt_state")? != "accepted" {
        return Err("invalid Attempt state".into());
    }
    validate_placement(
        plan.get("placement_request")
            .ok_or_else(|| "placement request missing".to_owned())?,
        &root_owner,
    )?;
    let intent = validate_intent(
        plan.get("runner_execution_intent")
            .ok_or_else(|| "Runner intent missing".to_owned())?,
        &root_owner,
        string_field(root, "conversation_id")?,
        string_field(root, "run_id")?,
    )?;
    validate_lease(
        plan.get("lease")
            .ok_or_else(|| "lease missing".to_owned())?,
        &intent,
    )
}

fn validate_placement(value: &Value, root_owner: &Owner) -> Result<(), String> {
    let placement = exact_object(
        value,
        &[
            "schema_version",
            "evaluated_at_ms",
            "owner",
            "max_snapshot_age_ms",
            "requirements",
            "devices",
        ],
    )?;
    if string_field(placement, "schema_version")? != "forge.device-placement-dry-run/v1"
        || integer_field(placement, "evaluated_at_ms")? == 0
        || owner(
            placement
                .get("owner")
                .ok_or_else(|| "placement owner missing".to_owned())?,
        )? != *root_owner
        || integer_field(placement, "max_snapshot_age_ms")? == 0
    {
        return Err("placement declaration crossed its archival boundary".into());
    }
    validate_requirements(
        placement
            .get("requirements")
            .ok_or_else(|| "placement requirements missing".to_owned())?,
    )?;
    let devices = placement
        .get("devices")
        .and_then(Value::as_array)
        .filter(|devices| !devices.is_empty())
        .ok_or_else(|| "placement devices missing".to_owned())?;
    for device in devices {
        validate_device(device, root_owner)?;
    }
    Ok(())
}

fn validate_requirements(value: &Value) -> Result<(), String> {
    let requirements = exact_object(
        value,
        &[
            "os",
            "architecture",
            "min_cpu_cores",
            "min_memory_bytes",
            "min_storage_bytes",
            "runtime",
            "gpu",
            "data_residency_zones",
            "minimum_trust_zone",
            "sandbox_floor",
            "concurrency_slots",
        ],
    )?;
    for key in [
        "os",
        "architecture",
        "runtime",
        "minimum_trust_zone",
        "sandbox_floor",
    ] {
        string_field(requirements, key)?;
    }
    for key in [
        "min_cpu_cores",
        "min_memory_bytes",
        "min_storage_bytes",
        "concurrency_slots",
    ] {
        integer_field(requirements, key)?;
    }
    validate_gpu(
        requirements
            .get("gpu")
            .ok_or_else(|| "requirement GPU missing".to_owned())?,
        false,
    )?;
    validate_string_array(
        requirements
            .get("data_residency_zones")
            .ok_or_else(|| "residency zones missing".to_owned())?,
        false,
    )
}

fn validate_device(value: &Value, root_owner: &Owner) -> Result<(), String> {
    let device = exact_object(
        value,
        &[
            "device_id",
            "owner",
            "approval_state",
            "cordon_state",
            "liveness",
            "snapshot_observed_at_ms",
            "lease_expires_at_ms",
            "os",
            "architecture",
            "available_cpu_cores",
            "available_memory_bytes",
            "available_storage_bytes",
            "runtimes",
            "gpu",
            "data_residency_zones",
            "trust_zone",
            "sandbox_levels",
            "concurrency_limit",
            "active_concurrency",
        ],
    )?;
    if string_field(device, "device_id")?.is_empty()
        || owner(
            device
                .get("owner")
                .ok_or_else(|| "device owner missing".to_owned())?,
        )? != *root_owner
    {
        return Err("device owner crossed the archival boundary".into());
    }
    for key in [
        "approval_state",
        "cordon_state",
        "liveness",
        "os",
        "architecture",
        "trust_zone",
    ] {
        string_field(device, key)?;
    }
    for key in [
        "snapshot_observed_at_ms",
        "lease_expires_at_ms",
        "available_cpu_cores",
        "available_memory_bytes",
        "available_storage_bytes",
        "concurrency_limit",
        "active_concurrency",
    ] {
        integer_field(device, key)?;
    }
    validate_string_array(
        device
            .get("runtimes")
            .ok_or_else(|| "device runtimes missing".to_owned())?,
        true,
    )?;
    validate_gpu(
        device
            .get("gpu")
            .ok_or_else(|| "device GPU missing".to_owned())?,
        true,
    )?;
    validate_string_array(
        device
            .get("data_residency_zones")
            .ok_or_else(|| "device residency zones missing".to_owned())?,
        true,
    )?;
    validate_string_array(
        device
            .get("sandbox_levels")
            .ok_or_else(|| "device sandbox levels missing".to_owned())?,
        true,
    )
}

fn validate_gpu(value: &Value, present_field: bool) -> Result<(), String> {
    let expected = if present_field {
        vec!["present", "memory_bytes", "runtime"]
    } else {
        vec!["required", "min_memory_bytes", "runtime"]
    };
    let gpu = exact_object(value, &expected)?;
    bool_field(gpu, expected[0], false)?;
    integer_field(gpu, expected[1])?;
    if gpu.get("runtime").and_then(Value::as_str).is_none() {
        return Err("gpu runtime must be a string".into());
    }
    Ok(())
}

fn validate_string_array(value: &Value, allow_empty: bool) -> Result<(), String> {
    let values = value
        .as_array()
        .filter(|values| allow_empty || !values.is_empty())
        .ok_or_else(|| "expected string array".to_owned())?;
    for value in values {
        if value.as_str().is_none() || (!allow_empty && value.as_str() == Some("")) {
            return Err("string array contains an invalid value".into());
        }
    }
    Ok(())
}

fn validate_intent<'a>(
    value: &'a Value,
    root_owner: &Owner,
    conversation_id: &str,
    run_id: &str,
) -> Result<String, String> {
    let intent = exact_object(
        value,
        &[
            "schema_version",
            "evaluation_mode",
            "owner",
            "conversation_id",
            "prompt_id",
            "run_id",
            "attempt_id",
            "command_id",
            "target_id",
            "command_sha256",
            "idempotency_key",
            "prompt_run_binding_valid",
            "runner_command_binding_valid",
            "preview_only",
            "selected_target_id",
            "authority",
        ],
    )?;
    if string_field(intent, "schema_version")? != "forge.runner-execution-intent/v1"
        || string_field(intent, "evaluation_mode")? != "pure_runner_binding_only"
        || owner(
            intent
                .get("owner")
                .ok_or_else(|| "intent owner missing".to_owned())?,
        )? != *root_owner
        || string_field(intent, "conversation_id")? != conversation_id
        || string_field(intent, "prompt_id")? != "prompt-001"
        || string_field(intent, "run_id")? != run_id
        || string_field(intent, "attempt_id")? != "attempt-001"
        || string_field(intent, "command_id")? != "command-001"
        || string_field(intent, "target_id")? != "runner-1"
        || string_field(intent, "command_sha256")?.len() != 64
        || string_field(intent, "idempotency_key")? != "run-001:attempt-001:command-001"
    {
        return Err("Runner intent crossed its archival boundary".into());
    }
    for key in [
        "prompt_run_binding_valid",
        "runner_command_binding_valid",
        "preview_only",
    ] {
        bool_field(intent, key, true)?;
    }
    if intent.get("selected_target_id") != Some(&Value::Null) {
        return Err("selected target crossed the archival boundary".into());
    }
    authority(
        intent
            .get("authority")
            .ok_or_else(|| "intent authority missing".to_owned())?,
    )?;
    Ok(string_field(intent, "attempt_id")?.to_owned())
}

fn validate_lease(value: &Value, attempt_id: &str) -> Result<(), String> {
    let lease = exact_object(
        value,
        &[
            "v",
            "attempt_id",
            "target_id",
            "epoch",
            "fencing_token",
            "issued_at_ms",
            "expires_at_ms",
        ],
    )?;
    if integer_field(lease, "v")? != 1
        || string_field(lease, "attempt_id")? != attempt_id
        || string_field(lease, "target_id")? != "runner-1"
        || integer_field(lease, "epoch")? == 0
        || string_field(lease, "fencing_token")? != "fence-001"
        || integer_field(lease, "issued_at_ms")? != 199500
        || integer_field(lease, "expires_at_ms")? != 205500
    {
        return Err("lease crossed its archival boundary or drifted from intent".into());
    }
    Ok(())
}

#[test]
fn canonical_request_is_bounded_and_authority_free() {
    let value = decode(FIXTURE).expect("canonical dispatch-preflight request fixture");
    assert_eq!(value["conversation_id"], "conversation-001");
    assert_eq!(value["run_id"], "run-001");
    assert_eq!(
        value["dispatch_plan"]["runner_execution_intent"]["target_id"],
        "runner-1"
    );
    assert_eq!(value["dispatch_plan"]["lease"]["target_id"], "runner-1");
}

#[test]
fn unknown_duplicate_trailing_selection_drift_and_authority_fail_closed() {
    let mut unknown = serde_json::from_slice::<Value>(FIXTURE).expect("object fixture");
    unknown["unexpected"] = Value::Bool(true);
    assert!(decode(&serde_json::to_vec(&unknown).expect("unknown mutation")).is_err());

    let mut nested_unknown = serde_json::from_slice::<Value>(FIXTURE).expect("object fixture");
    nested_unknown["dispatch_plan"]["placement_request"]["requirements"]["unexpected"] =
        Value::Bool(true);
    assert!(
        decode(&serde_json::to_vec(&nested_unknown).expect("nested unknown mutation")).is_err()
    );

    let source = String::from_utf8_lossy(FIXTURE);
    let duplicate = source.replacen(
        "\"run_id\": \"run-001\"",
        "\"run_id\": \"run-001\", \"run_id\": \"foreign\"",
        1,
    );
    assert!(decode(duplicate.as_bytes()).is_err());
    assert!(decode(format!("{} true", source.trim()).as_bytes()).is_err());

    let mut selected = serde_json::from_slice::<Value>(FIXTURE).expect("object fixture");
    selected["dispatch_plan"]["runner_execution_intent"]["selected_target_id"] =
        Value::String("runner-1".into());
    assert!(decode(&serde_json::to_vec(&selected).expect("selection mutation")).is_err());

    let mut lease_drift = serde_json::from_slice::<Value>(FIXTURE).expect("object fixture");
    lease_drift["dispatch_plan"]["lease"]["target_id"] = Value::String("runner-2".into());
    assert!(decode(&serde_json::to_vec(&lease_drift).expect("lease mutation")).is_err());

    let mut authority = serde_json::from_slice::<Value>(FIXTURE).expect("object fixture");
    authority["dispatch_plan"]["runner_execution_intent"]["authority"]["dispatch_performed"] =
        Value::Bool(true);
    assert!(decode(&serde_json::to_vec(&authority).expect("authority mutation")).is_err());
}
