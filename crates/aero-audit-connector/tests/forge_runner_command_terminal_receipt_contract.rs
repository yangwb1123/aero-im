//! Strict Aero-IM receiver for the pure Runner command/terminal receipt ABI.
//!
//! This test consumes immutable metadata only. It never opens a Runner
//! transport, persists a receipt, or grants execution/Audit authority.

use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fmt};

const FIXTURE: &[u8] = include_bytes!("testdata/forge-runner-command-terminal-receipt-v1.json");
const DIGEST_DOMAIN: &[u8] = b"forge.runtime.runner-command.v1\0";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct Authority {
    device_identity_verified: bool,
    command_persisted: bool,
    reservation_created: bool,
    execution_authorized: bool,
    dispatch_performed: bool,
    audit_published: bool,
}
#[derive(Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct Proof {
    attempt_id: String,
    target_id: String,
    epoch: u64,
    fencing_token: String,
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
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Command {
    v: u16,
    command_id: String,
    lease_proof: Proof,
    idempotency_key: String,
    workspace_ref: String,
    argv: Vec<String>,
    timeout_ms: u64,
    max_output_bytes: u64,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Disposition {
    Completed { receipt_sha256: String },
    Failed { reason: String },
    Uncertain { reason: String },
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    v: u16,
    command_id: String,
    command_sha256: String,
    proof: Proof,
    disposition: Disposition,
    observed_at_ms: u64,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Expected {
    command_sha256: String,
    receipt_valid: bool,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: String,
    evaluation_mode: String,
    authority: Authority,
    grant: Grant,
    command: Command,
    receipt: Receipt,
    expected: Expected,
}

#[test]
fn runner_command_terminal_receipt_is_bounded_and_authority_free() {
    let fixture = decode(FIXTURE).expect("canonical Runner command receipt fixture");
    assert_eq!(
        fixture.schema_version,
        "forge.runner-command-terminal-receipt/v1"
    );
    assert_eq!(fixture.evaluation_mode, "pure_runner_command_receipt_only");
    assert_eq!(fixture.authority, Authority::default());
    validate(&fixture).expect("canonical command/receipt binding");
}

#[test]
fn runner_command_terminal_receipt_rejects_wire_and_binding_drift() {
    let source = String::from_utf8_lossy(FIXTURE);
    let duplicate = format!(
        "{},\"schema_version\":\"forge.runner-command-terminal-receipt/v1\"}}",
        source.trim_end_matches('}')
    );
    assert!(decode(duplicate.as_bytes()).is_err());
    let unknown = source.replacen(
        "\"evaluation_mode\": \"pure_runner_command_receipt_only\",",
        "\"evaluation_mode\": \"pure_runner_command_receipt_only\", \"unexpected\": true,",
        1,
    );
    assert!(decode(unknown.as_bytes()).is_err());
    assert!(decode(format!("{source} {{}}").as_bytes()).is_err());

    let fixture = decode(FIXTURE).expect("canonical fixture");
    let mut digest_drift = serde_json::to_value(&fixture.receipt).expect("receipt value");
    digest_drift["command_sha256"] = Value::String("b".repeat(64));
    let receipt: Receipt = serde_json::from_value(digest_drift).expect("receipt mutation");
    assert!(validate_receipt(
        &fixture.command,
        &fixture.grant,
        &fixture.expected,
        &receipt
    )
    .is_err());

    let mut proof_drift = serde_json::to_value(&fixture.receipt).expect("receipt value");
    proof_drift["proof"]["target_id"] = Value::String("runner-foreign".into());
    let receipt: Receipt = serde_json::from_value(proof_drift).expect("receipt mutation");
    assert!(validate_receipt(
        &fixture.command,
        &fixture.grant,
        &fixture.expected,
        &receipt
    )
    .is_err());

    let mut expired = serde_json::to_value(&fixture.receipt).expect("receipt value");
    expired["observed_at_ms"] = Value::from(fixture.grant.expires_at_ms);
    let receipt: Receipt = serde_json::from_value(expired).expect("receipt mutation");
    assert!(validate_receipt(
        &fixture.command,
        &fixture.grant,
        &fixture.expected,
        &receipt
    )
    .is_err());
}

fn decode(raw: &[u8]) -> Result<Fixture, String> {
    reject_duplicate_json_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let fixture = Fixture::deserialize(&mut decoder).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    validate(&fixture)?;
    Ok(fixture)
}

fn validate(fixture: &Fixture) -> Result<(), String> {
    let grant = &fixture.grant;
    if grant.v != 1
        || grant.epoch == 0
        || grant.expires_at_ms <= grant.issued_at_ms
        || grant.expires_at_ms - grant.issued_at_ms < 1_000
        || grant.expires_at_ms - grant.issued_at_ms > 600_000
        || grant.expires_at_ms > MAX_SAFE_INTEGER
        || !valid_text(&grant.attempt_id, 128)
        || !valid_text(&grant.target_id, 128)
        || !valid_text(&grant.fencing_token, 256)
    {
        return Err("invalid lease grant".into());
    }
    if fixture.command.v != 1
        || !valid_text(&fixture.command.command_id, 128)
        || !valid_text(&fixture.command.idempotency_key, 256)
        || !valid_text(&fixture.command.workspace_ref, 256)
        || fixture.command.argv.is_empty()
        || fixture.command.argv.len() > 64
        || fixture.command.timeout_ms == 0
        || fixture.command.timeout_ms > 600_000
        || fixture.command.max_output_bytes == 0
        || fixture.command.max_output_bytes > 8 * 1024 * 1024
        || fixture.command.lease_proof
            != (Proof {
                attempt_id: grant.attempt_id.clone(),
                target_id: grant.target_id.clone(),
                epoch: grant.epoch,
                fencing_token: grant.fencing_token.clone(),
            })
    {
        return Err("invalid command binding".into());
    }
    for (index, argument) in fixture.command.argv.iter().enumerate() {
        if index > 0 && argument.is_empty() {
            continue;
        }
        if !valid_text(argument, 4_096) || (index > 0 && argument.trim() != argument) {
            return Err("invalid command argv".into());
        }
    }
    validate_receipt(&fixture.command, grant, &fixture.expected, &fixture.receipt)
}

fn validate_receipt(
    command: &Command,
    grant: &Grant,
    expected: &Expected,
    receipt: &Receipt,
) -> Result<(), String> {
    let digest = command_sha256(command);
    if receipt.v != 1
        || receipt.command_id != command.command_id
        || receipt.command_sha256 != digest
        || expected.command_sha256 != digest
        || !expected.receipt_valid
        || receipt.proof != command.lease_proof
        || receipt.proof.attempt_id != grant.attempt_id
        || receipt.proof.target_id != grant.target_id
        || receipt.proof.epoch != grant.epoch
        || receipt.proof.fencing_token != grant.fencing_token
        || receipt.observed_at_ms < grant.issued_at_ms
        || receipt.observed_at_ms >= grant.expires_at_ms
        || !valid_disposition(&receipt.disposition)
    {
        return Err("invalid terminal receipt".into());
    }
    Ok(())
}

fn valid_disposition(disposition: &Disposition) -> bool {
    match disposition {
        Disposition::Completed { receipt_sha256 } => valid_digest(receipt_sha256),
        Disposition::Failed { reason } | Disposition::Uncertain { reason } => {
            valid_text(reason, 256)
        }
    }
}
fn valid_text(value: &str, limit: usize) -> bool {
    !value.is_empty()
        && value.len() <= limit
        && value.trim() == value
        && !value.chars().any(char::is_control)
}
fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}
fn command_sha256(command: &Command) -> String {
    let bytes = serde_json::to_vec(command).expect("canonical command JSON");
    let mut digest = Sha256::new();
    digest.update(DIGEST_DOMAIN);
    digest.update(bytes);
    format!("{:x}", digest.finalize())
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
