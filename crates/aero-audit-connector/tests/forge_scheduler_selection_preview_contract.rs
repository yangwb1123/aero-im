//! The scheduler-selection preview is a deterministic display value. Aero-IM
//! verifies its envelope for interoperability and never turns it into an
//! audit fact, lease, reservation, dispatch, or execution decision.

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;

const FIXTURE: &[u8] = include_bytes!("testdata/forge-scheduler-selection-preview-v1.json");
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_CANDIDATES: u64 = 128;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Owner {
    issuer: String,
    subject: String,
    tenant_id: String,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Authority {
    placement_selected: bool,
    reservation_created: bool,
    lease_issued: bool,
    execution_authorized: bool,
    dispatch_performed: bool,
    audit_published: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Preview {
    schema_version: String,
    evaluation_mode: String,
    owner: Owner,
    conversation_id: String,
    run_id: String,
    attempt_id: String,
    evaluated_at_ms: u64,
    candidate_count: u64,
    eligible_candidate_count: u64,
    selection_available: bool,
    selection_reason: String,
    selected_device_id: Option<String>,
    selected_instance_id: Option<String>,
    preview_only: bool,
    authority: Authority,
}

fn decode<T: for<'de> Deserialize<'de>>(raw: &[u8]) -> Result<T, String> {
    reject_duplicate_json_keys(raw)?;
    let object = serde_json::from_slice::<Value>(raw)
        .map_err(|error| error.to_string())?
        .as_object()
        .cloned()
        .ok_or_else(|| "scheduler selection response is not an object".to_owned())?;
    const FIELDS: [&str; 15] = [
        "schema_version",
        "evaluation_mode",
        "owner",
        "conversation_id",
        "run_id",
        "attempt_id",
        "evaluated_at_ms",
        "candidate_count",
        "eligible_candidate_count",
        "selection_available",
        "selection_reason",
        "selected_device_id",
        "selected_instance_id",
        "preview_only",
        "authority",
    ];
    if object.len() != FIELDS.len() || FIELDS.iter().any(|field| !object.contains_key(*field)) {
        return Err("scheduler selection top-level fields are not exact".to_owned());
    }
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
        let mut keys = HashSet::new();
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

fn valid_owner_part(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn valid_identifier(value: &str) -> bool {
    if value.is_empty() || value.len() > 128 {
        return false;
    }
    value.chars().enumerate().all(|(index, character)| {
        character.is_ascii_alphanumeric()
            || (index > 0 && matches!(character, '.' | '_' | ':' | '-'))
    })
}

fn is_display_only(preview: &Preview) -> bool {
    if preview.schema_version != "forge.scheduler-selection-preview/v1"
        || preview.evaluation_mode != "pure_scheduler_selection_preview"
        || !valid_owner_part(&preview.owner.issuer)
        || !valid_owner_part(&preview.owner.subject)
        || !valid_owner_part(&preview.owner.tenant_id)
        || !valid_identifier(&preview.conversation_id)
        || !valid_identifier(&preview.run_id)
        || !valid_identifier(&preview.attempt_id)
        || preview.evaluated_at_ms == 0
        || preview.evaluated_at_ms > MAX_SAFE_INTEGER
        || preview.candidate_count > MAX_CANDIDATES
        || preview.eligible_candidate_count > preview.candidate_count
        || !preview.preview_only
        || preview.authority != Authority::default()
    {
        return false;
    }
    if preview.selection_available {
        preview.eligible_candidate_count > 0
            && preview.selection_reason == "first_sorted_eligible_candidate"
            && preview
                .selected_device_id
                .as_deref()
                .is_some_and(valid_identifier)
            && preview
                .selected_instance_id
                .as_deref()
                .is_some_and(valid_identifier)
    } else {
        preview.selection_reason == "no_eligible_candidate"
            && preview.selected_device_id.is_none()
            && preview.selected_instance_id.is_none()
    }
}

#[test]
fn scheduler_selection_preview_fixture_is_strict_and_authority_free() {
    let preview: Preview = decode(FIXTURE).expect("decode scheduler selection fixture");
    assert!(is_display_only(&preview));
    assert_eq!(preview.candidate_count, 2);
    assert_eq!(preview.eligible_candidate_count, 1);
    assert_eq!(preview.selected_device_id.as_deref(), Some("device-a"));
    assert_eq!(preview.selected_instance_id.as_deref(), Some("runner-a"));

    let mut unknown = serde_json::from_slice::<Value>(FIXTURE).unwrap();
    unknown["unexpected"] = Value::Bool(true);
    assert!(decode::<Preview>(&serde_json::to_vec(&unknown).unwrap()).is_err());

    let authority_mutation = String::from_utf8_lossy(FIXTURE).replace(
        "\"dispatch_performed\": false",
        "\"dispatch_performed\": true",
    );
    let authority = decode::<Preview>(authority_mutation.as_bytes()).unwrap();
    assert!(!is_display_only(&authority));

    let selected_mutation = String::from_utf8_lossy(FIXTURE).replace(
        "\"selected_device_id\": \"device-a\"",
        "\"selected_device_id\": \"device\\n-a\"",
    );
    let selected = decode::<Preview>(selected_mutation.as_bytes()).unwrap();
    assert!(!is_display_only(&selected));

    let duplicate_mutation = String::from_utf8_lossy(FIXTURE).replacen(
        "\"schema_version\": \"forge.scheduler-selection-preview/v1\",",
        "\"schema_version\": \"forge.scheduler-selection-preview/v1\",\"schema_version\": \"forge.scheduler-selection-preview/v1\",",
        1,
    );
    assert!(decode::<Preview>(duplicate_mutation.as_bytes()).is_err());

    let mut trailing = FIXTURE.to_vec();
    trailing.extend_from_slice(b" true");
    assert!(decode::<Preview>(&trailing).is_err());
}
