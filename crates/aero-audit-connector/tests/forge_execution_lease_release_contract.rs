//! Strict Aero-IM receiver for a terminal scheduler lease release.
//!
//! This value is a metadata-only observation. It never releases a lease,
//! selects a device, dispatches a Runner, or grants execution/Audit authority.

use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;

const FIXTURE: &[u8] = include_bytes!("testdata/forge-execution-lease-release-v1.json");
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
struct Receipt {
    schema_version: String,
    evaluation_mode: String,
    owner: Owner,
    conversation_id: String,
    run_id: String,
    attempt_id: String,
    device_id: String,
    instance_id: String,
    epoch: u64,
    released_at_ms: u64,
    replayed: bool,
    authority: Authority,
}

fn decode(raw: &[u8]) -> Result<Receipt, String> {
    reject_duplicate_json_keys(raw)?;
    let object = serde_json::from_slice::<Value>(raw)
        .map_err(|error| error.to_string())?
        .as_object()
        .cloned()
        .ok_or_else(|| "lease release receipt is not an object".to_owned())?;
    const FIELDS: [&str; 12] = [
        "schema_version",
        "evaluation_mode",
        "owner",
        "conversation_id",
        "run_id",
        "attempt_id",
        "device_id",
        "instance_id",
        "epoch",
        "released_at_ms",
        "replayed",
        "authority",
    ];
    if object.len() != FIELDS.len() || FIELDS.iter().any(|field| !object.contains_key(*field)) {
        return Err("lease release top-level fields are not exact".to_owned());
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
    receipt.schema_version == "forge.execution-lease-release/v1"
        && receipt.evaluation_mode == "durable_scheduler_lease_release"
        && valid_owner_part(&receipt.owner.issuer)
        && valid_owner_part(&receipt.owner.subject)
        && valid_owner_part(&receipt.owner.tenant_id)
        && valid_identifier(&receipt.conversation_id)
        && valid_identifier(&receipt.run_id)
        && valid_identifier(&receipt.attempt_id)
        && valid_identifier(&receipt.device_id)
        && valid_identifier(&receipt.instance_id)
        && receipt.epoch > 0
        && receipt.epoch <= MAX_SAFE_INTEGER
        && receipt.released_at_ms > 0
        && receipt.released_at_ms <= MAX_SAFE_INTEGER
        && !receipt.authority.placement_selected
        && !receipt.authority.reservation_created
        && !receipt.authority.lease_issued
        && !receipt.authority.execution_authorized
        && !receipt.authority.dispatch_performed
        && !receipt.authority.audit_published
}

fn matches_canonical(receipt: &Receipt) -> bool {
    bounded(receipt)
        && receipt.owner.issuer == "https://id.example"
        && receipt.owner.subject == "user-a"
        && receipt.owner.tenant_id == "tenant-a"
        && receipt.conversation_id == "conversation-1"
        && receipt.run_id == "run-1"
        && receipt.attempt_id == "attempt-1"
        && receipt.device_id == "device-a"
        && receipt.instance_id == "runner-a"
        && receipt.epoch == 2
        && receipt.released_at_ms == 1_800_000_040_000
        && !receipt.replayed
}

#[test]
fn scheduler_lease_release_receipt_is_strict_and_non_executing() {
    let receipt = decode(FIXTURE).expect("canonical lease release receipt");
    assert!(matches_canonical(&receipt));
    assert_eq!(receipt.device_id, "device-a");
    assert_eq!(receipt.instance_id, "runner-a");
    assert_eq!(receipt.epoch, 2);
    assert!(!receipt.replayed);

    let mut unknown = serde_json::from_slice::<Value>(FIXTURE).unwrap();
    unknown["unexpected"] = Value::Bool(true);
    assert!(decode(&serde_json::to_vec(&unknown).unwrap()).is_err());

    let binding = String::from_utf8_lossy(FIXTURE).replace(
        "\"instance_id\": \"runner-a\"",
        "\"instance_id\": \"runner-b\"",
    );
    let binding = decode(binding.as_bytes()).unwrap();
    assert!(!matches_canonical(&binding));

    let epoch = String::from_utf8_lossy(FIXTURE).replace("\"epoch\": 2", "\"epoch\": 0");
    let epoch = decode(epoch.as_bytes()).unwrap();
    assert!(!bounded(&epoch));

    let authority = String::from_utf8_lossy(FIXTURE)
        .replace("\"lease_issued\": false", "\"lease_issued\": true");
    let authority = decode(authority.as_bytes()).unwrap();
    assert!(!bounded(&authority));

    let duplicate = String::from_utf8_lossy(FIXTURE).replacen(
        "\"schema_version\": \"forge.execution-lease-release/v1\",",
        "\"schema_version\": \"forge.execution-lease-release/v1\",\"schema_version\": \"forge.execution-lease-release/v1\",",
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
