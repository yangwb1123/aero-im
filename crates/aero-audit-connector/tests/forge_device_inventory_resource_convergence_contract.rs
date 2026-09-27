//! Receiver-side parity for the paired v2 inventory/resource observation.
//!
//! This is an offline compatibility check. It validates a strict, owner-bound
//! pair and never authenticates a device, persists inventory, selects a target,
//! issues a lease, dispatches a Runner, executes a command, or publishes Audit.

use serde_json::{Map, Value};
use std::collections::BTreeSet;

#[path = "support/forge_duplicate_json_keys.rs"]
mod duplicate_json_keys;
#[path = "support/forge_inventory_resource_envelope.rs"]
mod envelope;

const FIXTURE: &[u8] =
    include_bytes!("testdata/forge-device-inventory-resource-convergence-v1.json");
const INVENTORY_NOTICE: &str = "Every owner, instance, state, timestamp, resource, GPU, reservation, residency, trust, sandbox, and concurrency value is an unverified caller declaration. This read-only observation selects no target and grants no execution authority.";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_ARRAY_ITEMS: usize = 128;
const MIN_LEASE_TTL_MS: u64 = 1_000;
const MAX_LEASE_TTL_MS: u64 = 600_000;

fn decode(raw: &[u8]) -> Result<Value, String> {
    duplicate_json_keys::reject_duplicate_json_keys(raw)?;
    serde_json::from_slice(raw).map_err(|error| error.to_string())
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

fn positive_integer(object: &Map<String, Value>, key: &str) -> Result<u64, String> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .filter(|value| *value > 0 && *value <= MAX_SAFE_INTEGER)
        .ok_or_else(|| format!("{key} must be a positive integer"))
}

fn nonnegative_integer(object: &Map<String, Value>, key: &str) -> Result<u64, String> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .filter(|value| *value <= MAX_SAFE_INTEGER)
        .ok_or_else(|| format!("{key} must be a non-negative integer"))
}

fn owner(value: &Value) -> Result<(), String> {
    let object = exact_object(value, &["issuer", "subject", "tenant_id"])?;
    for key in ["issuer", "subject", "tenant_id"] {
        string_field(object, key)?;
    }
    Ok(())
}

fn authority(value: &Value, keys: &[&str]) -> Result<(), String> {
    let object = exact_object(value, keys)?;
    for key in keys {
        bool_field(object, key, false)?;
    }
    Ok(())
}

fn validate_inventory(value: &Value) -> Result<(&Value, &Value), String> {
    let object = exact_object(
        value,
        &[
            "schema_version",
            "evaluation_mode",
            "evaluated_at_ms",
            "owner_declaration",
            "owner_declaration_unverified",
            "inventory_declarations_unverified",
            "notice",
            "devices",
            "execution_authorized",
            "reservation_created",
            "dispatch_performed",
        ],
    )?;
    if object.get("schema_version").and_then(Value::as_str)
        != Some("forge.device-inventory-observation/v2")
        || object.get("evaluation_mode").and_then(Value::as_str) != Some("offline_static_only")
        || object.get("notice").and_then(Value::as_str) != Some(INVENTORY_NOTICE)
    {
        return Err("inventory schema or notice drifted".to_owned());
    }
    positive_integer(object, "evaluated_at_ms")?;
    bool_field(object, "owner_declaration_unverified", true)?;
    bool_field(object, "inventory_declarations_unverified", true)?;
    for key in [
        "execution_authorized",
        "reservation_created",
        "dispatch_performed",
    ] {
        bool_field(object, key, false)?;
    }
    let declared_owner = object
        .get("owner_declaration")
        .ok_or_else(|| "inventory owner missing".to_owned())?;
    owner(declared_owner)?;
    let rows = object
        .get("devices")
        .and_then(Value::as_array)
        .ok_or_else(|| "inventory devices must be an array".to_owned())?;
    if rows.is_empty() || rows.len() > MAX_ARRAY_ITEMS {
        return Err("inventory devices are outside bounds".to_owned());
    }
    let mut device_ids = BTreeSet::new();
    let mut instance_ids = BTreeSet::new();
    let mut previous = ("", "");
    for row in rows {
        let candidate = exact_object(
            row,
            &[
                "instance_id",
                "revision",
                "generation",
                "heartbeat_sequence",
                "device",
            ],
        )?;
        let instance_id = string_field(candidate, "instance_id")?;
        positive_integer(candidate, "revision")?;
        positive_integer(candidate, "generation")?;
        positive_integer(candidate, "heartbeat_sequence")?;
        let device = exact_object(
            candidate
                .get("device")
                .ok_or_else(|| "inventory device missing".to_owned())?,
            &[
                "device_id",
                "owner",
                "approval_state",
                "cordon_state",
                "reservation_state",
                "liveness",
                "snapshot_observed_at_ms",
                "lease_expires_at_ms",
                "os",
                "architecture",
                "available_cpu_cores",
                "available_memory_bytes",
                "available_storage_bytes",
                "runtimes",
                "gpus",
                "data_residency_zones",
                "trust_zone",
                "sandbox_levels",
                "concurrency_limit",
                "active_concurrency",
            ],
        )?;
        let device_id = string_field(device, "device_id")?;
        if (device_id, instance_id) <= previous
            || !device_ids.insert(device_id)
            || !instance_ids.insert(instance_id)
        {
            return Err("inventory rows must be sorted and unique".to_owned());
        }
        previous = (device_id, instance_id);
        if device.get("owner") != Some(declared_owner) {
            return Err("inventory device owner drifted".to_owned());
        }
        for key in [
            "approval_state",
            "cordon_state",
            "reservation_state",
            "liveness",
            "os",
            "architecture",
            "trust_zone",
        ] {
            string_field(device, key)?;
        }
        let snapshot = nonnegative_integer(device, "snapshot_observed_at_ms")?;
        let lease = nonnegative_integer(device, "lease_expires_at_ms")?;
        if lease
            .checked_sub(snapshot)
            .is_none_or(|ttl| !(MIN_LEASE_TTL_MS..=MAX_LEASE_TTL_MS).contains(&ttl))
        {
            return Err("inventory lease window is outside bounds".to_owned());
        }
        for key in [
            "available_cpu_cores",
            "available_memory_bytes",
            "available_storage_bytes",
            "concurrency_limit",
            "active_concurrency",
        ] {
            nonnegative_integer(device, key)?;
        }
        for key in ["runtimes", "gpus", "data_residency_zones", "sandbox_levels"] {
            if !device.get(key).is_some_and(Value::is_array) {
                return Err(format!("{key} must be an array"));
            }
        }
    }
    Ok((declared_owner, object.get("devices").unwrap()))
}

