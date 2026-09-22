//! The canonical snapshot is a bounded owner-scoped value projection only.
//!
//! This receiver sorts and fingerprints caller-supplied rows deterministically.
//! It does not authenticate the owner, discover or persist inventory, establish
//! freshness, select or reserve a target, dispatch work, or grant authority.

#![allow(dead_code)]

use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt;

const FIXTURE: &[u8] = include_bytes!("testdata/forge-device-inventory-snapshot-canonical-v1.json");
const SNAPSHOT_CANONICAL_DOMAIN: &str = "forge.device-inventory-snapshot-canonical/v1";
const MAX_SNAPSHOT_ROWS: usize = 128;
const MAX_SNAPSHOT_IDENTIFIER: usize = 128;
const MAX_SNAPSHOT_OWNER_BYTES: usize = 512;

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Owner {
    issuer: String,
    subject: String,
    tenant_id: String,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Row {
    device_id: String,
    instance_id: String,
    owner: Owner,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    snapshot_id: String,
    observed_at_ms: u64,
    owner: Owner,
    rows: Vec<Row>,
}

#[derive(Debug, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Authority {
    identity_verified: bool,
    heartbeat_persisted: bool,
    inventory_authoritative: bool,
    reservation_created: bool,
    execution_authorized: bool,
    dispatch_performed: bool,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Expected {
    #[serde(default)]
    ordered_keys: Option<Vec<String>>,
    #[serde(default)]
    canonical_sha256: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    input: Snapshot,
    expected: Expected,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: String,
    evaluation_mode: String,
    owner_declaration_unverified: bool,
    inventory_declarations_unverified: bool,
    authority: Authority,
    cases: Vec<Case>,
}

fn decode<T: DeserializeOwned>(raw: &[u8]) -> Result<T, String> {
    reject_duplicate_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value = T::deserialize(&mut decoder).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    Ok(value)
}

fn canonicalize(value: &Snapshot) -> Result<Snapshot, &'static str> {
    if !valid_identifier(&value.snapshot_id) {
        return Err("invalid_snapshot_id");
    }
    if value.observed_at_ms == 0 {
        return Err("invalid_observed_at");
    }
    if !valid_owner(&value.owner) {
        return Err("invalid_owner");
    }
    if value.rows.len() > MAX_SNAPSHOT_ROWS {
        return Err("too_many_rows");
    }

    let mut rows = value.rows.clone();
    for row in &rows {
        if !valid_identifier(&row.device_id) || !valid_identifier(&row.instance_id) {
            return Err("invalid_snapshot_id");
        }
        if row.owner != value.owner {
            return Err("owner_mismatch");
        }
        if !valid_owner(&row.owner) {
            return Err("invalid_owner");
        }
    }
    rows.sort_by(|left, right| {
        left.device_id
            .cmp(&right.device_id)
            .then_with(|| left.instance_id.cmp(&right.instance_id))
    });
    for pair in rows.windows(2) {
        if pair[0].device_id == pair[1].device_id && pair[0].instance_id == pair[1].instance_id {
            return Err("duplicate_row");
        }
    }
    Ok(Snapshot {
        snapshot_id: value.snapshot_id.clone(),
        observed_at_ms: value.observed_at_ms,
        owner: value.owner.clone(),
        rows,
    })
}

fn snapshot_digest(value: &Snapshot) -> Result<String, &'static str> {
    let canonical = canonicalize(value)?;
    let mut hasher = Sha256::new();
    hasher.update(SNAPSHOT_CANONICAL_DOMAIN.as_bytes());
    hasher.update([0]);
    append_field(&mut hasher, &canonical.snapshot_id);
    append_field(&mut hasher, &canonical.observed_at_ms.to_string());
    append_owner(&mut hasher, &canonical.owner);
    for row in &canonical.rows {
        append_field(&mut hasher, &row.device_id);
        append_field(&mut hasher, &row.instance_id);
        append_owner(&mut hasher, &row.owner);
    }
    Ok(hex_lower(&hasher.finalize()))
}

fn append_owner(hasher: &mut Sha256, owner: &Owner) {
    append_field(hasher, &owner.issuer);
    append_field(hasher, &owner.subject);
    append_field(hasher, &owner.tenant_id);
}

