//! Runner terminal receipt outcome vectors are a pure cross-language value
//! contract. This receiver does not authenticate a Runner, persist a receipt,
//! dispatch work, or grant execution authority.

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer as _, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fmt};

const FIXTURE: &[u8] = include_bytes!("testdata/forge-runner-terminal-receipt-vectors-v1.json");
const DIGEST_DOMAIN: &[u8] = b"forge.runtime.runner-command.v1\0";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: String,
    evaluation_mode: String,
    authority: Authority,
    vectors: Vec<Vector>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Vector {
    name: String,
    grant: Grant,
    command: Command,
    receipt: Receipt,
    expected: Expected,
}

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

#[derive(Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct Proof {
    attempt_id: String,
    target_id: String,
    epoch: u64,
    fencing_token: String,
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

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Disposition {
    Completed { receipt_sha256: String },
    Failed { reason: String },
    Uncertain { reason: String },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Expected {
    command_sha256: String,
    disposition_kind: String,
    receipt_valid: bool,
    uncertain: bool,
    reconciliation_required: bool,
    manual_review_required: bool,
    automatic_retry: bool,
    follow_up: String,
}

#[test]
fn runner_terminal_receipt_vectors_match_all_outcomes() {
    let fixture = decode(FIXTURE).expect("strict Runner terminal receipt vector fixture");
    assert_eq!(
        fixture.schema_version,
        "forge.runner-terminal-receipt-vectors/v1"
    );
    assert_eq!(
        fixture.evaluation_mode,
        "pure_runner_terminal_receipt_vectors_only"
    );
    assert_eq!(fixture.authority, Authority::default());
    assert_eq!(fixture.vectors.len(), 3);

    let mut names = BTreeSet::new();
    for vector in fixture.vectors {
        assert!(!vector.name.is_empty());
        assert!(names.insert(vector.name.clone()));
        assert_eq!(vector.grant.v, 1);
        assert!(vector.grant.epoch > 0);
        assert!(vector.grant.expires_at_ms > vector.grant.issued_at_ms);
        assert_eq!(
            vector.grant.expires_at_ms - vector.grant.issued_at_ms,
            10_000
        );
        assert_eq!(vector.command.v, 1);
        assert_eq!(
            vector.command.lease_proof.attempt_id,
            vector.grant.attempt_id
        );
        assert_eq!(vector.command.lease_proof.target_id, vector.grant.target_id);
        assert_eq!(vector.command.lease_proof.epoch, vector.grant.epoch);
        assert_eq!(
            vector.command.lease_proof.fencing_token,
            vector.grant.fencing_token
        );
        assert_eq!(vector.receipt.v, 1);
        assert_eq!(vector.receipt.command_id, vector.command.command_id);
        assert_eq!(vector.receipt.proof, vector.command.lease_proof);
        assert!(vector.receipt.observed_at_ms >= vector.grant.issued_at_ms);
        assert!(vector.receipt.observed_at_ms < vector.grant.expires_at_ms);
        assert!(receipt_is_valid(
            &vector.command,
            &vector.grant,
            &vector.receipt
        ));

        let digest = command_sha256(&vector.command);
        assert_eq!(digest, vector.expected.command_sha256);
        assert_eq!(vector.receipt.command_sha256, digest);
        assert_eq!(vector.expected.receipt_valid, true);
        assert_eq!(vector.expected.automatic_retry, false);
        match (
            &vector.receipt.disposition,
            vector.expected.disposition_kind.as_str(),
        ) {
            (Disposition::Completed { receipt_sha256 }, "completed") => {
                assert_eq!(receipt_sha256.len(), 64);
                assert!(!vector.expected.uncertain);
                assert!(!vector.expected.reconciliation_required);
                assert!(!vector.expected.manual_review_required);
                assert_eq!(vector.expected.follow_up, "none");
            }
            (Disposition::Failed { reason }, "failed") => {
                assert!(!reason.is_empty());
                assert!(!vector.expected.uncertain);
                assert!(!vector.expected.reconciliation_required);
                assert!(!vector.expected.manual_review_required);
                assert_eq!(vector.expected.follow_up, "none");
            }
            (Disposition::Uncertain { reason }, "uncertain") => {
                assert!(!reason.is_empty());
                assert!(vector.expected.uncertain);
                assert!(vector.expected.reconciliation_required);
                assert!(vector.expected.manual_review_required);
                assert_eq!(vector.expected.follow_up, "reconciliation_manual");
            }
            _ => panic!("disposition mismatch for {}", vector.name),
        }
    }
}

#[test]
fn runner_terminal_receipt_vectors_reject_wire_and_evidence_drift() {
    let source = String::from_utf8_lossy(FIXTURE);
    let duplicate = source.replacen(
        "\"schema_version\": \"forge.runner-terminal-receipt-vectors/v1\",",
        "\"schema_version\": \"forge.runner-terminal-receipt-vectors/v1\", \"schema_version\": \"forge.runner-terminal-receipt-vectors/v1\",",
        1,
    );
    assert!(decode(duplicate.as_bytes()).is_err());

    let unknown = source.replacen(
        "\"evaluation_mode\": \"pure_runner_terminal_receipt_vectors_only\",",
        "\"evaluation_mode\": \"pure_runner_terminal_receipt_vectors_only\", \"unexpected\": true,",
        1,
    );
    assert!(decode(unknown.as_bytes()).is_err());
    assert!(decode(format!("{source} {{}}").as_bytes()).is_err());

    let fixture = decode(FIXTURE).expect("canonical fixture");
    let first = &fixture.vectors[0];
    let mut digest_drift = serde_json::to_value(&first.receipt).expect("receipt value");
    digest_drift["command_sha256"] = Value::String("b".repeat(64));
    let drift: Receipt = serde_json::from_value(digest_drift).expect("receipt mutation");
    assert!(!receipt_is_valid(&first.command, &first.grant, &drift));

    let uncertain = &fixture.vectors[2];
    assert!(uncertain.receipt.observed_at_ms < uncertain.grant.expires_at_ms);
    let mut expired = serde_json::to_value(&uncertain.receipt).expect("receipt value");
    expired["observed_at_ms"] = Value::from(uncertain.grant.expires_at_ms);
    let expired: Receipt = serde_json::from_value(expired).expect("expired receipt mutation");
    assert!(!receipt_is_valid(
        &uncertain.command,
        &uncertain.grant,
        &expired
    ));
}

fn decode(raw: &[u8]) -> Result<Fixture, String> {
    reject_duplicate_json_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let fixture = Fixture::deserialize(&mut decoder).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    Ok(fixture)
}

fn command_sha256(command: &Command) -> String {
    let bytes = serde_json::to_vec(command).expect("canonical command JSON");
    let mut digest = Sha256::new();
    digest.update(DIGEST_DOMAIN);
    digest.update(bytes);
    format!("{:x}", digest.finalize())
}

fn receipt_is_valid(command: &Command, grant: &Grant, receipt: &Receipt) -> bool {
    receipt.v == 1
        && receipt.command_id == command.command_id
        && receipt.command_sha256 == command_sha256(command)
        && receipt.proof == command.lease_proof
        && receipt.proof.attempt_id == grant.attempt_id
        && receipt.proof.target_id == grant.target_id
        && receipt.proof.epoch == grant.epoch
        && receipt.proof.fencing_token == grant.fencing_token
        && is_active(
            grant.issued_at_ms,
            grant.expires_at_ms,
            receipt.observed_at_ms,
        )
}

fn is_active(issued_at_ms: u64, expires_at_ms: u64, observed_at_ms: u64) -> bool {
    observed_at_ms >= issued_at_ms && observed_at_ms < expires_at_ms
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
        let mut keys = BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(de::Error::custom("duplicate JSON key"));
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