fn validate_resource<'a>(value: &'a Value, expected_owner: &Value) -> Result<&'a Value, String> {
    let object = exact_object(
        value,
        &[
            "schema_version",
            "evaluation_mode",
            "owner_declaration",
            "owner_declaration_unverified",
            "instances",
            "devices",
            "device_attributes_unverified",
            "read_only",
            "authority",
        ],
    )?;
    if object.get("schema_version").and_then(Value::as_str)
        != Some("forge.client-instance-resource-view/v1")
        || object.get("evaluation_mode").and_then(Value::as_str)
            != Some("owner_bound_instance_resource_view_only")
        || object.get("owner_declaration") != Some(expected_owner)
    {
        return Err("resource view schema or owner drifted".to_owned());
    }
    bool_field(object, "owner_declaration_unverified", true)?;
    bool_field(object, "device_attributes_unverified", true)?;
    bool_field(object, "read_only", true)?;
    authority(
        object
            .get("authority")
            .ok_or_else(|| "resource authority missing".to_owned())?,
        &[
            "owner_authenticated",
            "session_read_authorized",
            "prompt_write_authorized",
            "device_identity_verified",
            "reservation_created",
            "execution_authorized",
            "dispatch_performed",
            "audit_published",
        ],
    )?;
    let instances = object
        .get("instances")
        .and_then(Value::as_array)
        .ok_or_else(|| "resource instances must be an array".to_owned())?;
    if instances.is_empty() || instances.len() > MAX_ARRAY_ITEMS {
        return Err("resource instances are empty".to_owned());
    }
    let mut instance_ids = BTreeSet::new();
    let mut previous_instance = "";
    for row in instances {
        let instance = exact_object(
            row,
            &[
                "instance_id",
                "client_kind",
                "session_ids",
                "observed_at_ms",
                "status",
            ],
        )?;
        let instance_id = string_field(instance, "instance_id")?;
        if (!previous_instance.is_empty() && previous_instance >= instance_id)
            || !instance_ids.insert(instance_id)
        {
            return Err("resource instances must be sorted and unique".to_owned());
        }
        previous_instance = instance_id;
        if !matches!(
            string_field(instance, "client_kind")?,
            "app" | "cli" | "mobile" | "tui" | "web"
        ) || !matches!(
            string_field(instance, "status")?,
            "active" | "idle" | "offline" | "unknown"
        ) {
            return Err("resource instance kind or status drifted".to_owned());
        }
        positive_integer(instance, "observed_at_ms")?;
        let sessions = instance
            .get("session_ids")
            .and_then(Value::as_array)
            .ok_or_else(|| "resource session_ids must be an array".to_owned())?;
        if sessions.len() > MAX_ARRAY_ITEMS {
            return Err("resource session_ids are outside bounds".to_owned());
        }
        let mut previous_session = "";
        for session in sessions {
            let session_id = session
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "resource session ID is invalid".to_owned())?;
            if !previous_session.is_empty() && previous_session >= session_id {
                return Err("resource session IDs must be sorted and unique".to_owned());
            }
            previous_session = session_id;
        }
        if sessions.iter().any(|session| !session.is_string()) {
            return Err("resource session_ids must be an array".to_owned());
        }
    }
    let devices = object
        .get("devices")
        .and_then(Value::as_array)
        .ok_or_else(|| "resource devices must be an array".to_owned())?;
    if devices.is_empty() || devices.len() > MAX_ARRAY_ITEMS {
        return Err("resource devices are outside bounds".to_owned());
    }
    let mut device_ids = BTreeSet::new();
    let mut runner_ids = BTreeSet::new();
    let mut previous_device = ("", "");
    for row in devices {
        let device = exact_object(
            row,
            &[
                "device_id",
                "runner_instance_id",
                "owner",
                "revision",
                "generation",
                "heartbeat_sequence",
                "observed_at_ms",
                "approval_state",
                "cordon_state",
                "reservation_state",
                "liveness",
                "os",
                "architecture",
                "cpu_cores",
                "available_cpu_cores",
                "memory_bytes",
                "available_memory_bytes",
                "storage_bytes",
                "available_storage_bytes",
                "gpu_count",
                "available_gpu_memory_bytes",
            ],
        )?;
        if device.get("owner") != Some(expected_owner) {
            return Err("resource device owner drifted".to_owned());
        }
        let device_id = string_field(device, "device_id")?;
        let runner_id = string_field(device, "runner_instance_id")?;
        if (device_id, runner_id) <= previous_device
            || !device_ids.insert(device_id)
            || !runner_ids.insert(runner_id)
        {
            return Err("resource devices must be sorted and unique".to_owned());
        }
        previous_device = (device_id, runner_id);
        for key in [
            "approval_state",
            "cordon_state",
            "reservation_state",
            "liveness",
            "os",
            "architecture",
        ] {
            string_field(device, key)?;
        }
        for key in ["revision", "generation", "heartbeat_sequence"] {
            positive_integer(device, key)?;
        }
        for key in ["observed_at_ms"] {
            positive_integer(device, key)?;
        }
        for key in [
            "cpu_cores",
            "memory_bytes",
            "storage_bytes",
            "available_cpu_cores",
            "available_memory_bytes",
            "available_storage_bytes",
            "gpu_count",
            "available_gpu_memory_bytes",
        ] {
            nonnegative_integer(device, key)?;
        }
        if nonnegative_integer(device, "available_cpu_cores")?
            > nonnegative_integer(device, "cpu_cores")?
            || nonnegative_integer(device, "available_memory_bytes")?
                > nonnegative_integer(device, "memory_bytes")?
            || nonnegative_integer(device, "available_storage_bytes")?
                > nonnegative_integer(device, "storage_bytes")?
        {
            return Err("resource availability exceeds capacity".to_owned());
        }
    }
    Ok(object.get("devices").unwrap())
}

