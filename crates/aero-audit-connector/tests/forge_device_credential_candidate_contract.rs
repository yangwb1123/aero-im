//! Device credential candidate parity. This receiver validates only the
//! metadata-only candidate response and has no secret, persistence, or
//! authority path.

use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt;

const FIXTURE: &[u8] = include_bytes!("testdata/forge-device-credential-candidate-v1.json");
const SCHEMA: &str = "forge.device-credential-lifecycle/v1";
const MODE: &str = "pure_device_credential_lifecycle";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MIN_LIFETIME_MS: u64 = 1_000;
const MAX_LIFETIME_MS: u64 = 3_600_000;

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
    owner_binding_matched: bool,
    owner_authenticated: bool,
    credential_material_made: bool,
    persisted: bool,
    inventory_authoritative: bool,
    execution_authorized: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Candidate {
    schema_version: String,
    evaluation_mode: String,
    owner: Owner,
    device_id: String,
    action: String,
    revision: u64,
    #[serde(default)]
    previous: Option<State>,
    next: State,
    preview_only: bool,
    candidate_published: bool,
    authority: Authority,
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

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_state(state: &State) -> bool {
    if !valid_identifier(&state.credential_id)
        || !valid_identifier(&state.device_id)
        || !valid_owner(&state.owner)
        || !matches!(
            state.approval_state.as_str(),
            "pending" | "approved" | "revoked"
        )
        || !matches!(
            state.credential_state.as_str(),
            "active" | "expired" | "revoked"
        )
        || !valid_identifier(&state.key_id)
        || !valid_digest(&state.public_key_sha256)
        || state.key_generation == 0
        || state.key_generation > MAX_SAFE_INTEGER
        || state.issued_at_ms > MAX_SAFE_INTEGER
        || state.expires_at_ms > MAX_SAFE_INTEGER
        || state.expires_at_ms <= state.issued_at_ms
    {
        return false;
    }
    let lifetime = state.expires_at_ms - state.issued_at_ms;
    (MIN_LIFETIME_MS..=MAX_LIFETIME_MS).contains(&lifetime)
}

fn display_only(candidate: &Candidate) -> bool {
    if candidate.schema_version != SCHEMA
        || candidate.evaluation_mode != MODE
        || !valid_owner(&candidate.owner)
        || !valid_identifier(&candidate.device_id)
        || !matches!(candidate.action.as_str(), "issue" | "revoke" | "rotate")
        || candidate.revision == 0
        || candidate.revision > MAX_SAFE_INTEGER
        || !valid_state(&candidate.next)
        || candidate.next.owner != candidate.owner
        || candidate.next.device_id != candidate.device_id
        || !candidate.preview_only
        || !candidate.candidate_published
        || candidate.authority != Authority::default()
    {
        return false;
    }
    if let Some(previous) = candidate.previous.as_ref() {
        if !valid_state(previous)
            || previous.owner != candidate.owner
            || previous.device_id != candidate.device_id
        {
            return false;
        }
    }
    match candidate.action.as_str() {
        "issue" => {
            candidate.previous.is_none()
                && candidate.next.credential_state == "active"
                && candidate.next.approval_state != "revoked"
        }
        "revoke" => {
            let Some(previous) = candidate.previous.as_ref() else {
                return false;
            };
            previous.credential_state != "revoked"
                && candidate.next.credential_state == "revoked"
                && same_metadata(previous, &candidate.next)
        }
        "rotate" => {
            let Some(previous) = candidate.previous.as_ref() else {
                return false;
            };
            previous.credential_state != "revoked"
                && previous.key_generation < MAX_SAFE_INTEGER
                && candidate.next.credential_state == "active"
                && candidate.next.key_generation == previous.key_generation + 1
                && candidate.next.approval_state == previous.approval_state
                && (candidate.next.credential_id != previous.credential_id
                    || candidate.next.key_id != previous.key_id
                    || candidate.next.public_key_sha256 != previous.public_key_sha256)
        }
        _ => false,
    }
}

fn same_metadata(left: &State, right: &State) -> bool {
    left.credential_id == right.credential_id
        && left.device_id == right.device_id
        && left.owner == right.owner
        && left.approval_state == right.approval_state
        && left.key_id == right.key_id
        && left.public_key_sha256 == right.public_key_sha256
        && left.key_generation == right.key_generation
        && left.issued_at_ms == right.issued_at_ms
        && left.expires_at_ms == right.expires_at_ms
}

#[test]
fn forge_device_credential_candidate_receiver_is_strict_and_inert() {
    let candidate: Candidate = decode(FIXTURE).expect("credential candidate fixture");
    assert!(display_only(&candidate));
    assert_eq!(candidate.owner.subject, "user-1");
    assert_eq!(candidate.device_id, "device-1");
    assert_eq!(candidate.action, "issue");
    assert_eq!(candidate.next.credential_id, "credential-1");
}

#[test]
fn forge_device_credential_candidate_rejects_mutation() {
    let mutate = |mutator: fn(&mut Value)| -> Vec<u8> {
        let mut value: Value = serde_json::from_slice(FIXTURE).unwrap();
        mutator(&mut value);
        serde_json::to_vec(&value).unwrap()
    };

    let mutations: [(&str, Vec<u8>); 6] = [
        (
            "unknown_field",
            mutate(|value| {
                value["credential_material"] = Value::String("secret".into());
            }),
        ),
        (
            "foreign_owner",
            mutate(|value| {
                value["owner"]["subject"] = Value::String("foreign-user".into());
            }),
        ),
        (
            "nested_device_drift",
            mutate(|value| {
                value["next"]["device_id"] = Value::String("device-foreign".into());
            }),
        ),
        (
            "preview_disabled",
            mutate(|value| {
                value["preview_only"] = Value::Bool(false);
            }),
        ),
        (
            "candidate_not_published",
            mutate(|value| {
                value["candidate_published"] = Value::Bool(false);
            }),
        ),
        (
            "authority_enabled",
            mutate(|value| {
                value["authority"]["persisted"] = Value::Bool(true);
            }),
        ),
    ];
    for (name, raw) in mutations {
        let decoded = decode::<Candidate>(&raw);
        if name == "unknown_field" {
            assert!(decoded.is_err(), "unknown field accepted");
            continue;
        }
        assert!(decoded.is_ok(), "mutation did not retain candidate shape");
        assert!(!display_only(&decoded.unwrap()), "{name} mutation accepted");
    }

    let mut duplicate = FIXTURE.to_vec();
    duplicate = duplicate[..duplicate.len() - 2].to_vec();
    duplicate
        .extend_from_slice(br#",\"schema_version\":\"forge.device-credential-lifecycle/v1\"}"#);
    assert!(
        decode::<Candidate>(&duplicate).is_err(),
        "duplicate field accepted"
    );

    let mut trailing = FIXTURE.to_vec();
    trailing.extend_from_slice(b" true");
    assert!(
        decode::<Candidate>(&trailing).is_err(),
        "trailing JSON accepted"
    );
}

fn reject_duplicate_keys(raw: &[u8]) -> Result<(), String> {
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    struct DuplicateKeySeed;
    impl<'de> DeserializeSeed<'de> for DuplicateKeySeed {
        type Value = ();

        fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_any(DuplicateKeyVisitor)
        }
    }
    struct DuplicateKeyVisitor;
    impl<'de> Visitor<'de> for DuplicateKeyVisitor {
        type Value = ();

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("JSON value")
        }

        fn visit_map<M>(self, mut access: M) -> Result<(), M::Error>
        where
            M: MapAccess<'de>,
        {
            let mut keys = BTreeSet::new();
            while let Some(key) = access.next_key::<String>()? {
                if !keys.insert(key) {
                    return Err(de::Error::custom("duplicate JSON key"));
                }
                access.next_value_seed(DuplicateKeySeed)?;
            }
            Ok(())
        }

        fn visit_seq<S>(self, mut access: S) -> Result<(), S::Error>
        where
            S: SeqAccess<'de>,
        {
            while access.next_element_seed(DuplicateKeySeed)?.is_some() {}
            Ok(())
        }

        fn visit_bool<E>(self, _: bool) -> Result<(), E> {
            Ok(())
        }

        fn visit_i64<E>(self, _: i64) -> Result<(), E> {
            Ok(())
        }

        fn visit_u64<E>(self, _: u64) -> Result<(), E> {
            Ok(())
        }

        fn visit_f64<E>(self, _: f64) -> Result<(), E> {
            Ok(())
        }

        fn visit_str<E>(self, _: &str) -> Result<(), E> {
            Ok(())
        }

        fn visit_string<E>(self, _: String) -> Result<(), E> {
            Ok(())
        }

        fn visit_bytes<E>(self, _: &[u8]) -> Result<(), E> {
            Ok(())
        }

        fn visit_byte_buf<E>(self, _: Vec<u8>) -> Result<(), E> {
            Ok(())
        }

        fn visit_none<E>(self) -> Result<(), E> {
            Ok(())
        }

        fn visit_some<D>(self, deserializer: D) -> Result<(), D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            DuplicateKeySeed.deserialize(deserializer)
        }

        fn visit_unit<E>(self) -> Result<(), E> {
            Ok(())
        }
    }
    DuplicateKeySeed
        .deserialize(&mut decoder)
        .map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())
}
