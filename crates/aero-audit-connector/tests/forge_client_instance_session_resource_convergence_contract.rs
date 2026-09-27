//! Receiver-side parity for the composed display-only client-instance view.
//!
//! This contract is an offline compatibility check. It never authenticates an
//! owner or instance, persists a session/resource view, publishes Audit, or
//! grants device, reservation, dispatch, or execution authority.

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::Deserializer;
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::fmt;

const FIXTURE: &[u8] =
    include_bytes!("testdata/forge-client-instance-session-resource-convergence-v1.json");
const MAX_CLIENT_INSTANCE_IDENTIFIER_BYTES: usize = 128;
const MAX_CLIENT_INSTANCE_OWNER_PART_BYTES: usize = 512;
const MAX_CLIENT_INSTANCE_SESSION_IDS: usize = 128;
const MAX_JSON_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

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

fn decode(raw: &[u8]) -> Result<Value, String> {
    reject_duplicate_json_keys(raw)?;
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

fn bool_field(object: &Map<String, Value>, key: &str) -> Result<bool, String> {
    object
        .get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("{key} must be a boolean"))
}

fn positive_integer(object: &Map<String, Value>, key: &str) -> Result<u64, String> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| format!("{key} must be a positive integer"))
}

fn authority_is_offline(value: &Value) -> Result<(), String> {
    let object = exact_object(
        value,
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
    for key in object.keys() {
        if bool_field(object, key)? {
            return Err(format!("{key} unexpectedly grants authority"));
        }
    }
    Ok(())
}

fn owner(value: &Value) -> Result<(), String> {
    let object = exact_object(value, &["issuer", "subject", "tenant_id"])?;
    for key in ["issuer", "subject", "tenant_id"] {
        if !valid_owner_part(string_field(object, key)?) {
            return Err(format!("{key} is not a bounded owner part"));
        }
    }
    Ok(())
}

fn valid_owner_part(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_CLIENT_INSTANCE_OWNER_PART_BYTES
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn valid_identifier(value: &str) -> bool {
    if value.is_empty() || value.len() > MAX_CLIENT_INSTANCE_IDENTIFIER_BYTES {
        return false;
    }
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && characters.all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '.' | '_' | ':' | '+' | '/' | '-')
        })
}

fn valid_client_kind(value: &str) -> bool {
    matches!(value, "cli" | "tui" | "web" | "app" | "mobile")
}

fn valid_client_status(value: &str) -> bool {
    matches!(value, "active" | "idle" | "offline" | "unknown")
}

fn instances(value: &Value, expected_owner: &Value) -> Result<(), String> {
    let rows = value
        .as_array()
        .ok_or_else(|| "instances must be an array".to_owned())?;
    if rows.len() != 5 {
        return Err("instances must contain five bounded client rows".to_owned());
    }
    let kinds = ["app", "cli", "mobile", "tui", "web"];
    let mut previous_id = "";
    for (index, row) in rows.iter().enumerate() {
        let object = exact_object(
            row,
            &[
                "instance_id",
                "client_kind",
                "session_ids",
                "observed_at_ms",
                "status",
            ],
        )?;
        let instance_id = string_field(object, "instance_id")?;
        if !valid_identifier(instance_id) {
            return Err("instance ID is not bounded or canonical".to_owned());
        }
        if index > 0 && previous_id >= instance_id {
            return Err("instances must be sorted and unique".to_owned());
        }
        previous_id = instance_id;
        let client_kind = string_field(object, "client_kind")?;
        if !valid_client_kind(client_kind) || client_kind != kinds[index] {
            return Err("instances have a non-canonical client kind".to_owned());
        }
        if !valid_client_status(string_field(object, "status")?) {
            return Err("instances have an unsupported status".to_owned());
        }
        if positive_integer(object, "observed_at_ms")? > MAX_JSON_SAFE_INTEGER {
            return Err("instance observation time is outside JSON-safe range".to_owned());
        }
        let sessions = object
            .get("session_ids")
            .and_then(Value::as_array)
            .ok_or_else(|| "session_ids must be an array".to_owned())?;
        if sessions.len() > MAX_CLIENT_INSTANCE_SESSION_IDS {
            return Err("session IDs exceed the bounded row limit".to_owned());
        }
        let mut previous_session = "";
        for (session_index, session) in sessions.iter().enumerate() {
            let session_id = session
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "session ID must be a non-empty string".to_owned())?;
            if !valid_identifier(session_id) {
                return Err("session ID is not bounded or canonical".to_owned());
            }
            if session_index > 0 && previous_session >= session_id {
                return Err("session IDs must be sorted and unique".to_owned());
            }
            previous_session = session_id;
        }
        if expected_owner.is_null() {
            return Err("owner is missing".to_owned());
        }
    }
    Ok(())
}

