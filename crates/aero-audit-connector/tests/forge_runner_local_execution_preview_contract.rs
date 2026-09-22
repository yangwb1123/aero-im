//! Forge local Runner execution-readiness is a metadata-only comparison.
//!
//! This receiver does not authenticate a Runner, persist a command or receipt,
//! select a target, reserve capacity, dispatch work, publish Audit, or grant
//! execution authority.

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer as _};
use serde_json::{Map, Value};
use std::fmt;

const FIXTURE: &[u8] = include_bytes!("testdata/forge-runner-local-execution-preview-v1.json");
const SCHEMA_VERSION: &str = "forge.runner-local-execution-preview/v1";
const EVALUATION_MODE: &str = "injected_local_runner_preview_only";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Owner {
    issuer: String,
    subject: String,
    tenant_id: String,
}

#[derive(Debug, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Authority {
    device_identity_verified: bool,
    command_persisted: bool,
    reservation_created: bool,
    execution_authorized: bool,
    dispatch_performed: bool,
    audit_published: bool,
}

#[derive(Debug, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SessionAuthority {
    identity_verified: bool,
    receipt_persisted: bool,
    execution_authorized: bool,
    dispatch_performed: bool,
    audit_published: bool,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct TerminalReceipt {
    schema_version: String,
    evaluation_mode: String,
    command_id: String,
    command_sha256: String,
    attempt_id: String,
    target_id: String,
    disposition_kind: String,
    observed_at_ms: u64,
    receipt_valid: bool,
    preview_only: bool,
    uncertain: bool,
    reconciliation_required: bool,
    manual_review_required: bool,
    automatic_retry: bool,
    follow_up: String,
    authority: Authority,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Intent {
    schema_version: String,
    evaluation_mode: String,
    owner: Owner,
    conversation_id: String,
    prompt_id: String,
    run_id: String,
    attempt_id: String,
    command_id: String,
    target_id: String,
    command_sha256: String,
    idempotency_key: String,
    prompt_run_binding_valid: bool,
    runner_command_binding_valid: bool,
    preview_only: bool,
    selected_target_id: Option<String>,
    authority: Authority,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SessionReceipt {
    schema_version: String,
    evaluation_mode: String,
    owner: Owner,
    conversation_id: String,
    prompt_id: String,
    run_id: String,
    receipt_observation: TerminalReceipt,
    prompt_run_binding_valid: bool,
    receipt_binding_valid: bool,
    preview_only: bool,
    selected_target_id: Option<String>,
    authority: SessionAuthority,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Observation {
    schema_version: String,
    evaluation_mode: String,
    runner_execution_intent: Intent,
    session_runner_receipt: SessionReceipt,
    command_id: String,
    attempt_id: String,
    target_id: String,
    command_sha256: String,
    disposition_kind: String,
    observed_at_ms: u64,
    output_bytes: u64,
    exit_code: i64,
    executor_invoked: bool,
    preview_only: bool,
    authority: Authority,
}

fn decode(raw: &[u8]) -> Result<Observation, String> {
    reject_duplicate_json_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value = Observation::deserialize(&mut decoder).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    validate(&value)?;
    Ok(value)
}

fn validate(value: &Observation) -> Result<(), String> {
    if value.schema_version != SCHEMA_VERSION
        || value.evaluation_mode != EVALUATION_MODE
        || !valid_owner(&value.runner_execution_intent.owner)
        || !valid_identifier(&value.runner_execution_intent.conversation_id)
        || !valid_identifier(&value.runner_execution_intent.prompt_id)
        || !valid_identifier(&value.runner_execution_intent.run_id)
        || !valid_identifier(&value.runner_execution_intent.attempt_id)
        || !valid_identifier(&value.runner_execution_intent.command_id)
        || !valid_identifier(&value.runner_execution_intent.target_id)
        || !valid_digest(&value.runner_execution_intent.command_sha256)
        || !valid_identifier(&value.command_id)
        || !valid_identifier(&value.attempt_id)
        || !valid_identifier(&value.target_id)
        || !valid_digest(&value.command_sha256)
        || value.observed_at_ms == 0
        || value.observed_at_ms > MAX_SAFE_INTEGER
        || value.output_bytes > MAX_SAFE_INTEGER
        || value.exit_code.unsigned_abs() > MAX_SAFE_INTEGER
        || !value.executor_invoked
        || !value.preview_only
        || value.authority != Authority::default()
        || value.runner_execution_intent.selected_target_id.is_some()
        || value.session_runner_receipt.selected_target_id.is_some()
    {
        return Err("invalid bounded local Runner execution preview".into());
    }

    let intent = &value.runner_execution_intent;
    if intent.schema_version != "forge.runner-execution-intent/v1"
        || intent.evaluation_mode != "pure_runner_binding_only"
        || !valid_identifier(&intent.conversation_id)
        || !valid_identifier(&intent.prompt_id)
        || !valid_identifier(&intent.run_id)
        || !valid_identifier(&intent.attempt_id)
        || !intent.prompt_run_binding_valid
        || !intent.runner_command_binding_valid
        || !intent.preview_only
        || intent.authority != Authority::default()
        || intent.idempotency_key
            != format!(
                "{}:{}:{}",
                intent.run_id, intent.attempt_id, intent.command_id
            )
    {
        return Err("invalid local Runner execution intent binding".into());
    }

    let session = &value.session_runner_receipt;
    if session.schema_version != "forge.session-runner-receipt-observation/v1"
        || session.evaluation_mode != "pure_session_runner_receipt_binding_only"
        || !valid_owner(&session.owner)
        || session.owner != intent.owner
        || session.conversation_id != intent.conversation_id
        || session.prompt_id != intent.prompt_id
        || session.run_id != intent.run_id
        || !session.prompt_run_binding_valid
        || !session.receipt_binding_valid
        || !session.preview_only
        || session.authority != SessionAuthority::default()
    {
        return Err("invalid local Runner session receipt binding".into());
    }

    let receipt = &session.receipt_observation;
    if receipt.schema_version != "forge.runner-command-terminal-receipt/v1"
        || receipt.evaluation_mode != "pure_runner_command_receipt_only"
        || receipt.command_id != value.command_id
        || receipt.attempt_id != value.attempt_id
        || receipt.target_id != value.target_id
        || receipt.command_sha256 != value.command_sha256
        || receipt.disposition_kind != value.disposition_kind
        || receipt.observed_at_ms != value.observed_at_ms
        || !receipt.receipt_valid
        || !receipt.preview_only
        || receipt.authority != Authority::default()
    {
        return Err("invalid local Runner terminal receipt binding".into());
    }
    match receipt.disposition_kind.as_str() {
        "completed" | "failed" => {
            if receipt.uncertain
                || receipt.reconciliation_required
                || receipt.manual_review_required
                || receipt.automatic_retry
                || receipt.follow_up != "none"
            {
                return Err("terminal disposition flags are inconsistent".into());
            }
        }
        "uncertain" => {
            if !receipt.uncertain
                || !receipt.reconciliation_required
                || !receipt.manual_review_required
                || receipt.automatic_retry
                || receipt.follow_up != "reconciliation_manual"
            {
                return Err("uncertain disposition flags are inconsistent".into());
            }
        }
        _ => return Err("unknown terminal disposition".into()),
    }
    Ok(())
}

fn valid_owner(owner: &Owner) -> bool {
    [
        owner.issuer.as_str(),
        owner.subject.as_str(),
        owner.tenant_id.as_str(),
    ]
    .into_iter()
    .all(valid_owner_part)
}

fn valid_owner_part(value: &str) -> bool {
    !value.is_empty() && value.trim() == value && !value.chars().any(char::is_control)
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.chars().enumerate().all(|(index, character)| {
            character.is_ascii_alphanumeric()
                || (index > 0 && matches!(character, '.' | '_' | ':' | '-' | '+' | '/'))
        })
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[test]
fn canonical_local_runner_execution_preview_is_bounded_and_authority_free() {
    let value = decode(FIXTURE).expect("canonical local Runner execution preview fixture");
    assert_eq!(value.schema_version, SCHEMA_VERSION);
    assert_eq!(value.evaluation_mode, EVALUATION_MODE);
    assert_eq!(
        value.runner_execution_intent.owner.issuer,
        "https://id.example"
    );
    assert_eq!(value.runner_execution_intent.owner.subject, "user-1");
    assert_eq!(value.runner_execution_intent.owner.tenant_id, "tenant-1");
    assert_eq!(value.runner_execution_intent.run_id, "run-001");
    assert_eq!(value.target_id, "runner-1");
    assert_eq!(value.disposition_kind, "completed");
    assert_eq!(value.output_bytes, 8);
    assert_eq!(value.exit_code, 0);
    assert!(value.preview_only);
    assert_eq!(value.authority, Authority::default());
}

#[test]
fn unknown_duplicate_binding_selection_and_authority_mutations_fail_closed() {
    let mut root: Map<String, Value> = serde_json::from_slice(FIXTURE).expect("object fixture");
    root.insert("unexpected".into(), Value::Bool(true));
    assert!(decode(&serde_json::to_vec(&root).expect("unknown mutation")).is_err());

    let duplicate = format!(
        "{},\"schema_version\":\"forge.runner-local-execution-preview/v1\"}}",
        String::from_utf8_lossy(FIXTURE).trim_end_matches('}')
    );
    assert!(decode(duplicate.as_bytes()).is_err());

    let nested_duplicate = String::from_utf8_lossy(FIXTURE).replace(
        "      \"audit_published\": false\n      }\n    },\n    \"prompt_run_binding_valid\"",
        "      \"audit_published\": false,\n      \"audit_published\": false\n      }\n    },\n    \"prompt_run_binding_valid\"",
    );
    assert_ne!(nested_duplicate.as_bytes(), FIXTURE);
    assert!(decode(nested_duplicate.as_bytes()).is_err());

    let owner =
        String::from_utf8_lossy(FIXTURE).replace("\"subject\": \"user-1\"", "\"subject\": \"\"");
    assert!(decode(owner.as_bytes()).is_err());

    let binding = String::from_utf8_lossy(FIXTURE).replace(
        "\"command_id\": \"command-001\"",
        "\"command_id\": \"command-foreign\"",
    );
    assert!(decode(binding.as_bytes()).is_err());

    let mut selected: Map<String, Value> = serde_json::from_slice(FIXTURE).expect("object fixture");
    selected.insert(
        "selected_target_id".into(),
        Value::String("runner-1".into()),
    );
    assert!(decode(&serde_json::to_vec(&selected).expect("selection mutation")).is_err());

    let mut authority: Map<String, Value> =
        serde_json::from_slice(FIXTURE).expect("object fixture");
    let mut authority_value = authority.remove("authority").expect("authority object");
    if let Value::Object(ref mut fields) = authority_value {
        fields.insert("execution_authorized".into(), Value::Bool(true));
    } else {
        panic!("authority is not an object");
    }
    authority.insert("authority".into(), authority_value);
    assert!(decode(&serde_json::to_vec(&authority).expect("authority mutation")).is_err());
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

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut keys = std::collections::BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(de::Error::custom(format!("duplicate JSON key {key:?}")));
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
