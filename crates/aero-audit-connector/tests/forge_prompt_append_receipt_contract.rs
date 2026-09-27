//! Strict Aero-IM receiver for the content-minimized Prompt append receipt.
//!
//! This compatibility test never writes a Prompt, creates a Run, selects a
//! device, dispatches a Runner, or publishes Audit evidence.

use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::BTreeSet, fmt};

const FIXTURE: &[u8] = include_bytes!("testdata/forge-prompt-append-receipt-v1.json");
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct Owner {
    issuer: String,
    subject: String,
    tenant_id: String,
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct Request {
    conversation_id: String,
    expected_version: u64,
    role: String,
    content_sha256: String,
    idempotency_key_sha256: String,
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct Receipt {
    conversation_id: String,
    prompt_id: String,
    role: String,
    aggregate_version: u64,
    created_at_ms: u64,
    replayed: bool,
    storage_commit_observed: bool,
    content_included: bool,
}

#[derive(Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct Authority {
    run_created: bool,
    device_selected: bool,
    reservation_created: bool,
    dispatch_performed: bool,
    execution_authorized: bool,
    audit_published: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    schema_version: String,
    owner: Owner,
    request: Request,
    receipt: Receipt,
    authority: Authority,
}

#[test]
fn prompt_append_receipt_is_content_free_and_authority_free() {
    let value = decode(FIXTURE).expect("canonical Prompt append receipt");
    validate(&value).expect("valid Prompt append receipt");
    assert_eq!(value.owner.issuer, "https://id.example");
    assert_eq!(value.owner.subject, "user-1");
    assert_eq!(value.owner.tenant_id, "tenant-1");
    assert_eq!(value.request.conversation_id, "conversation-001");
    assert_eq!(value.request.expected_version, 2);
    assert_eq!(value.receipt.prompt_id, "prompt-003");
    assert_eq!(value.receipt.aggregate_version, 3);
    assert!(value.receipt.storage_commit_observed);
    assert!(!value.receipt.content_included);
    assert!(!value.receipt.replayed);

    let source = String::from_utf8_lossy(FIXTURE);
    let duplicate = format!(
        "{},\"schema_version\":\"forge.prompt-append-receipt/v1\"}}",
        source.trim_end_matches('}')
    );
    assert!(decode(duplicate.as_bytes()).is_err());
    let unknown = source.replacen("\"owner\": {", "\"unexpected\": true, \"owner\": {", 1);
    assert!(decode(unknown.as_bytes()).is_err());
    assert!(decode(format!("{source} {{}}").as_bytes()).is_err());

    let mut version: Value = serde_json::from_slice(FIXTURE).expect("fixture value");
    version["receipt"]["aggregate_version"] = Value::from(4);
    let version: Envelope = serde_json::from_value(version).expect("version mutation");
    assert!(validate(&version).is_err());
    let mut authority: Value = serde_json::from_slice(FIXTURE).expect("fixture value");
    authority["authority"]["audit_published"] = Value::Bool(true);
    let authority: Envelope = serde_json::from_value(authority).expect("authority mutation");
    assert!(validate(&authority).is_err());
    let mut content: Value = serde_json::from_slice(FIXTURE).expect("fixture value");
    content["receipt"]["content_included"] = Value::Bool(true);
    let content: Envelope = serde_json::from_value(content).expect("content mutation");
    assert!(validate(&content).is_err());
}

fn decode(raw: &[u8]) -> Result<Envelope, String> {
    reject_duplicate_json_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value = Envelope::deserialize(&mut decoder).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    validate(&value)?;
    Ok(value)
}

fn validate(value: &Envelope) -> Result<(), String> {
    if value.schema_version != "forge.prompt-append-receipt/v1"
        || !valid_text(&value.owner.issuer, 512)
        || !valid_text(&value.owner.subject, 512)
        || !valid_text(&value.owner.tenant_id, 512)
        || !valid_identifier(&value.request.conversation_id)
        || value.request.expected_version == 0
        || value.request.expected_version > MAX_SAFE_INTEGER
        || value.request.role != "user"
        || !valid_digest(&value.request.content_sha256)
        || !valid_digest(&value.request.idempotency_key_sha256)
        || value.receipt.conversation_id != value.request.conversation_id
        || !valid_identifier(&value.receipt.prompt_id)
        || value.receipt.role != value.request.role
        || value.receipt.aggregate_version != value.request.expected_version + 1
        || value.receipt.aggregate_version > MAX_SAFE_INTEGER
        || value.receipt.created_at_ms > MAX_SAFE_INTEGER
        || !value.receipt.storage_commit_observed
        || value.receipt.content_included
        || value.authority != Authority::default()
    {
        return Err("invalid Prompt append receipt".into());
    }
    Ok(())
}

fn valid_text(value: &str, limit: usize) -> bool {
    !value.is_empty()
        && value.len() <= limit
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn valid_identifier(value: &str) -> bool {
    valid_text(value, 128)
        && value.chars().enumerate().all(|(index, character)| {
            character.is_ascii_alphanumeric() || (index > 0 && ".:_+/-".contains(character))
        })
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
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
            if !keys.insert(key) {
                return Err(de::Error::custom("duplicate JSON key"));
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

    fn visit_bool<E>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E>(self, _: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_string<E>(self, _: String) -> Result<(), E> {
        Ok(())
    }
    fn visit_none<E>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E>(self) -> Result<(), E> {
        Ok(())
    }
}
