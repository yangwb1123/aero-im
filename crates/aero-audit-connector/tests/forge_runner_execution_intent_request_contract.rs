//! Forge Runner execution-intent requests are a bounded, authority-free
//! handoff. This receiver does not authenticate a device, issue a lease,
//! dispatch a Runner, execute argv, or grant execution authority.

use serde::de::{self, DeserializeSeed, Deserializer as _, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fmt};

const FIXTURE: &[u8] = include_bytes!("testdata/forge-runner-execution-intent-request-v1.json");
const COMMAND_SHA256: &str = "42ed02a535113450e6f2cc757fb9b4e2cce6143724274191bbae159e9ea8de7a";

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Owner {
    issuer: String,
    subject: String,
    tenant_id: String,
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
struct Request {
    owner: Owner,
    conversation_id: String,
    prompt_receipt: PromptReceipt,
    run_reference: RunReference,
    execution_intent: ExecutionIntent,
    command: Command,
}

fn decode(raw: &[u8]) -> Result<Request, String> {
    reject_duplicate_json_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value = Request::deserialize(&mut decoder).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    validate(&value)?;
    Ok(value)
}

fn validate(value: &Request) -> Result<(), String> {
    if value.owner.issuer != "https://id.example"
        || value.owner.subject != "user-1"
        || value.owner.tenant_id != "tenant-1"
        || value.conversation_id != "conversation-001"
    {
        return Err("invalid owner or conversation binding".into());
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

    let encoded = serde_json::to_vec(command).map_err(|error| error.to_string())?;
    let mut preimage = b"forge.runtime.runner-command.v1\0".to_vec();
    preimage.extend(encoded);
    let digest = Sha256::digest(preimage);
    if format!("{:x}", digest) != intent.command_sha256 {
        return Err("invalid command digest".into());
    }
    Ok(())
}

#[test]
fn canonical_runner_execution_intent_request_is_authority_free() {
    let value = decode(FIXTURE).expect("canonical execution-intent request fixture");
    assert_eq!(value.owner.subject, "user-1");
    assert_eq!(value.execution_intent.run_id, "run-001");
    assert_eq!(value.command.lease_proof.target_id, "runner-1");
}

#[test]
fn unknown_duplicate_trailing_and_binding_mutations_fail_closed() {
    let base = String::from_utf8_lossy(FIXTURE);
    let mut unknown: Map<String, Value> = serde_json::from_slice(FIXTURE).expect("object fixture");
    unknown.insert("unexpected".into(), Value::Bool(true));
    assert!(decode(&serde_json::to_vec(&unknown).expect("unknown mutation")).is_err());

    let trailing = format!("{} true", base.trim());
    assert!(decode(trailing.as_bytes()).is_err());

    let mut owner: Value = serde_json::from_slice(FIXTURE).expect("object fixture");
    owner["owner"]["subject"] = Value::String("foreign".into());
    assert!(decode(&serde_json::to_vec(&owner).expect("owner mutation")).is_err());

    let mut binding: Value = serde_json::from_slice(FIXTURE).expect("object fixture");
    binding["command"]["command_id"] = Value::String("command-foreign".into());
    assert!(decode(&serde_json::to_vec(&binding).expect("binding mutation")).is_err());

    let mut digest: Value = serde_json::from_slice(FIXTURE).expect("object fixture");
    digest["execution_intent"]["command_sha256"] = Value::String("0".repeat(64));
    assert!(decode(&serde_json::to_vec(&digest).expect("digest mutation")).is_err());

    let mut selection: Value = serde_json::from_slice(FIXTURE).expect("object fixture");
    selection["execution_intent"]["selected_target_id"] = Value::String("runner-1".into());
    assert!(decode(&serde_json::to_vec(&selection).expect("selection mutation")).is_err());
}

#[test]
fn duplicate_keys_are_rejected_before_deserialization() {
    let source = String::from_utf8_lossy(FIXTURE);
    let duplicate = source.replacen(
        "\"conversation_id\": \"conversation-001\"",
        "\"conversation_id\": \"conversation-001\", \"conversation_id\": \"foreign\"",
        1,
    );
    assert!(reject_duplicate_json_keys(duplicate.as_bytes()).is_err());
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
