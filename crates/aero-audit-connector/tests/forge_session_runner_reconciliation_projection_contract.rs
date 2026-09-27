//! Strict receiver contract for the session Runner manual-reconciliation
//! projection. This value is metadata-only and never authorizes a retry,
//! target, lease, Runner effect, persistence, or Audit publication.

use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::collections::BTreeSet;
use std::fmt;

const FIXTURE: &[u8] =
    include_bytes!("testdata/forge-session-runner-reconciliation-projection-v1.json");
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_ATTEMPTS: u32 = 16;

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
struct Source {
    schema_version: String,
    owner: Owner,
    conversation_id: String,
    prompt_id: String,
    run_id: String,
    attempt_count: u32,
    latest_attempt_id: String,
    latest_command_id: String,
    latest_target_id: String,
    latest_disposition_kind: String,
    latest_observed_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct Projection {
    schema_version: String,
    evaluation_mode: String,
    owner: Owner,
    conversation_id: String,
    prompt_id: String,
    run_id: String,
    source: Source,
    latest_attempt_id: String,
    latest_command_id: String,
    latest_target_id: String,
    latest_disposition_kind: String,
    latest_observed_at_ms: u64,
    reconciliation_kind: String,
    reconciliation_reason: String,
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

fn validate(value: &Projection) -> Result<(), &'static str> {
    if value.schema_version != "forge.session-runner-reconciliation-projection/v1"
        || value.evaluation_mode != "pure_session_runner_reconciliation_projection_only"
        || !valid_owner(&value.owner)
        || !valid_identifier(&value.conversation_id)
        || !valid_identifier(&value.prompt_id)
        || !valid_identifier(&value.run_id)
        || value.source.schema_version != "forge.session-runner-receipt-history/v1"
        || !valid_owner(&value.source.owner)
        || value.source.owner != value.owner
        || value.source.conversation_id != value.conversation_id
        || value.source.prompt_id != value.prompt_id
        || value.source.run_id != value.run_id
        || value.source.attempt_count == 0
        || value.source.attempt_count > MAX_ATTEMPTS
        || !valid_identifier(&value.latest_attempt_id)
        || !valid_identifier(&value.latest_command_id)
        || !valid_identifier(&value.latest_target_id)
        || value.latest_disposition_kind != "uncertain"
        || value.latest_observed_at_ms > MAX_SAFE_INTEGER
        || value.source.latest_attempt_id != value.latest_attempt_id
        || value.source.latest_command_id != value.latest_command_id
        || value.source.latest_target_id != value.latest_target_id
        || value.source.latest_disposition_kind != value.latest_disposition_kind
        || value.source.latest_observed_at_ms != value.latest_observed_at_ms
        || value.reconciliation_kind != "manual"
        || value.reconciliation_reason != "uncertain_terminal_receipt"
        || !value.reconciliation_required
        || !value.manual_review_required
        || value.automatic_retry
        || value.follow_up != "reconciliation_manual"
        || value.selected_target_id.is_some()
        || !value.preview_only
        || value.authority != Authority::default()
    {
        return Err("invalid manual reconciliation projection");
    }
    Ok(())
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

fn assert_invalid<F>(base: &Projection, mutate: F)
where
    F: FnOnce(&mut Projection),
{
    let mut mutated = base.clone();
    mutate(&mut mutated);
    assert!(validate(&mutated).is_err());
}

#[test]
fn session_runner_reconciliation_projection_is_manual_and_authority_free() {
    let projection: Projection = decode(FIXTURE).expect("strict reconciliation projection");
    validate(&projection).expect("valid manual reconciliation projection");
    assert_eq!(projection.source.attempt_count, 2);
    assert_eq!(projection.latest_disposition_kind, "uncertain");
    assert!(projection.reconciliation_required);
    assert!(projection.manual_review_required);
    assert!(!projection.automatic_retry);
    assert_eq!(projection.follow_up, "reconciliation_manual");
    assert!(projection.selected_target_id.is_none());
}

#[test]
fn session_runner_reconciliation_projection_rejects_wire_and_semantic_drift() {
    let duplicate = String::from_utf8_lossy(FIXTURE).replacen(
        "\"schema_version\": \"forge.session-runner-reconciliation-projection/v1\",",
        "\"schema_version\": \"forge.session-runner-reconciliation-projection/v1\", \"schema_version\": \"forge.session-runner-reconciliation-projection/v1\",",
        1,
    );
    assert!(decode::<Projection>(duplicate.as_bytes()).is_err());

    let unknown = String::from_utf8_lossy(FIXTURE).replacen(
        "\"evaluation_mode\": \"pure_session_runner_reconciliation_projection_only\",",
        "\"evaluation_mode\": \"pure_session_runner_reconciliation_projection_only\", \"unexpected\": true,",
        1,
    );
    assert!(decode::<Projection>(unknown.as_bytes()).is_err());
    let trailing = [FIXTURE, b" {}"].concat();
    assert!(decode::<Projection>(&trailing).is_err());

    let base: Projection = decode(FIXTURE).expect("fixture");
    assert_invalid(&base, |value| value.owner.subject = "user-foreign".into());
    assert_invalid(&base, |value| {
        value.conversation_id = "conversation-foreign".into()
    });
    assert_invalid(&base, |value| value.prompt_id = "prompt-foreign".into());
    assert_invalid(&base, |value| value.run_id = "run-foreign".into());
    assert_invalid(&base, |value| {
        value.source.schema_version = "forge.other-history/v1".into()
    });
    assert_invalid(&base, |value| value.source.attempt_count = 0);
    assert_invalid(&base, |value| {
        value.latest_attempt_id = "attempt-foreign".into()
    });
    assert_invalid(&base, |value| {
        value.latest_command_id = "command-foreign".into()
    });
    assert_invalid(&base, |value| {
        value.latest_target_id = "runner-foreign".into()
    });
    assert_invalid(&base, |value| {
        value.latest_disposition_kind = "failed".into()
    });
    assert_invalid(&base, |value| value.latest_observed_at_ms = 100);
    assert_invalid(&base, |value| {
        value.reconciliation_kind = "automatic".into()
    });
    assert_invalid(&base, |value| value.reconciliation_reason = "retry".into());
    assert_invalid(&base, |value| value.reconciliation_required = false);
    assert_invalid(&base, |value| value.manual_review_required = false);
    assert_invalid(&base, |value| value.automatic_retry = true);
    assert_invalid(&base, |value| value.follow_up = "none".into());
    assert_invalid(&base, |value| {
        value.selected_target_id = Some("runner-2".into())
    });
    assert_invalid(&base, |value| value.authority.identity_verified = true);
}
