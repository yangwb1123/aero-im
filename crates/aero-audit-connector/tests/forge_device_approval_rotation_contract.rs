//! Device approval, revocation, and key rotation are a pure Forge value
//! contract.  This receiver checks the shared fixture without authenticating
//! an owner, persisting enrollment state, issuing credentials, or granting
//! inventory or execution authority.

use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::fmt;

const FIXTURE: &[u8] = include_bytes!("testdata/forge-device-approval-rotation-v1.json");
const SCHEMA_VERSION: &str = "forge.device-approval-rotation/v1";
const EVALUATION_MODE: &str = "pure_owner_device_lifecycle";

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Owner {
    issuer: String,
    subject: String,
    tenant_id: String,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct State {
    device_id: String,
    owner: Owner,
    approval_state: String,
    key_id: String,
    public_key_sha256: String,
    key_generation: u64,
}

#[derive(Debug, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Authority {
    owner_authenticated: bool,
    persisted: bool,
    credential_issued: bool,
    inventory_authoritative: bool,
    execution_authorized: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    action: String,
    owner: Owner,
    device_id: String,
    next_key_id: String,
    next_public_key_sha256: String,
    #[serde(default)]
    expected_state: Option<State>,
    #[serde(default)]
    expected_error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: String,
    evaluation_mode: String,
    notice: String,
    initial: State,
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

fn valid_identifier(value: &str) -> bool {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | ':' | '-')
        })
        && value.len() <= 128
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_owner(owner: &Owner) -> bool {
    [&owner.issuer, &owner.subject, &owner.tenant_id]
        .into_iter()
        .all(|value| {
            !value.is_empty()
                && value.len() <= 512
                && value.trim() == value
                && value.chars().all(|character| !character.is_control())
        })
}

fn valid_state(state: &State) -> bool {
    valid_identifier(&state.device_id)
        && valid_owner(&state.owner)
        && matches!(
            state.approval_state.as_str(),
            "pending" | "approved" | "revoked"
        )
        && valid_identifier(&state.key_id)
        && valid_digest(&state.public_key_sha256)
        && state.key_generation > 0
}

fn apply(current: &State, case: &Case) -> Result<State, &'static str> {
    if !valid_state(current) {
        return Err("invalid_state");
    }
    if case.owner != current.owner {
        return Err("owner_mismatch");
    }
    if case.device_id != current.device_id {
        return Err("device_mismatch");
    }

    let mut next = current.clone();
    match case.action.as_str() {
        "approve" => {
            if !case.next_key_id.is_empty() || !case.next_public_key_sha256.is_empty() {
                return Err("unexpected_rotation_key");
            }
            if current.approval_state != "pending" {
                return Err(if current.approval_state == "revoked" {
                    "revocation_terminal"
                } else {
                    "invalid_transition"
                });
            }
            next.approval_state = "approved".into();
        }
        "revoke" => {
            if !case.next_key_id.is_empty() || !case.next_public_key_sha256.is_empty() {
                return Err("unexpected_rotation_key");
            }
            if current.approval_state == "revoked" {
                return Err("revocation_terminal");
            }
            next.approval_state = "revoked".into();
        }
        "rotate_key" => {
            if current.approval_state == "revoked" {
                return Err("revocation_terminal");
            }
            if !valid_identifier(&case.next_key_id) || !valid_digest(&case.next_public_key_sha256) {
                return Err("invalid_rotation_key");
            }
            if case.next_key_id == current.key_id
                && case.next_public_key_sha256 == current.public_key_sha256
            {
                return Err("rotation_key_unchanged");
            }
            if current.key_generation == u64::MAX {
                return Err("key_generation_overflow");
            }
            next.key_id = case.next_key_id.clone();
            next.public_key_sha256 = case.next_public_key_sha256.clone();
            next.key_generation += 1;
        }
        _ => return Err("unknown_action"),
    }
    if !valid_state(&next) {
        return Err("invalid_state");
    }
    Ok(next)
}

fn validate_fixture(fixture: &Fixture) -> Result<(), String> {
    if fixture.schema_version != SCHEMA_VERSION
        || fixture.evaluation_mode != EVALUATION_MODE
        || fixture.notice.is_empty()
        || fixture.authority != Authority::default()
        || fixture.cases.len() != 7
        || fixture.initial.approval_state != "pending"
        || fixture.initial.key_generation != 1
        || !valid_state(&fixture.initial)
    {
        return Err("invalid envelope".into());
    }

    let mut names = BTreeSet::new();
    for case in &fixture.cases {
        if case.name.is_empty() || !names.insert(case.name.as_str()) {
            return Err(format!("duplicate or empty case name: {}", case.name));
        }
        let result = apply(&fixture.initial, case);
        match (&case.expected_error, &case.expected_state, result) {
            (Some(error), None, Err(actual)) if error == actual => {}
            (None, Some(expected), Ok(actual)) if expected == &actual => {}
            (Some(error), None, Err(actual)) => {
                return Err(format!("case {} error {actual}, want {error}", case.name))
            }
            (None, Some(expected), Ok(actual)) => {
                return Err(format!(
                    "case {} state mismatch: {actual:?} != {expected:?}",
                    case.name
                ))
            }
            (Some(_), Some(_), _) => {
                return Err(format!("case {} has both expected fields", case.name))
            }
            (None, None, _) => return Err(format!("case {} has no expected field", case.name)),
            (Some(error), None, Ok(actual)) => {
                return Err(format!(
                    "case {} accepted {actual:?}, want {error}",
                    case.name
                ))
            }
            (None, Some(expected), Err(actual)) => {
                return Err(format!(
                    "case {} rejected {actual}, want {expected:?}",
                    case.name
                ))
            }
        }
    }
    Ok(())
}

#[test]
fn canonical_device_approval_rotation_fixture_is_strict_and_authority_free() {
    let fixture: Fixture = decode(FIXTURE).expect("canonical device approval fixture");
    validate_fixture(&fixture).expect("valid device approval fixture");
    assert_eq!(
        fixture.cases[0]
            .expected_state
            .as_ref()
            .unwrap()
            .approval_state,
        "approved"
    );
    assert_eq!(
        fixture.cases[2]
            .expected_state
            .as_ref()
            .unwrap()
            .key_generation,
        2
    );
    assert_eq!(
        fixture.cases[6].expected_error.as_deref(),
        Some("invalid_rotation_key")
    );
}

#[test]
fn device_approval_rotation_rejects_unknown_duplicate_and_authority_mutations() {
    let mut unknown: Map<String, Value> = serde_json::from_slice(FIXTURE).unwrap();
    unknown.insert("unexpected".into(), true.into());
    assert!(decode::<Fixture>(&serde_json::to_vec(&unknown).unwrap()).is_err());

    let mut duplicate = String::from_utf8(FIXTURE.to_vec()).unwrap();
    let end = duplicate
        .trim_end()
        .strip_suffix('}')
        .expect("root object close")
        .len();
    duplicate.truncate(end);
    duplicate.push_str(",\"schema_version\":\"forge.device-approval-rotation/v1\"}\n");
    assert!(decode::<Fixture>(duplicate.as_bytes()).is_err());

    let mut authority: Value = serde_json::from_slice(FIXTURE).unwrap();
    authority["authority"]["persisted"] = true.into();
    let decoded: Fixture = decode(&serde_json::to_vec(&authority).unwrap()).unwrap();
    assert!(validate_fixture(&decoded).is_err());
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