#[test]
fn canonical_inventory_resource_convergence_is_strict_and_display_only() {
    let value = decode(FIXTURE).expect("inventory/resource convergence fixture");
    envelope::validate_envelope(&value).expect("canonical inventory/resource convergence envelope");
}

#[test]
fn inventory_resource_convergence_rejects_unknown_authority_drift_and_duplicate_keys() {
    let mut value = decode(FIXTURE).expect("fixture");
    value
        .as_object_mut()
        .unwrap()
        .insert("unexpected".into(), Value::Bool(true));
    assert!(envelope::validate_envelope(&value).is_err());

    let mut value = decode(FIXTURE).expect("fixture");
    value["authority"]["lease_issued"] = Value::Bool(true);
    assert!(envelope::validate_envelope(&value).is_err());

    let mut value = decode(FIXTURE).expect("fixture");
    value["resource_view"]["devices"][0]["heartbeat_sequence"] = Value::from(5_u64);
    assert!(envelope::validate_envelope(&value).is_err());

    let mut value = decode(FIXTURE).expect("fixture");
    value["resource_view"]["devices"][0]["observed_at_ms"] = Value::from(150001_u64);
    assert!(envelope::validate_envelope(&value).is_err());

    let mut capacity = decode(FIXTURE).expect("fixture");
    capacity["resource_view"]["devices"][0]["available_cpu_cores"] = Value::from(9_u64);
    assert!(envelope::validate_envelope(&capacity).is_err());

    let mut duplicate = String::from_utf8(FIXTURE.to_vec()).expect("fixture UTF-8");
    duplicate = duplicate.trim_end().trim_end_matches('}').to_owned();
    duplicate.push_str(",\"schema_version\":\"forge.device-inventory-resource-convergence/v1\"}\n");
    assert!(decode(duplicate.as_bytes()).is_err());
}
