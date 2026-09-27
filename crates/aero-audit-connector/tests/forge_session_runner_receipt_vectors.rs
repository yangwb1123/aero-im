//! Session-bound Runner terminal receipt outcome vectors are a pure value
//! interoperability contract. They do not persist receipts or grant effect
//! authority.

use serde::Deserialize;

const FIXTURE: &str = include_str!("testdata/forge-session-runner-receipt-vectors-v1.json");
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Authority {
    identity_verified: bool,
    receipt_persisted: bool,
    execution_authorized: bool,
    dispatch_performed: bool,
    audit_published: bool,
}

impl Default for Authority {
    fn default() -> Self {
        Self {
            identity_verified: false,
            receipt_persisted: false,
            execution_authorized: false,
            dispatch_performed: false,
            audit_published: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Owner {
    issuer: String,
    subject: String,
    tenant_id: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct TerminalAuthority {
    device_identity_verified: bool,
    command_persisted: bool,
    reservation_created: bool,
    execution_authorized: bool,
    dispatch_performed: bool,
    audit_published: bool,
}

impl Default for TerminalAuthority {
    fn default() -> Self {
        Self {
            device_identity_verified: false,
            command_persisted: false,
            reservation_created: false,
            execution_authorized: false,
            dispatch_performed: false,
            audit_published: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct TerminalReceiptObservation {
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
    authority: TerminalAuthority,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SessionObservation {
    schema_version: String,
    evaluation_mode: String,
    owner: Owner,
    conversation_id: String,
    prompt_id: String,
    run_id: String,
    receipt_observation: TerminalReceiptObservation,
    prompt_run_binding_valid: bool,
    receipt_binding_valid: bool,
    preview_only: bool,
    selected_target_id: Option<String>,
    authority: Authority,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Expected {
    command_id: String,
    command_sha256: String,
    attempt_id: String,
    target_id: String,
    disposition_kind: String,
    observed_at_ms: u64,
    uncertain: bool,
    reconciliation_required: bool,
    manual_review_required: bool,
    automatic_retry: bool,
    follow_up: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Vector {
    name: String,
    observation: SessionObservation,
    expected: Expected,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: String,
    evaluation_mode: String,
    authority: Authority,
    vectors: Vec<Vector>,
}

#[test]
fn session_runner_receipt_vectors_match_all_terminal_outcomes() {
    let fixture: Fixture = serde_json::from_str(FIXTURE).expect("strict fixture");
    assert_eq!(
        fixture.schema_version,
        "forge.session-runner-receipt-vectors/v1"
    );
    assert_eq!(
        fixture.evaluation_mode,
        "pure_session_runner_receipt_vectors_only"
    );
    assert_eq!(fixture.authority, Authority::default());
    assert_eq!(fixture.vectors.len(), 3);

    for vector in fixture.vectors {
        assert!(!vector.name.is_empty());
        validate(&vector).expect("valid session Runner receipt vector");
    }
}

#[test]
fn session_runner_receipt_vectors_reject_wire_and_outcome_drift() {
    let duplicate = FIXTURE.replace(
        "\"schema_version\": \"forge.session-runner-receipt-vectors/v1\",",
        "\"schema_version\": \"forge.session-runner-receipt-vectors/v1\", \"schema_version\": \"forge.session-runner-receipt-vectors/v1\",",
    );
    assert!(serde_json::from_str::<Fixture>(&duplicate).is_err());

    let unknown = FIXTURE.replace(
        "\"evaluation_mode\": \"pure_session_runner_receipt_vectors_only\",",
        "\"evaluation_mode\": \"pure_session_runner_receipt_vectors_only\", \"unexpected\": true,",
    );
    assert!(serde_json::from_str::<Fixture>(&unknown).is_err());
    let trailing = format!("{FIXTURE} {{}}");
    let mut stream = serde_json::Deserializer::from_str(&trailing);
    let _: Fixture = Fixture::deserialize(&mut stream).expect("first JSON value");
    assert!(stream.end().is_err());

    let fixture: Fixture = serde_json::from_str(FIXTURE).expect("fixture");
    let mut command_drift = fixture.vectors[0].observation.clone();
    command_drift.receipt_observation.command_id = "command-foreign".into();
    assert_ne!(
        command_drift.receipt_observation.command_id,
        fixture.vectors[0].expected.command_id
    );

    let mut retry = fixture.vectors[2].observation.clone();
    retry.receipt_observation.automatic_retry = true;
    assert!(validate(&Vector {
        name: "uncertain".into(),
        observation: retry,
        expected: fixture.vectors[2].expected.clone(),
    })
    .is_err());

    let mut selected = fixture.vectors[2].observation.clone();
    selected.selected_target_id = Some("runner-1".into());
    assert!(validate(&Vector {
        name: "uncertain".into(),
        observation: selected,
        expected: fixture.vectors[2].expected.clone(),
    })
    .is_err());
}

fn validate(vector: &Vector) -> Result<(), &'static str> {
    let observation = &vector.observation;
    let receipt = &observation.receipt_observation;
    let expected = &vector.expected;
    if observation.schema_version != "forge.session-runner-receipt-observation/v1"
        || observation.evaluation_mode != "pure_session_runner_receipt_binding_only"
        || !valid_text(&observation.owner.issuer)
        || !valid_text(&observation.owner.subject)
        || !valid_text(&observation.owner.tenant_id)
        || !valid_identifier(&observation.conversation_id)
        || !valid_identifier(&observation.prompt_id)
        || !valid_identifier(&observation.run_id)
        || !observation.prompt_run_binding_valid
        || !observation.receipt_binding_valid
        || !observation.preview_only
        || observation.selected_target_id.is_some()
        || observation.authority != Authority::default()
        || receipt.schema_version != "forge.runner-command-terminal-receipt/v1"
        || receipt.evaluation_mode != "pure_runner_command_receipt_only"
        || !valid_identifier(&receipt.command_id)
        || !valid_identifier(&receipt.attempt_id)
        || !valid_identifier(&receipt.target_id)
        || !valid_digest(&receipt.command_sha256)
        || receipt.observed_at_ms > MAX_SAFE_INTEGER
        || !receipt.receipt_valid
        || !receipt.preview_only
        || receipt.authority != TerminalAuthority::default()
        || receipt.command_id != expected.command_id
        || receipt.command_sha256 != expected.command_sha256
        || receipt.attempt_id != expected.attempt_id
        || receipt.target_id != expected.target_id
        || receipt.disposition_kind != expected.disposition_kind
        || receipt.observed_at_ms != expected.observed_at_ms
        || receipt.uncertain != expected.uncertain
        || receipt.reconciliation_required != expected.reconciliation_required
        || receipt.manual_review_required != expected.manual_review_required
        || receipt.automatic_retry != expected.automatic_retry
        || receipt.follow_up != expected.follow_up
    {
        return Err("invalid session Runner receipt vector");
    }
    let uncertain = receipt.disposition_kind == "uncertain";
    if !matches!(
        receipt.disposition_kind.as_str(),
        "completed" | "failed" | "uncertain"
    ) || receipt.uncertain != uncertain
        || receipt.reconciliation_required != uncertain
        || receipt.manual_review_required != uncertain
        || receipt.automatic_retry
        || (uncertain && receipt.follow_up != "reconciliation_manual")
        || (!uncertain && receipt.follow_up != "none")
    {
        return Err("inconsistent terminal outcome");
    }
    Ok(())
}

fn valid_text(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && value.trim() == value
        && !value.contains(['\0', '\r', '\n'])
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric() || (index > 0 && b"._:+/-".contains(&byte))
        })
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}