fn append_field(hasher: &mut Sha256, value: &str) {
    hasher.update(value.len().to_string().as_bytes());
    hasher.update([b':']);
    hasher.update(value.as_bytes());
    hasher.update([0]);
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn valid_owner(owner: &Owner) -> bool {
    valid_owner_part(&owner.issuer)
        && valid_owner_part(&owner.subject)
        && valid_owner_part(&owner.tenant_id)
}

fn valid_owner_part(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SNAPSHOT_OWNER_BYTES
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn valid_identifier(value: &str) -> bool {
    if value.is_empty() || value.len() > MAX_SNAPSHOT_IDENTIFIER {
        return false;
    }
    for (index, character) in value.chars().enumerate() {
        let allowed = character.is_ascii_alphanumeric()
            || (index > 0 && matches!(character, '.' | '_' | ':' | '-'));
        if !allowed {
            return false;
        }
    }
    true
}

fn ordered_keys(rows: &[Row]) -> Vec<String> {
    rows.iter()
        .map(|row| format!("{}/{}", row.device_id, row.instance_id))
        .collect()
}

fn validate_fixture(fixture: &Fixture) -> Result<(), String> {
    if fixture.schema_version != "forge.device-inventory-snapshot-canonical/v1"
        || fixture.evaluation_mode != "pure_owner_scoped_snapshot_only"
        || !fixture.owner_declaration_unverified
        || !fixture.inventory_declarations_unverified
        || fixture.authority != Authority::default()
        || fixture.cases.len() != 6
    {
        return Err("invalid envelope".into());
    }
    let names = [
        "sorts_by_device_then_instance_without_mutating_input",
        "empty_snapshot_has_stable_digest",
        "foreign_owner_row_rejected",
        "duplicate_composite_row_rejected",
        "invalid_snapshot_id_rejected",
        "zero_observed_time_rejected",
    ];
    let mut seen = BTreeSet::new();
    for (index, case) in fixture.cases.iter().enumerate() {
        if case.name != names[index] || !seen.insert(case.name.as_str()) {
            return Err(format!("unexpected case order at {index}"));
        }
        let result = canonicalize(&case.input);
        if let Some(error) = case.expected.error.as_deref() {
            if case.expected.ordered_keys.is_some() || case.expected.canonical_sha256.is_some() {
                return Err(format!("rejected case {} has accepted fields", case.name));
            }
            if result.as_ref().err().copied() != Some(error) {
                return Err(format!("case {} error mismatch", case.name));
            }
            continue;
        }
        let canonical = result.map_err(|error| format!("case {}: {error}", case.name))?;
        if case.expected.error.is_some()
            || case.expected.ordered_keys.as_ref() != Some(&ordered_keys(&canonical.rows))
            || case.expected.canonical_sha256.as_deref()
                != Some(
                    snapshot_digest(&case.input)
                        .map_err(str::to_owned)?
                        .as_str(),
                )
        {
            return Err(format!("case {} canonical projection mismatch", case.name));
        }
    }
    Ok(())
}

fn reject_duplicate_keys(raw: &[u8]) -> Result<(), String> {
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

#[test]
fn canonical_snapshot_projection_is_strict_sorted_digest_bound_and_authority_free() {
    let fixture: Fixture = decode(FIXTURE).expect("canonical snapshot fixture");
    validate_fixture(&fixture).expect("valid canonical snapshot fixture");
    assert!(fixture.owner_declaration_unverified);
    assert!(fixture.inventory_declarations_unverified);
    assert_eq!(
        fixture.cases[0]
            .expected
            .ordered_keys
            .as_ref()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        fixture.cases[1]
            .expected
            .ordered_keys
            .as_ref()
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn snapshot_projection_rejects_unknown_duplicate_digest_and_all_authority_mutations() {
    let mut mutations: Vec<(&str, Box<dyn Fn(&mut Value)>)> = vec![
        (
            "unknown_root",
            Box::new(|root| {
                root.as_object_mut()
                    .expect("root object")
                    .insert("unexpected".into(), true.into());
            }),
        ),
        (
            "unknown_row",
            Box::new(|root| {
                root["cases"][0]["input"]["rows"][0]["unexpected"] = true.into();
            }),
        ),
        (
            "owner_declaration_verified",
            Box::new(|root| root["owner_declaration_unverified"] = false.into()),
        ),
        (
            "inventory_declarations_verified",
            Box::new(|root| root["inventory_declarations_unverified"] = false.into()),
        ),
    ];
    for key in [
        "identity_verified",
        "heartbeat_persisted",
        "inventory_authoritative",
        "reservation_created",
        "execution_authorized",
        "dispatch_performed",
    ] {
        mutations.push((
            key,
            Box::new(move |root| root["authority"][key] = true.into()),
        ));
    }
    mutations.push((
        "digest_mismatch",
        Box::new(|root| {
            root["cases"][0]["expected"]["canonical_sha256"] = "0".repeat(64).into();
        }),
    ));

    for (name, mutation) in mutations {
        let mut value: Value = serde_json::from_slice(FIXTURE).expect("fixture JSON");
        mutation(&mut value);
        let decoded: Result<Fixture, _> = decode(&serde_json::to_vec(&value).expect("JSON"));
        if let Ok(fixture) = decoded {
            assert!(
                validate_fixture(&fixture).is_err(),
                "mutation {name} unexpectedly accepted"
            );
        }
    }

    let mut duplicate = String::from_utf8(FIXTURE.to_vec()).expect("fixture UTF-8");
    let end = duplicate
        .trim_end()
        .strip_suffix('}')
        .expect("root object close")
        .len();
    duplicate.truncate(end);
    duplicate.push_str(",\"schema_version\":\"forge.device-inventory-snapshot-canonical/v1\"}\n");
    assert!(decode::<Fixture>(duplicate.as_bytes()).is_err());
}
