//! Forge execution-lease checkpoints are consumed as bounded restart/fencing
//! evidence only. This receiver does not issue a lease, persist a terminal,
//! authorize execution, dispatch work, or publish Audit.

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};
use std::fmt;

const FIXTURE: &[u8] = include_bytes!("testdata/forge-execution-lease-checkpoint-v1.json");

#[derive(Debug, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Authority {
    lease_issued: bool,
    terminal_persisted: bool,
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
struct Proof {
    attempt_id: String,
    target_id: String,
    epoch: u64,
    fencing_token: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Disposition {
    kind: String,
    #[serde(default)]
    receipt_sha256: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    v: u16,
    proof: Proof,
    disposition: Disposition,
    observed_at_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    schema_version: String,
    evaluation_mode: String,
    grant: Grant,
    terminal: Option<Receipt>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Expected {
    accepted: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    terminal: bool,
    #[serde(default)]
    uncertain: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    checkpoint: Checkpoint,
    expected: Expected,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: String,
    evaluation_mode: String,
    authority: Authority,
    cases: Vec<Case>,
}

fn decode(raw: &[u8]) -> Result<Fixture, String> {
    reject_duplicate_json_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value = Fixture::deserialize(&mut decoder).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    if value.authority != Authority::default() {
        return Err("checkpoint authority is not disabled".into());
    }
    Ok(value)
}

fn checkpoint_valid(value: &Checkpoint) -> (bool, bool) {
    let grant = &value.grant;
    if value.schema_version != "forge.execution-lease-checkpoint/v1"
        || value.evaluation_mode != "pure_execution_lease_checkpoint_only"
        || grant.v != 1
        || grant.attempt_id.is_empty()
        || grant.target_id.is_empty()
        || grant.epoch == 0
        || grant.fencing_token.is_empty()
        || grant.expires_at_ms <= grant.issued_at_ms
    {
        return (false, false);
    }
    let Some(receipt) = &value.terminal else {
        return (true, false);
    };
    if receipt.v != 1
        || receipt.proof.attempt_id != grant.attempt_id
        || receipt.proof.target_id != grant.target_id
        || receipt.proof.epoch != grant.epoch
        || receipt.proof.fencing_token != grant.fencing_token
        || receipt.observed_at_ms < grant.issued_at_ms
        || receipt.observed_at_ms >= grant.expires_at_ms
    {
        return (false, false);
    }
    let uncertain = match receipt.disposition.kind.as_str() {
        "completed" => receipt
            .disposition
            .receipt_sha256
            .as_deref()
            .is_some_and(|digest| {
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            }),
        "failed" | "uncertain" => receipt
            .disposition
            .reason
            .as_deref()
            .is_some_and(|reason| !reason.is_empty()),
        _ => false,
    };
    if !uncertain {
        return (false, false);
    }
    (true, receipt.disposition.kind == "uncertain")
}

#[test]
fn canonical_fixture_is_bounded_and_authority_free() {
    let fixture = decode(FIXTURE).expect("canonical execution lease checkpoint fixture");
    assert_eq!(
        fixture.schema_version,
        "forge.execution-lease-checkpoint/v1"
    );
    assert_eq!(
        fixture.evaluation_mode,
        "pure_execution_lease_checkpoint_only"
    );
    assert_eq!(fixture.authority, Authority::default());
    assert_eq!(fixture.cases.len(), 4);

    let mut names = std::collections::BTreeSet::new();
    for case in fixture.cases {
        assert!(names.insert(case.name.clone()), "duplicate checkpoint case");
        let (accepted, uncertain) = checkpoint_valid(&case.checkpoint);
        assert_eq!(accepted, case.expected.accepted, "{}", case.name);
        if accepted {
            assert_eq!(
                case.checkpoint.terminal.is_some(),
                case.expected.terminal,
                "{}",
                case.name
            );
            assert_eq!(uncertain, case.expected.uncertain, "{}", case.name);
        }
        if !accepted {
            assert_eq!(case.expected.error.as_deref(), Some("invalid_checkpoint"));
        }
    }
    for name in [
        "empty_state",
        "completed_receipt_survives_restart",
        "uncertain_receipt_remains_terminal",
        "foreign_proof_rejected",
    ] {
        assert!(names.contains(name), "missing checkpoint case {name}");
    }
}

#[test]
fn unknown_authority_and_duplicate_fields_fail_closed() {
    let mut unknown: Map<String, Value> = serde_json::from_slice(FIXTURE).expect("object fixture");
    unknown.insert("unexpected".into(), Value::Bool(true));
    assert!(decode(&serde_json::to_vec(&unknown).expect("unknown mutation")).is_err());

    let mut authority: Map<String, Value> =
        serde_json::from_slice(FIXTURE).expect("object fixture");
    authority["authority"]["audit_published"] = Value::Bool(true);
    assert!(decode(&serde_json::to_vec(&authority).expect("authority mutation")).is_err());

    let duplicate = format!(
        "{},\"schema_version\":\"forge.execution-lease-checkpoint/v1\"}}",
        String::from_utf8_lossy(FIXTURE).trim_end_matches('}')
    );
    assert!(decode(duplicate.as_bytes()).is_err());
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
