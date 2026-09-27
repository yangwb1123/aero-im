//! Strict Aero-IM receiver for the durable scheduler lease receipt.
//!
//! This value is placement/reservation/lease evidence only. It never grants
//! execution authority, dispatches a Runner, or publishes Audit.

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;

const FIXTURE: &[u8] = include_bytes!("testdata/forge-execution-lease-registry-v1.json");
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Owner {
    issuer: String,
    subject: String,
    tenant_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Authority {
    placement_selected: bool,
    reservation_created: bool,
    lease_issued: bool,
    execution_authorized: bool,
    dispatch_performed: bool,
    audit_published: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Grant {
    v: u16,
    attempt_id: String,
    target_id: String,
    epoch: u64,
    fencing_token: String,
    issued_at_ms: u64,
    expires_at_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema_version: String,
    evaluation_mode: String,
    owner: Owner,
    conversation_id: String,
    run_id: String,
    attempt_id: String,
    device_id: String,
    instance_id: String,
    inventory_revision: u64,
    generation: u64,
    heartbeat_sequence: u64,
    grant: Grant,
    replayed: bool,
    authority: Authority,
}

fn decode(raw: &[u8]) -> Result<Receipt, String> {
    reject_duplicate_json_keys(raw)?;
    let object = serde_json::from_slice::<Value>(raw)
        .map_err(|error| error.to_string())?
        .as_object()
        .cloned()
        .ok_or_else(|| "scheduler lease receipt is not an object".to_owned())?;
    const FIELDS: [&str; 14] = [
        "schema_version",
        "evaluation_mode",
        "owner",
        "conversation_id",
        "run_id",
        "attempt_id",
        "device_id",
        "instance_id",
        "inventory_revision",
        "generation",
        "heartbeat_sequence",
        "grant",
        "replayed",
        "authority",
    ];
    if object.len() != FIELDS.len() || FIELDS.iter().any(|field| !object.contains_key(*field)) {
        return Err("scheduler lease top-level fields are not exact".to_owned());
    }
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let receipt = Receipt::deserialize(&mut decoder).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    Ok(receipt)
}

fn valid_owner_part(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn valid_identifier(value: &str) -> bool {
    if value.is_empty() || value.len() > 128 || value.trim() != value {
        return false;
    }
    value.chars().enumerate().all(|(index, character)| {
        character.is_ascii_alphanumeric()
            || (index > 0 && matches!(character, '.' | ':' | '_' | '+' | '/' | '-'))
    })
}

fn bounded(receipt: &Receipt) -> bool {
    receipt.schema_version == "forge.execution-lease-registry/v1"
        && receipt.evaluation_mode == "durable_scheduler_lease_claim"
        && valid_owner_part(&receipt.owner.issuer)
        && valid_owner_part(&receipt.owner.subject)
        && valid_owner_part(&receipt.owner.tenant_id)
        && valid_identifier(&receipt.conversation_id)
        && valid_identifier(&receipt.run_id)
        && valid_identifier(&receipt.attempt_id)
        && valid_identifier(&receipt.device_id)
        && valid_identifier(&receipt.instance_id)
        && receipt.inventory_revision > 0
        && receipt.inventory_revision <= MAX_SAFE_INTEGER
        && receipt.generation > 0
        && receipt.generation <= MAX_SAFE_INTEGER
        && receipt.heartbeat_sequence > 0
        && receipt.heartbeat_sequence <= MAX_SAFE_INTEGER
        && receipt.grant.v == 1
        && valid_identifier(&receipt.grant.attempt_id)
        && valid_identifier(&receipt.grant.target_id)
        && receipt.grant.attempt_id == receipt.attempt_id
        && receipt.grant.target_id == receipt.instance_id
        && receipt.grant.epoch > 0
        && valid_identifier(&receipt.grant.fencing_token)
        && receipt.grant.issued_at_ms > 0
        && receipt.grant.issued_at_ms <= MAX_SAFE_INTEGER
        && receipt.grant.expires_at_ms <= MAX_SAFE_INTEGER
        && receipt.grant.expires_at_ms > receipt.grant.issued_at_ms
        && receipt.grant.expires_at_ms - receipt.grant.issued_at_ms >= 1_000
        && receipt.grant.expires_at_ms - receipt.grant.issued_at_ms <= 600_000
        && receipt.authority.placement_selected
        && receipt.authority.reservation_created
        && receipt.authority.lease_issued
        && !receipt.authority.execution_authorized
        && !receipt.authority.dispatch_performed
        && !receipt.authority.audit_published
}

#[test]
fn scheduler_lease_receipt_is_strict_and_non_executing() {
    let receipt = decode(FIXTURE).expect("canonical scheduler lease receipt");
    assert!(bounded(&receipt));
    assert_eq!(receipt.device_id, "device-a");
    assert_eq!(receipt.instance_id, "runner-a");
    assert_eq!(receipt.grant.epoch, 1);
    assert!(!receipt.replayed);

    let mut unknown = serde_json::from_slice::<Value>(FIXTURE).unwrap();
    unknown["unexpected"] = Value::Bool(true);
    assert!(decode(&serde_json::to_vec(&unknown).unwrap()).is_err());

    let authority = String::from_utf8_lossy(FIXTURE).replace(
        "\"execution_authorized\": false",
        "\"execution_authorized\": true",
    );
    let authority = decode(authority.as_bytes()).unwrap();
    assert!(!bounded(&authority));

    let binding = String::from_utf8_lossy(FIXTURE)
        .replace("\"target_id\": \"runner-a\"", "\"target_id\": \"runner-b\"");
    let binding = decode(binding.as_bytes()).unwrap();
    assert!(!bounded(&binding));

    let duplicate = String::from_utf8_lossy(FIXTURE).replacen(
        "\"schema_version\": \"forge.execution-lease-registry/v1\",",
        "\"schema_version\": \"forge.execution-lease-registry/v1\",\"schema_version\": \"forge.execution-lease-registry/v1\",",
        1,
    );
    assert!(decode(duplicate.as_bytes()).is_err());

    let mut trailing = FIXTURE.to_vec();
    trailing.extend_from_slice(b" true");
    assert!(decode(&trailing).is_err());
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

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut keys = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(de::Error::custom(format!(
                    "duplicate JSON object key {key:?}"
                )));
            }
            map.next_value_seed(ScanSeed)?;
        }
        Ok(())
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence.next_element_seed(ScanSeed)?.is_some() {}
        Ok(())
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(())
    }
    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(())
    }
    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(())
    }
    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(())
    }
    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(())
    }
    fn visit_string<E>(self, _: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(())
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(())
    }
}
