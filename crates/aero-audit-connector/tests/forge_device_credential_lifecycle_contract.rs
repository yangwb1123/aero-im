//! Device credential lifecycle parity. This receiver validates only a pure
//! metadata replacement plan and has no secret, persistence, or authority path.

use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;

const FIXTURE: &[u8] = include_bytes!("testdata/forge-device-credential-lifecycle-v1.json");
const SCHEMA: &str = "forge.device-credential-lifecycle/v1";
const MODE: &str = "pure_device_credential_lifecycle";
const NOTICE: &str = "This fixture is a pure device credential metadata replacement plan. It authenticates no caller, creates no secret, writes no state, and grants no inventory or execution authority.";

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Owner {
    issuer: String,
    subject: String,
    tenant_id: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct State {
    credential_id: String,
    device_id: String,
    owner: Owner,
    approval_state: String,
    credential_state: String,
    key_id: String,
    public_key_sha256: String,
    key_generation: u64,
    issued_at_ms: u64,
    expires_at_ms: u64,
}

#[derive(Debug, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Authority {
    owner_authenticated: bool,
    credential_material_made: bool,
    persisted: bool,
    inventory_authoritative: bool,
    execution_authorized: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    action: String,
    #[serde(default)]
    current: Option<State>,
    owner: Owner,
    device_id: String,
    #[serde(default)]
    approval_state: String,
    #[serde(default)]
    credential_id: String,
    #[serde(default)]
    key_id: String,
    #[serde(default)]
    public_key_sha256: String,
    #[serde(default)]
    key_generation: u64,
    #[serde(default)]
    issued_at_ms: u64,
    #[serde(default)]
    expires_at_ms: u64,
    #[serde(default)]
    next_credential_id: String,
    #[serde(default)]
    next_key_id: String,
    #[serde(default)]
    next_public_key_sha256: String,
    #[serde(default)]
    observed_at_ms: u64,
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

fn valid_token(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_owner(owner: &Owner) -> bool {
    valid_token(&owner.issuer, 512)
        && valid_token(&owner.subject, 512)
        && valid_token(&owner.tenant_id, 512)
}

fn valid_state(state: &State) -> bool {
    valid_token(&state.credential_id, 128)
        && valid_token(&state.device_id, 128)
        && valid_owner(&state.owner)
        && matches!(
            state.approval_state.as_str(),
            "pending" | "approved" | "revoked"
        )
        && matches!(
            state.credential_state.as_str(),
            "active" | "expired" | "revoked"
        )
        && valid_token(&state.key_id, 128)
        && valid_digest(&state.public_key_sha256)
        && state.key_generation > 0
        && state.issued_at_ms < state.expires_at_ms
}

fn display_only(fixture: &Fixture) -> bool {
    if fixture.schema_version != SCHEMA
        || fixture.evaluation_mode != MODE
        || fixture.notice != NOTICE
        || fixture.authority != Authority::default()
        || fixture.cases.len() != 6
    {
        return false;
    }
    let mut names = HashSet::new();
    for case in &fixture.cases {
        if !valid_token(&case.name, 128)
            || !names.insert(&case.name)
            || !valid_owner(&case.owner)
            || !valid_token(&case.device_id, 128)
            || case.action.is_empty()
        {
            return false;
        }
        if case.expected_error.is_some() {
            if case.expected_state.is_some() {
                return false;
            }
            if !matches!(
                case.expected_error.as_deref(),
                Some("credential_terminal") | Some("credential_lifetime_too_long")
            ) {
                return false;
            }
            continue;
        }
        let Some(expected) = case.expected_state.as_ref() else {
            return false;
        };
        if !valid_state(expected)
            || expected.owner != case.owner
            || expected.device_id != case.device_id
        {
            return false;
        }
        match case.action.as_str() {
            "issue" => {
                if case.current.is_some()
                    || !matches!(case.approval_state.as_str(), "pending" | "approved")
                    || case.observed_at_ms < case.issued_at_ms
                    || case.observed_at_ms >= case.expires_at_ms
                    || expected.credential_state != "active"
                    || expected.approval_state != case.approval_state
                    || expected.credential_id != case.credential_id
                    || expected.key_id != case.key_id
                    || expected.public_key_sha256 != case.public_key_sha256
                    || expected.key_generation != case.key_generation
                    || expected.issued_at_ms != case.issued_at_ms
                    || expected.expires_at_ms != case.expires_at_ms
                {
                    return false;
                }
            }
            "revoke" => {
                let Some(current) = case.current.as_ref() else {
                    return false;
                };
                if !valid_state(current)
                    || expected.credential_state != "revoked"
                    || expected.credential_id != current.credential_id
                    || expected.key_id != current.key_id
                    || expected.key_generation != current.key_generation
                {
                    return false;
                }
            }
            "rotate" => {
                let Some(current) = case.current.as_ref() else {
                    return false;
                };
                if !valid_state(current)
                    || case.observed_at_ms < case.issued_at_ms
                    || case.observed_at_ms >= case.expires_at_ms
                    || expected.credential_state != "active"
                    || expected.approval_state != current.approval_state
                    || expected.credential_id != case.next_credential_id
                    || expected.key_id != case.next_key_id
                    || expected.public_key_sha256 != case.next_public_key_sha256
                    || expected.key_generation != current.key_generation + 1
                {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

#[test]
fn forge_device_credential_lifecycle_receiver_is_strict_and_inert() {
    let fixture: Fixture = decode(FIXTURE).expect("credential lifecycle fixture");
    assert!(display_only(&fixture));
}

#[test]
fn forge_device_credential_lifecycle_rejects_mutation() {
    let mut unknown: Value = serde_json::from_slice(FIXTURE).unwrap();
    unknown["credential_material"] = Value::String("secret".into());
    assert!(decode::<Fixture>(&serde_json::to_vec(&unknown).unwrap()).is_err());

    let duplicate = br#"{"schema_version":"forge.device-credential-lifecycle/v1","schema_version":"forge.device-credential-lifecycle/v1"}"#;
    assert!(decode::<Fixture>(duplicate).is_err());

    let mut authority: Value = serde_json::from_slice(FIXTURE).unwrap();
    authority["authority"]["persisted"] = Value::Bool(true);
    let mutated: Fixture = decode(&serde_json::to_vec(&authority).unwrap()).unwrap();
    assert!(!display_only(&mutated));

    let mut action: Value = serde_json::from_slice(FIXTURE).unwrap();
    action["cases"][0]["action"] = Value::String("mint".into());
    let mutated: Fixture = decode(&serde_json::to_vec(&action).unwrap()).unwrap();
    assert!(!display_only(&mutated));

    let mut trailing = FIXTURE.to_vec();
    trailing.extend_from_slice(b" true");
    assert!(decode::<Fixture>(&trailing).is_err());
}

fn reject_duplicate_keys(bytes: &[u8]) -> Result<(), String> {
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    UniqueJson
        .deserialize(&mut decoder)
        .map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())
}

struct UniqueJson;
impl<'de> DeserializeSeed<'de> for UniqueJson {
    type Value = ();
    fn deserialize<D: de::Deserializer<'de>>(self, decoder: D) -> Result<Self::Value, D::Error> {
        decoder.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for UniqueJson {
    type Value = ();
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }
    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_borrowed_str<E>(self, _: &'de str) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_string<E>(self, _: String) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }
    fn visit_some<D>(self, decoder: D) -> Result<Self::Value, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        decoder.deserialize_any(self)
    }
    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while seq.next_element_seed(UniqueJson)?.is_some() {}
        Ok(())
    }
    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut keys = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(de::Error::custom("duplicate JSON object key"));
            }
            map.next_value_seed(UniqueJson)?;
        }
        Ok(())
    }
}
