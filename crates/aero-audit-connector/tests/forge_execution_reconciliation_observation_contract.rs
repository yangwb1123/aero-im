//! Forge execution reconciliation is a read-only restart observation.  This
//! receiver validates the canonical value without authenticating a caller,
//! persisting a Run/Attempt/receipt, issuing a lease, selecting a target,
//! scheduling/dispatching work, executing a process, or publishing Audit.

use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::fmt;

const FIXTURE: &[u8] =
    include_bytes!("testdata/forge-execution-reconciliation-observation-v1.json");
const SCHEMA_VERSION: &str = "forge.execution-reconciliation-observation/v1";
const EVALUATION_MODE: &str = "pure_execution_reconciliation_observation";

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Owner {
    issuer: String,
    subject: String,
    tenant_id: String,
}

#[derive(Debug, Deserialize, Default, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Authority {
    identity_verified: bool,
    run_authoritative: bool,
    attempt_persisted: bool,
    lease_issued: bool,
    terminal_persisted: bool,
    reservation_created: bool,
    execution_authorized: bool,
    dispatch_performed: bool,
    audit_published: bool,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Lease {
    v: u8,
    attempt_id: String,
    target_id: String,
    epoch: u64,
    fencing_token: String,
    issued_at_ms: i64,
    expires_at_ms: i64,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Proof {
    attempt_id: String,
    target_id: String,
    epoch: u64,
    fencing_token: String,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Disposition {
    kind: String,
    #[serde(default)]
    receipt_sha256: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Terminal {
    v: u8,
    proof: Proof,
    disposition: Disposition,
    observed_at_ms: i64,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Input {
    owner: Owner,
    conversation_id: String,
    run_id: String,
    attempt_id: String,
    command_id: String,
    target_id: String,
    run_status: String,
    attempt_state: String,
    lease: Lease,
    observed_at_ms: i64,
    terminal: Option<Terminal>,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Expected {
    accepted: bool,
    #[serde(default)]
    next_observation: String,
    #[serde(default)]
    lease_active: bool,
    #[serde(default)]
    terminal_observed: bool,
    #[serde(default)]
    terminal_disposition: String,
    #[serde(default)]
    terminal_state_aligned: bool,
    #[serde(default)]
    reconciliation_required: bool,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    input: Input,
    expected: Expected,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: String,
    evaluation_mode: String,
    authority: Authority,
    cases: Vec<Case>,
}

fn decode<T: DeserializeOwned>(raw: &[u8]) -> Result<T, String> {
    reject_duplicate_json_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value = T::deserialize(&mut decoder).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    Ok(value)
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

fn observe(input: &Input) -> Result<Expected, String> {
    if input.owner.issuer.is_empty()
        || input.owner.subject.is_empty()
        || input.owner.tenant_id.is_empty()
        || input.conversation_id.is_empty()
        || input.run_id.is_empty()
        || input.attempt_id.is_empty()
        || input.command_id.is_empty()
        || input.target_id.is_empty()
        || input.run_status != "nonterminal"
        || input.lease.v != 1
        || input.lease.attempt_id != input.attempt_id
        || input.lease.target_id != input.target_id
        || input.lease.epoch == 0
        || input.lease.fencing_token.is_empty()
        || input.lease.issued_at_ms <= 0
        || input.lease.expires_at_ms <= input.lease.issued_at_ms
        || input.observed_at_ms < input.lease.issued_at_ms
    {
        return Err("invalid execution reconciliation bindings".into());
    }
    if !matches!(
        input.attempt_state.as_str(),
        "running" | "completed" | "failed" | "uncertain"
    ) {
        return Err("invalid attempt state".into());
    }

    let active = input.observed_at_ms < input.lease.expires_at_ms;
    let mut expected = Expected {
        accepted: true,
        next_observation: String::new(),
        lease_active: active,
        terminal_observed: false,
        terminal_disposition: "none".into(),
        terminal_state_aligned: true,
        reconciliation_required: false,
    };

    if let Some(terminal) = input.terminal.as_ref() {
        if terminal.v != 1
            || terminal.observed_at_ms < input.lease.issued_at_ms
            || terminal.observed_at_ms > input.observed_at_ms
            || terminal.observed_at_ms >= input.lease.expires_at_ms
            || terminal.proof.attempt_id != input.attempt_id
            || terminal.proof.target_id != input.target_id
            || terminal.proof.epoch != input.lease.epoch
            || terminal.proof.fencing_token != input.lease.fencing_token
            || !matches!(
                terminal.disposition.kind.as_str(),
                "completed" | "failed" | "uncertain"
            )
        {
            return Err("terminal proof or time binding mismatch".into());
        }
        expected.terminal_observed = true;
        expected.terminal_disposition = terminal.disposition.kind.clone();
        expected.next_observation = match terminal.disposition.kind.as_str() {
            "completed" => {
                expected.terminal_state_aligned = input.attempt_state == "completed";
                "terminal_completed"
            }
            "failed" => {
                expected.terminal_state_aligned = input.attempt_state == "failed";
                "terminal_failed"
            }
            "uncertain" => {
                expected.terminal_state_aligned = input.attempt_state == "uncertain";
                "terminal_uncertain"
            }
            _ => unreachable!(),
        }
        .into();
        if !expected.terminal_state_aligned {
            expected.next_observation = "terminal_state_conflict".into();
        }
        expected.reconciliation_required =
            !expected.terminal_state_aligned || expected.terminal_disposition == "uncertain";
        return Ok(expected);
    }

    if input.attempt_state != "running" {
        expected.next_observation = "attempt_terminal_without_receipt".into();
        expected.reconciliation_required = true;
    } else if !active {
        expected.next_observation = "lease_expired_without_terminal".into();
        expected.reconciliation_required = true;
    } else {
        expected.next_observation = "await_terminal".into();
    }
    Ok(expected)
}

fn validate_fixture(fixture: &Fixture) -> Result<(), String> {
    if fixture.schema_version != SCHEMA_VERSION
        || fixture.evaluation_mode != EVALUATION_MODE
        || fixture.authority != Authority::default()
        || fixture.cases.len() != 6
    {
        return Err("invalid execution reconciliation envelope".into());
    }
    let mut names = BTreeSet::new();
    for case in &fixture.cases {
        if case.name.is_empty() || !names.insert(case.name.as_str()) {
            return Err(format!("duplicate or empty case name: {}", case.name));
        }
        let actual = observe(&case.input);
        if !case.expected.accepted {
            if actual.is_ok() {
                return Err(format!("case {} accepted unexpectedly", case.name));
            }
            continue;
        }
        let actual = actual.map_err(|error| format!("case {}: {error}", case.name))?;
        if actual != case.expected {
            return Err(format!(
                "case {} actual={actual:?} expected={:?}",
                case.name, case.expected
            ));
        }
    }
    Ok(())
}

#[test]
fn canonical_execution_reconciliation_fixture_is_read_only_and_authority_free() {
    let fixture: Fixture = decode(FIXTURE).expect("canonical execution reconciliation fixture");
    validate_fixture(&fixture).expect("valid execution reconciliation fixture");
    let uncertain = &fixture.cases[3];
    assert_eq!(uncertain.name, "uncertain_terminal_requires_manual_review");
    assert_eq!(uncertain.expected.next_observation, "terminal_uncertain");
    assert!(uncertain.expected.reconciliation_required);
}

#[test]
fn execution_reconciliation_rejects_unknown_duplicate_authority_proof_and_retry_mutations() {
    let mut unknown: Map<String, Value> = serde_json::from_slice(FIXTURE).unwrap();
    unknown.insert("unexpected".into(), Value::Bool(true));
    assert!(decode::<Fixture>(&serde_json::to_vec(&unknown).unwrap()).is_err());

    let duplicate = format!(
        "{},\"schema_version\":\"{SCHEMA_VERSION}\"}}",
        String::from_utf8_lossy(FIXTURE).trim_end_matches('}')
    );
    assert!(decode::<Fixture>(duplicate.as_bytes()).is_err());

    let mut authority: Value = serde_json::from_slice(FIXTURE).unwrap();
    authority["authority"]["execution_authorized"] = Value::Bool(true);
    let authority_bytes = serde_json::to_vec(&authority).unwrap();
    let decoded: Fixture = decode(&authority_bytes).unwrap();
    assert!(validate_fixture(&decoded).is_err());

    let mut proof: Value = serde_json::from_slice(FIXTURE).unwrap();
    proof["cases"][2]["input"]["terminal"]["proof"]["target_id"] =
        Value::String("foreign-runner".into());
    let proof_bytes = serde_json::to_vec(&proof).unwrap();
    let decoded: Fixture = decode(&proof_bytes).unwrap();
    assert!(validate_fixture(&decoded).is_err());

    let mut retry: Map<String, Value> = serde_json::from_slice(FIXTURE).unwrap();
    retry.insert("automatic_retry".into(), Value::Bool(true));
    assert!(decode::<Fixture>(&serde_json::to_vec(&retry).unwrap()).is_err());
}
