//! Forge Runner execution intent is a bounded, display-only interoperability
//! value. This receiver never authenticates a device, issues a lease,
//! dispatches a Runner, executes argv, or grants execution authority.

use serde::{de::Deserializer as _, Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fmt};

const FIXTURE: &[u8] = include_bytes!("testdata/forge-runner-execution-intent-v1.json");
const SCHEMA_VERSION: &str = "forge.runner-execution-intent/v1";
const EVALUATION_MODE: &str = "pure_runner_binding_only";
const COMMAND_SHA256: &str = "42ed02a535113450e6f2cc757fb9b4e2cce6143724274191bbae159e9ea8de7a";

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Owner {
    issuer: String,
    subject: String,
    tenant_id: String,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Authority {
    device_identity_verified: bool,
    command_persisted: bool,
    reservation_created: bool,
    execution_authorized: bool,
    dispatch_performed: bool,
    audit_published: bool,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct PromptReceipt {
    prompt_id: String,
    conversation_id: String,
    role: String,
    accepted_at_ms: u64,
    intent_id: String,
    initial_event_id: String,
    initial_event_sequence: u64,
    initial_event_type: String,
    replayed: bool,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RunReference {
    run_id: String,
    conversation_id: String,
    prompt_id: String,
    created_at_ms: u64,
    latest_sequence: u64,
    status: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ExecutionIntent {
    conversation_id: String,
    prompt_id: String,
    run_id: String,
    attempt_id: String,
    command_id: String,
    target_id: String,
    command_sha256: String,
    idempotency_key: String,
    selected_target_id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct LeaseProof {
    attempt_id: String,
    target_id: String,
    epoch: u64,
    fencing_token: String,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Command {
    v: u64,
    command_id: String,
    lease_proof: LeaseProof,
    idempotency_key: String,
    workspace_ref: String,
    argv: Vec<String>,
    timeout_ms: u64,
    max_output_bytes: u64,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Expected {
    prompt_run_binding_valid: bool,
    runner_command_binding_valid: bool,
    preview_only: bool,
    command_sha256: String,
    selected_target_id: Option<String>,
    authority: Authority,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Observation {
    schema_version: String,
    evaluation_mode: String,
    authority: Authority,
    owner: Owner,
    conversation_id: String,
    prompt_receipt: PromptReceipt,
    run_reference: RunReference,
    execution_intent: ExecutionIntent,
    command: Command,
    expected: Expected,
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
        || value.authority != Authority::default()
        || !valid_owner(&value.owner)
        || value.conversation_id != "conversation-001"
    {
        return Err("invalid execution intent envelope".into());
    }

    let prompt = &value.prompt_receipt;
    if prompt.prompt_id != "prompt-001"
        || prompt.conversation_id != value.conversation_id
        || prompt.role != "user"
        || prompt.accepted_at_ms != 200
        || prompt.intent_id != "intent-001"
        || prompt.initial_event_id != "event-001"
        || prompt.initial_event_sequence != 1
        || prompt.initial_event_type != "submitted"
        || prompt.replayed
    {
        return Err("invalid prompt receipt binding".into());
    }

    let run = &value.run_reference;
    if run.run_id != "run-001"
        || run.conversation_id != value.conversation_id
        || run.prompt_id != prompt.prompt_id
        || run.created_at_ms != 200
        || run.latest_sequence != 5
        || run.status != "nonterminal"
    {
        return Err("invalid run reference binding".into());
    }

    let intent = &value.execution_intent;
    if intent.conversation_id != value.conversation_id
        || intent.prompt_id != prompt.prompt_id
        || intent.run_id != run.run_id
        || intent.attempt_id != "attempt-001"
        || intent.command_id != "command-001"
        || intent.target_id != "runner-1"
        || intent.command_sha256 != COMMAND_SHA256
        || intent.idempotency_key != "run-001:attempt-001:command-001"
        || intent.selected_target_id.is_some()
    {
        return Err("invalid execution intent binding".into());
    }

    let command = &value.command;
    if command.v != 1
        || command.command_id != intent.command_id
        || command.idempotency_key != intent.idempotency_key
        || command.workspace_ref != "workspace-001"
        || command.argv != ["forge-task", "--prompt-ref", "prompt-001"]
        || command.timeout_ms != 5000
        || command.max_output_bytes != 65536
        || command.lease_proof.attempt_id != intent.attempt_id
        || command.lease_proof.target_id != intent.target_id
        || command.lease_proof.epoch != 1
        || command.lease_proof.fencing_token != "fence-001"
    {
        return Err("invalid command binding".into());
    }

    let expected = &value.expected;
    if !expected.prompt_run_binding_valid
        || !expected.runner_command_binding_valid
        || !expected.preview_only
        || expected.command_sha256 != intent.command_sha256
        || expected.selected_target_id.is_some()
        || expected.authority != Authority::default()
    {
        return Err("invalid expected metadata".into());
    }
    Ok(())
}

fn valid_owner(owner: &Owner) -> bool {
    [&owner.issuer, &owner.subject, &owner.tenant_id]
        .into_iter()
        .all(|value| {
            !value.is_empty() && value.trim() == value && !value.chars().any(char::is_control)
        })
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[test]
fn canonical_runner_execution_intent_is_authority_free() {
    let value = decode(FIXTURE).expect("canonical Runner execution intent fixture");
    assert_eq!(value.schema_version, SCHEMA_VERSION);
    assert_eq!(value.owner.subject, "user-1");
    assert_eq!(value.execution_intent.run_id, "run-001");
    assert_eq!(value.execution_intent.command_sha256, COMMAND_SHA256);
    assert_eq!(value.command.lease_proof.target_id, "runner-1");
    assert_eq!(value.authority, Authority::default());
}

#[test]
fn unknown_duplicate_binding_selection_and_authority_mutations_fail_closed() {
    let mut root: Map<String, Value> = serde_json::from_slice(FIXTURE).expect("object fixture");
    root.insert("unexpected".into(), Value::Bool(true));
    assert!(decode(&serde_json::to_vec(&root).expect("unknown mutation")).is_err());

    let duplicate = format!(
        "{},\"schema_version\":\"forge.runner-execution-intent/v1\"}}",
        String::from_utf8_lossy(FIXTURE).trim_end_matches('}')
    );
    assert!(decode(duplicate.as_bytes()).is_err());

    let owner =
        String::from_utf8_lossy(FIXTURE).replace("\"subject\": \"user-1\"", "\"subject\": \"\"");
    assert!(decode(owner.as_bytes()).is_err());

    let mut run_reference_id: Map<String, Value> =
        serde_json::from_slice(FIXTURE).expect("run reference object");
    run_reference_id["run_reference"]["run_id"] = Value::String("run-foreign".into());
    assert!(decode(&serde_json::to_vec(&run_reference_id).expect("run id mutation")).is_err());

    let mut run_reference_prompt: Map<String, Value> =
        serde_json::from_slice(FIXTURE).expect("run reference object");
    run_reference_prompt["run_reference"]["prompt_id"] = Value::String("prompt-foreign".into());
    assert!(
        decode(&serde_json::to_vec(&run_reference_prompt).expect("run prompt mutation")).is_err()
    );

    let binding = String::from_utf8_lossy(FIXTURE).replace(
        "\"command_id\": \"command-001\"",
        "\"command_id\": \"command-foreign\"",
    );
    assert!(decode(binding.as_bytes()).is_err());

    let selection = String::from_utf8_lossy(FIXTURE).replacen(
        "\"selected_target_id\": null",
        "\"selected_target_id\": \"runner-1\"",
        1,
    );
    assert!(decode(selection.as_bytes()).is_err());

    let authority = String::from_utf8_lossy(FIXTURE).replacen(
        "\"execution_authorized\": false",
        "\"execution_authorized\": true",
        1,
    );
    assert!(decode(authority.as_bytes()).is_err());

    assert!(valid_digest(COMMAND_SHA256));
}

const REQUEST_FIXTURE: &[u8] =
    include_bytes!("testdata/forge-runner-execution-intent-request-v1.json");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    owner: Owner,
    conversation_id: String,
    prompt_receipt: PromptReceipt,
    run_reference: RunReference,
    execution_intent: ExecutionIntent,
    command: Command,
}

fn decode_request(raw: &[u8]) -> Result<Request, String> {
    reject_duplicate_json_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value = Request::deserialize(&mut decoder).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    validate_request(&value)?;
    Ok(value)
}

fn validate_request(value: &Request) -> Result<(), String> {
    if !valid_owner(&value.owner) || value.conversation_id != "conversation-001" {
        return Err("invalid request owner or conversation binding".into());
    }
    let prompt = &value.prompt_receipt;
    if prompt.prompt_id != "prompt-001"
        || prompt.conversation_id != value.conversation_id
        || prompt.role != "user"
        || prompt.accepted_at_ms != 200
        || prompt.intent_id != "intent-001"
        || prompt.initial_event_id != "event-001"
        || prompt.initial_event_sequence != 1
        || prompt.initial_event_type != "submitted"
        || prompt.replayed
    {
        return Err("invalid request prompt binding".into());
    }
    let run = &value.run_reference;
    if run.run_id != "run-001"
        || run.conversation_id != value.conversation_id
        || run.prompt_id != prompt.prompt_id
        || run.created_at_ms != 200
        || run.latest_sequence != 5
        || run.status != "nonterminal"
    {
        return Err("invalid request run binding".into());
    }
    let intent = &value.execution_intent;
    if intent.conversation_id != value.conversation_id
        || intent.prompt_id != prompt.prompt_id
        || intent.run_id != run.run_id
        || intent.attempt_id != "attempt-001"
        || intent.command_id != "command-001"
        || intent.target_id != "runner-1"
        || intent.command_sha256 != COMMAND_SHA256
        || intent.idempotency_key != "run-001:attempt-001:command-001"
        || intent.selected_target_id.is_some()
    {
        return Err("invalid request execution-intent binding".into());
    }
    let command = &value.command;
    if command.v != 1
        || command.command_id != intent.command_id
        || command.idempotency_key != intent.idempotency_key
        || command.workspace_ref != "workspace-001"
        || command.argv != ["forge-task", "--prompt-ref", "prompt-001"]
        || command.timeout_ms != 5000
        || command.max_output_bytes != 65536
        || command.lease_proof.attempt_id != intent.attempt_id
        || command.lease_proof.target_id != intent.target_id
        || command.lease_proof.epoch != 1
        || command.lease_proof.fencing_token != "fence-001"
    {
        return Err("invalid request command binding".into());
    }
    if command_sha256(command) != intent.command_sha256 {
        return Err("invalid request command digest".into());
    }
    Ok(())
}

fn command_sha256(command: &Command) -> String {
    let encoded = serde_json::to_vec(command).expect("Runner command JSON");
    let mut digest = Sha256::new();
    digest.update(b"forge.runtime.runner-command.v1\0");
    digest.update(encoded);
    format!("{:x}", digest.finalize())
}

#[test]
fn canonical_runner_execution_intent_request_is_authority_free() {
    let value = decode_request(REQUEST_FIXTURE).expect("canonical Runner execution-intent request");
    assert_eq!(value.owner.subject, "user-1");
    assert_eq!(value.execution_intent.run_id, "run-001");
    assert_eq!(value.command.lease_proof.target_id, "runner-1");
    assert_eq!(command_sha256(&value.command), COMMAND_SHA256);
}

#[test]
fn request_unknown_duplicate_binding_and_selection_mutations_fail_closed() {
    let base = String::from_utf8_lossy(REQUEST_FIXTURE);
    let mut unknown: Map<String, Value> =
        serde_json::from_slice(REQUEST_FIXTURE).expect("request object");
    unknown.insert("unexpected".into(), Value::Bool(true));
    assert!(decode_request(&serde_json::to_vec(&unknown).expect("unknown mutation")).is_err());

    let duplicate = format!("{},\"owner\":{{}}}}", base.trim_end_matches('}'));
    assert!(decode_request(duplicate.as_bytes()).is_err());

    let owner = base.replace("\"subject\": \"user-1\"", "\"subject\": \"\"");
    assert!(decode_request(owner.as_bytes()).is_err());

    let binding = base.replace(
        "\"command_id\": \"command-001\"",
        "\"command_id\": \"command-foreign\"",
    );
    assert!(decode_request(binding.as_bytes()).is_err());

    let mut run_reference_id: Map<String, Value> =
        serde_json::from_slice(REQUEST_FIXTURE).expect("request object");
    run_reference_id["run_reference"]["run_id"] = Value::String("run-foreign".into());
    assert!(decode_request(
        &serde_json::to_vec(&run_reference_id).expect("request run id mutation")
    )
    .is_err());

    let mut run_reference_prompt: Map<String, Value> =
        serde_json::from_slice(REQUEST_FIXTURE).expect("request object");
    run_reference_prompt["run_reference"]["prompt_id"] = Value::String("prompt-foreign".into());
    assert!(decode_request(
        &serde_json::to_vec(&run_reference_prompt).expect("request run prompt mutation")
    )
    .is_err());

    let digest = base.replace(
        COMMAND_SHA256,
        "0000000000000000000000000000000000000000000000000000000000000000",
    );
    assert!(decode_request(digest.as_bytes()).is_err());

    let selection = base.replace(
        "\"selected_target_id\": null",
        "\"selected_target_id\": \"runner-1\"",
    );
    assert!(decode_request(selection.as_bytes()).is_err());
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

impl<'de> serde::de::DeserializeSeed<'de> for ScanSeed {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(ScanVisitor)
    }
}

impl<'de> serde::de::Visitor<'de> for ScanVisitor {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON without duplicate object keys")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        let mut keys = BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(serde::de::Error::custom(format!(
                    "duplicate JSON key {key:?}"
                )));
            }
            map.next_value_seed(ScanSeed)?;
        }
        Ok(())
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        while sequence.next_element_seed(ScanSeed)?.is_some() {}
        Ok(())
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(())
    }

    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(())
    }

    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(())
    }

    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(())
    }

    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(())
    }

    fn visit_string<E>(self, _: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(())
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(())
    }
}