fn validate_session(value: &Value) -> Result<(Value, Value), String> {
    let object = exact_object(
        value,
        &[
            "schema_version",
            "evaluation_mode",
            "owner_declaration",
            "owner_declaration_unverified",
            "instances",
            "read_only",
            "authority",
        ],
    )?;
    if string_field(object, "schema_version")? != "forge.client-instance-session-view/v1"
        || string_field(object, "evaluation_mode")? != "owner_bound_session_view_only"
        || !bool_field(object, "owner_declaration_unverified")?
        || !bool_field(object, "read_only")?
    {
        return Err("session view is not display-only".to_owned());
    }
    let owner_value = object["owner_declaration"].clone();
    owner(&owner_value)?;
    instances(&object["instances"], &owner_value)?;
    authority_is_offline(&object["authority"])?;
    Ok((owner_value, object["instances"].clone()))
}

fn validate_resource(value: &Value) -> Result<(Value, Value), String> {
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
    if string_field(object, "schema_version")? != "forge.client-instance-resource-view/v1"
        || string_field(object, "evaluation_mode")? != "owner_bound_instance_resource_view_only"
        || !bool_field(object, "owner_declaration_unverified")?
        || !bool_field(object, "device_attributes_unverified")?
        || !bool_field(object, "read_only")?
    {
        return Err("resource view is not display-only".to_owned());
    }
    let owner_value = object["owner_declaration"].clone();
    owner(&owner_value)?;
    instances(&object["instances"], &owner_value)?;
    authority_is_offline(&object["authority"])?;
    let devices = object["devices"]
        .as_array()
        .ok_or_else(|| "devices must be an array".to_owned())?;
    if devices.len() != 2 {
        return Err("devices must contain two bounded rows".to_owned());
    }
    let mut previous_id = "";
    for device in devices {
        let device_object = exact_object(
            device,
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
        let device_id = string_field(device_object, "device_id")?;
        if !valid_identifier(device_id)
            || !valid_identifier(string_field(device_object, "runner_instance_id")?)
        {
            return Err("resource device identifiers are not bounded or canonical".to_owned());
        }
        if !previous_id.is_empty() && previous_id >= device_id {
            return Err("devices must be sorted and unique".to_owned());
        }
        previous_id = device_id;
        if device_object["owner"] != owner_value {
            return Err("device owner drifted from resource owner".to_owned());
        }
        for key in [
            "runner_instance_id",
            "approval_state",
            "cordon_state",
            "reservation_state",
            "liveness",
            "os",
            "architecture",
        ] {
            string_field(device_object, key)?;
        }
        for key in [
            "revision",
            "generation",
            "heartbeat_sequence",
            "observed_at_ms",
            "cpu_cores",
            "memory_bytes",
            "storage_bytes",
        ] {
            positive_integer(device_object, key)?;
        }
        if positive_integer(device_object, "observed_at_ms")? > MAX_JSON_SAFE_INTEGER {
            return Err("resource device observation time is outside JSON-safe range".to_owned());
        }
        if positive_integer(device_object, "available_cpu_cores")?
            > positive_integer(device_object, "cpu_cores")?
            || positive_integer(device_object, "available_memory_bytes")?
                > positive_integer(device_object, "memory_bytes")?
            || positive_integer(device_object, "available_storage_bytes")?
                > positive_integer(device_object, "storage_bytes")?
        {
            return Err("resource availability exceeds capacity".to_owned());
        }
        for key in ["gpu_count", "available_gpu_memory_bytes"] {
            if device_object[key].as_u64().is_none() {
                return Err(format!("{key} must be an unsigned integer"));
            }
        }
    }
    Ok((owner_value, object["instances"].clone()))
}

fn validate_envelope(value: &Value) -> Result<(), String> {
    let object = exact_object(
        value,
        &[
            "schema_version",
            "evaluation_mode",
            "session_view",
            "resource_view",
            "converged",
            "read_only",
            "authority",
        ],
    )?;
    if string_field(object, "schema_version")?
        != "forge.client-instance-session-resource-convergence/v1"
        || string_field(object, "evaluation_mode")?
            != "owner_bound_client_instance_session_resource_convergence_only"
        || !bool_field(object, "converged")?
        || !bool_field(object, "read_only")?
    {
        return Err("convergence envelope is not read-only and converged".to_owned());
    }
    authority_is_offline(&object["authority"])?;
    let (session_owner, session_instances) = validate_session(&object["session_view"])?;
    let (resource_owner, resource_instances) = validate_resource(&object["resource_view"])?;
    if session_owner != resource_owner || session_instances != resource_instances {
        return Err("session/resource observations did not converge".to_owned());
    }
    Ok(())
}

#[test]
fn canonical_convergence_fixture_is_strict_and_authority_free() {
    let value = decode(FIXTURE).expect("convergence fixture");
    validate_envelope(&value).expect("canonical convergence envelope");
}

#[test]
fn convergence_rejects_unknown_duplicate_and_drifted_values() {
    let mut unknown: Value = decode(FIXTURE).expect("fixture");
    unknown
        .as_object_mut()
        .expect("root object")
        .insert("prompt".into(), Value::String("raw prompt".into()));
    assert!(validate_envelope(&unknown).is_err());

    let duplicate = format!(
        r#"{},"schema_version":"forge.client-instance-session-resource-convergence/v1"}}"#,
        String::from_utf8_lossy(FIXTURE).trim_end_matches('}')
    );
    assert!(decode(duplicate.as_bytes()).is_err());

    let mut authority: Value = decode(FIXTURE).expect("fixture");
    authority["authority"]["dispatch_performed"] = Value::Bool(true);
    assert!(validate_envelope(&authority).is_err());

    let mut drift: Value = decode(FIXTURE).expect("fixture");
    drift["resource_view"]["instances"][0]["observed_at_ms"] = Value::from(200501_u64);
    assert!(validate_envelope(&drift).is_err());

    let mut owner_drift: Value = decode(FIXTURE).expect("fixture");
    owner_drift["resource_view"]["owner_declaration"]["subject"] = Value::String("other".into());
    assert!(validate_envelope(&owner_drift).is_err());

    for (field, replacement) in [
        ("client_kind", Value::String("runner".into())),
        ("status", Value::String("draining".into())),
        ("observed_at_ms", Value::from(MAX_JSON_SAFE_INTEGER + 1)),
        ("instance_id", Value::String("client instance".into())),
    ] {
        let mut malformed: Value = decode(FIXTURE).expect("fixture");
        malformed["session_view"]["instances"][0][field] = replacement;
        assert!(
            validate_envelope(&malformed).is_err(),
            "malformed client-instance field {field} unexpectedly converged"
        );
    }

    let mut malformed_owner: Value = decode(FIXTURE).expect("fixture");
    malformed_owner["session_view"]["owner_declaration"]["issuer"] =
        Value::String(" https://id.example".into());
    assert!(validate_envelope(&malformed_owner).is_err());

    let mut malformed_device: Value = decode(FIXTURE).expect("fixture");
    malformed_device["resource_view"]["devices"][0]["device_id"] =
        Value::String("device id".into());
    assert!(validate_envelope(&malformed_device).is_err());
}
