//! The registry placement preview is a display-only v2 evaluation. Aero-IM
//! can verify its bounded value shape, but it does not publish an audit fact,
//! authenticate an instance, reserve capacity, schedule, dispatch, or run it.

use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;

const FIXTURE: &[u8] =
    include_bytes!("testdata/forge-device-inventory-registry-placement-preview-v1.json");
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_DECISIONS: usize = 128;
const MAX_REASONS: usize = 64;
const MAX_GPU_COUNT: u64 = 32;
const NOTICE: &str = "Every owner, state, timestamp, resource, GPU, reservation, residency, trust, sandbox, and concurrency value is an unverified caller declaration. This read-only comparison selects no target and grants no execution authority.";

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Owner {
    issuer: String,
    subject: String,
    tenant_id: String,
}

#[derive(Debug, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Authority {
    identity_verified: bool,
    heartbeat_persisted: bool,
    inventory_authoritative: bool,
    placement_selected: bool,
    reservation_created: bool,
    execution_authorized: bool,
    dispatch_performed: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Decision {
    revision: u64,
    generation: u64,
    heartbeat_sequence: u64,
    device_id: String,
    instance_id: String,
    reservation_state: String,
    gpu_count: u64,
    available_gpu_memory_bytes: u64,
    matches_requirements: bool,
    exclusion_reasons: Vec<String>,
    owner_declaration_unverified: bool,
    device_attributes_unverified: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Preview {
    schema_version: String,
    evaluation_mode: String,
    source_schema_version: String,
    evaluation_owner: Owner,
    evaluated_at_ms: u64,
    notice: String,
    decisions: Vec<Decision>,
    eligible_candidate_count: u64,
    selected_device_id: Option<String>,
    selected_instance_id: Option<String>,
    authority: Authority,
}

fn decode<T: DeserializeOwned>(raw: &[u8]) -> Result<T, String> {
    reject_duplicate_json_keys(raw)?;
    let object = serde_json::from_slice::<Value>(raw)
        .map_err(|error| error.to_string())?
        .as_object()
        .cloned()
        .ok_or_else(|| "registry placement response is not an object".to_owned())?;
    const FIELDS: [&str; 11] = [
        "schema_version",
        "evaluation_mode",
        "source_schema_version",
        "evaluation_owner",
        "evaluated_at_ms",
        "notice",
        "decisions",
        "eligible_candidate_count",
        "selected_device_id",
        "selected_instance_id",
        "authority",
    ];
    if object.len() != FIELDS.len() || FIELDS.iter().any(|field| !object.contains_key(*field)) {
        return Err("registry placement top-level fields are not exact".to_owned());
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

fn is_display_only(preview: &Preview) -> bool {
    preview.schema_version == "forge.device-inventory-placement-evaluation/v2"
        && preview.evaluation_mode == "offline_static_only"
        && preview.source_schema_version == "forge.device-inventory-observation/v2"
        && preview.evaluated_at_ms > 0
        && preview.evaluated_at_ms <= MAX_SAFE_INTEGER
        && preview.notice == NOTICE
        && valid_owner_part(&preview.evaluation_owner.issuer)
        && valid_owner_part(&preview.evaluation_owner.subject)
        && valid_owner_part(&preview.evaluation_owner.tenant_id)
        && preview.selected_device_id.is_none()
        && preview.selected_instance_id.is_none()
        && preview.authority == Authority::default()
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

fn valid_reason(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '.' | '_' | ':' | '+' | '/' | '-')
        })
}

#[test]
fn registry_placement_preview_fixture_is_strict_and_authority_free() {
    let preview: Preview = decode(FIXTURE).expect("decode registry placement fixture");
    assert!(is_display_only(&preview));
    assert_eq!(preview.decisions.len(), 2);
    assert!(preview.decisions.len() <= MAX_DECISIONS);
    assert!(preview.eligible_candidate_count <= MAX_DECISIONS as u64);

    let mut devices = HashSet::new();
    let mut instances = HashSet::new();
    let mut eligible = 0;
    for pair in preview.decisions.windows(2) {
        assert!(
            (pair[0].device_id.as_str(), pair[0].instance_id.as_str())
                < (pair[1].device_id.as_str(), pair[1].instance_id.as_str())
        );
    }
    for decision in &preview.decisions {
        assert!(decision.revision > 0 && decision.revision <= MAX_SAFE_INTEGER);
        assert!(decision.generation > 0 && decision.generation <= MAX_SAFE_INTEGER);
        assert!(decision.heartbeat_sequence > 0 && decision.heartbeat_sequence <= MAX_SAFE_INTEGER);
        assert!(valid_identifier(&decision.device_id) && valid_identifier(&decision.instance_id));
        assert!(devices.insert(&decision.device_id));
        assert!(instances.insert(&decision.instance_id));
        assert!(matches!(
            decision.reservation_state.as_str(),
            "none" | "reserved"
        ));
        assert!(decision.gpu_count <= MAX_GPU_COUNT);
        assert!(decision.available_gpu_memory_bytes <= MAX_SAFE_INTEGER);
        assert!(decision.owner_declaration_unverified);
        assert!(decision.device_attributes_unverified);
        assert_eq!(
            decision.matches_requirements,
            decision.exclusion_reasons.is_empty()
        );
        assert!(decision.exclusion_reasons.len() <= MAX_REASONS);
        assert!(decision
            .exclusion_reasons
            .windows(2)
            .all(|pair| pair[0] < pair[1]));
        assert!(decision
            .exclusion_reasons
            .iter()
            .all(|reason| valid_reason(reason)));
        if decision.matches_requirements {
            eligible += 1;
        }
    }
    assert_eq!(preview.eligible_candidate_count, eligible);

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
        "\"selected_device_id\": null",
        "\"selected_device_id\": \"device-a\"",
    );
    let selected = decode::<Preview>(selected_mutation.as_bytes()).unwrap();
    assert!(!is_display_only(&selected));

    let duplicate_mutation = String::from_utf8_lossy(FIXTURE).replacen(
        "\"schema_version\": \"forge.device-inventory-placement-evaluation/v2\",",
        "\"schema_version\": \"forge.device-inventory-placement-evaluation/v2\",\"schema_version\": \"forge.device-inventory-placement-evaluation/v2\",",
        1,
    );
    assert!(decode::<Preview>(duplicate_mutation.as_bytes()).is_err());

    let mut trailing = FIXTURE.to_vec();
    trailing.extend_from_slice(b" true");
    assert!(decode::<Preview>(&trailing).is_err());
}
