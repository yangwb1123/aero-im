//! Session-bound Runner receipt history is a strict, read-only receiver value.
//! It validates owner/session binding, ordered attempts, terminal lifecycle,
//! derived summary fields, and the all-false authority boundary.

use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::collections::BTreeSet;
use std::fmt;

const FIXTURE: &[u8] = include_bytes!("testdata/forge-session-runner-receipt-history-v1.json");
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_RECEIPTS: usize = 16;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct Owner {
    issuer: String,
    subject: String,
    tenant_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
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
    authority: TerminalAuthority,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct ReceiptObservation {
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
    authority: Authority,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct History {
    schema_version: String,
    evaluation_mode: String,
    owner: Owner,
    conversation_id: String,
    prompt_id: String,
    run_id: String,
    receipts: Vec<ReceiptObservation>,
    attempt_count: u32,
    latest_attempt_id: String,
    latest_command_id: String,
    latest_target_id: String,
    latest_disposition_kind: String,
    latest_observed_at_ms: u64,
    reconciliation_required: bool,
    manual_review_required: bool,
    automatic_retry: bool,
    follow_up: String,
    selected_target_id: Option<String>,
    preview_only: bool,
    authority: Authority,
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

fn validate(history: &History) -> Result<(), &'static str> {
    if history.schema_version != "forge.session-runner-receipt-history/v1"
        || history.evaluation_mode != "pure_session_runner_receipt_history_only"
        || !valid_owner(&history.owner)
        || !valid_identifier(&history.conversation_id)
        || !valid_identifier(&history.prompt_id)
        || !valid_identifier(&history.run_id)
        || history.receipts.is_empty()
        || history.receipts.len() > MAX_RECEIPTS
        || history.authority != Authority::default()
        || history.selected_target_id.is_some()
        || !history.preview_only
        || history.automatic_retry
    {
        return Err("invalid history envelope");
    }

    let mut attempts = BTreeSet::new();
    for (index, receipt) in history.receipts.iter().enumerate() {
        if validate_observation(receipt).is_err()
            || receipt.owner != history.owner
            || receipt.conversation_id != history.conversation_id
            || receipt.prompt_id != history.prompt_id
            || receipt.run_id != history.run_id
            || !attempts.insert(receipt.receipt_observation.attempt_id.clone())
        {
            return Err("invalid receipt binding");
        }
        if index > 0 {
            let previous = &history.receipts[index - 1];
            if receipt_after(previous, receipt)
                || matches!(
                    previous.receipt_observation.disposition_kind.as_str(),
                    "completed" | "uncertain"
                )
            {
                return Err("invalid receipt lifecycle");
            }
        }
    }

    let latest = &history
        .receipts
        .last()
        .expect("non-empty history")
        .receipt_observation;
    let uncertain = latest.uncertain;
    let follow_up = if uncertain {
        "reconciliation_manual"
    } else {
        "none"
    };
    if history.attempt_count != history.receipts.len() as u32
        || history.latest_attempt_id != latest.attempt_id
        || history.latest_command_id != latest.command_id
        || history.latest_target_id != latest.target_id
        || history.latest_disposition_kind != latest.disposition_kind
        || history.latest_observed_at_ms != latest.observed_at_ms
        || history.reconciliation_required != uncertain
        || history.manual_review_required != uncertain
        || history.follow_up != follow_up
    {
        return Err("history summary drift");
    }
    Ok(())
}

fn validate_observation(observation: &ReceiptObservation) -> Result<(), &'static str> {
    let receipt = &observation.receipt_observation;
    if observation.schema_version != "forge.session-runner-receipt-observation/v1"
        || observation.evaluation_mode != "pure_session_runner_receipt_binding_only"
        || !valid_owner(&observation.owner)
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
    {
        return Err("invalid receipt observation");
    }
    let uncertain = receipt.disposition_kind == "uncertain";
    if receipt.disposition_kind != "completed" && receipt.disposition_kind != "failed" && !uncertain
    {
        return Err("unknown disposition");
    }
    if receipt.uncertain != uncertain
        || receipt.reconciliation_required != uncertain
        || receipt.manual_review_required != uncertain
        || receipt.automatic_retry
        || (uncertain && receipt.follow_up != "reconciliation_manual")
        || (!uncertain && receipt.follow_up != "none")
    {
        return Err("inconsistent outcome flags");
    }
    Ok(())
}

fn receipt_after(left: &ReceiptObservation, right: &ReceiptObservation) -> bool {
    let left_time = left.receipt_observation.observed_at_ms;
    let right_time = right.receipt_observation.observed_at_ms;
    left_time > right_time
        || (left_time == right_time
            && left.receipt_observation.attempt_id >= right.receipt_observation.attempt_id)
}

fn valid_owner(owner: &Owner) -> bool {
    valid_owner_part(&owner.issuer)
        && valid_owner_part(&owner.subject)
        && valid_owner_part(&owner.tenant_id)
}

fn valid_owner_part(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && value.trim() == value
        && !value
            .chars()
            .any(|character| character == '\0' || character == '\r' || character == '\n')
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.chars().enumerate().all(|(index, character)| {
            character.is_ascii_alphanumeric() || (index > 0 && "._:+/-".contains(character))
        })
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value.chars().all(|character| character.is_ascii_hexdigit())
        && value == value.to_ascii_lowercase()
}

fn set_disposition(observation: &mut ReceiptObservation, disposition: &str) {
    let uncertain = disposition == "uncertain";
    let receipt = &mut observation.receipt_observation;
    receipt.disposition_kind = disposition.into();
    receipt.uncertain = uncertain;
    receipt.reconciliation_required = uncertain;
    receipt.manual_review_required = uncertain;
    receipt.automatic_retry = false;
    receipt.follow_up = if uncertain {
        "reconciliation_manual".into()
    } else {
        "none".into()
    };
}

fn assert_invalid<F>(base: &History, mutate: F)
where
    F: FnOnce(&mut History),
{
    let mut mutated = base.clone();
    mutate(&mut mutated);
    assert!(validate(&mutated).is_err());
}

#[test]
fn session_runner_receipt_history_is_ordered_and_reduces_to_manual_boundary() {
    let history: History = decode(FIXTURE).expect("strict session receipt history fixture");
    validate(&history).expect("valid session receipt history");
    assert_eq!(history.receipts.len(), 2);
    assert_eq!(history.attempt_count, 2);
    assert_eq!(history.latest_attempt_id, "attempt-002");
    assert_eq!(history.latest_disposition_kind, "uncertain");
    assert!(history.reconciliation_required);
    assert!(history.manual_review_required);
    assert!(!history.automatic_retry);
    assert_eq!(history.follow_up, "reconciliation_manual");
}

#[test]
fn session_runner_receipt_history_rejects_wire_and_lifecycle_drift() {
    let duplicate = String::from_utf8_lossy(FIXTURE).replacen(
        "\"schema_version\": \"forge.session-runner-receipt-history/v1\",",
        "\"schema_version\": \"forge.session-runner-receipt-history/v1\", \"schema_version\": \"forge.session-runner-receipt-history/v1\",",
        1,
    );
    assert!(decode::<History>(duplicate.as_bytes()).is_err());

    let unknown = String::from_utf8_lossy(FIXTURE).replacen(
        "\"evaluation_mode\": \"pure_session_runner_receipt_history_only\",",
        "\"evaluation_mode\": \"pure_session_runner_receipt_history_only\", \"unexpected\": true,",
        1,
    );
    assert!(decode::<History>(unknown.as_bytes()).is_err());
    let trailing = [FIXTURE, b" {}"].concat();
    assert!(decode::<History>(&trailing).is_err());

    let base: History = decode(FIXTURE).expect("fixture");
    assert_invalid(&base, |history| {
        history.receipts[1].owner.subject = "user-foreign".into();
    });
    assert_invalid(&base, |history| {
        history.receipts[1].conversation_id = "conversation-foreign".into();
    });
    assert_invalid(&base, |history| {
        history.receipts[1].prompt_id = "prompt-foreign".into();
    });
    assert_invalid(&base, |history| {
        history.receipts[1].run_id = "run-foreign".into();
    });
    assert_invalid(&base, |history| {
        history.receipts[1].receipt_observation.observed_at_ms = 50;
    });
    assert_invalid(&base, |history| {
        history.receipts[1].receipt_observation.attempt_id =
            history.receipts[0].receipt_observation.attempt_id.clone();
    });
    assert_invalid(&base, |history| {
        set_disposition(&mut history.receipts[0], "completed");
    });
    assert_invalid(&base, |history| {
        set_disposition(&mut history.receipts[0], "uncertain");
    });
    assert_invalid(&base, |history| {
        history.attempt_count += 1;
    });
    assert_invalid(&base, |history| {
        history.selected_target_id = Some("runner-2".into());
    });
    assert_invalid(&base, |history| {
        history.authority.receipt_persisted = true;
    });
    assert_invalid(&base, |history| {
        history.receipts[0]
            .receipt_observation
            .authority
            .command_persisted = true;
    });
}
