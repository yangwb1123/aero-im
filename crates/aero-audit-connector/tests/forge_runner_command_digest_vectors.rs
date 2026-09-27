//! The Runner command digest is a cross-language byte contract. This receiver
//! only checks supplied values; it never authenticates a device or executes a
//! command.

use serde::{Deserialize, Serialize};
use serde_json::de::Deserializer;
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fmt};

const FIXTURE: &[u8] = include_bytes!("testdata/forge-runner-command-digest-v1.json");
const SCHEMA_VERSION: &str = "forge.runner-command-digest/v1";
const DIGEST_DOMAIN: &[u8] = b"forge.runtime.runner-command.v1\0";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: String,
    digest_domain: String,
    vectors: Vec<Vector>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Vector {
    name: String,
    command: Command,
    command_sha256: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Command {
    v: u16,
    command_id: String,
    lease_proof: LeaseProof,
    idempotency_key: String,
    workspace_ref: String,
    argv: Vec<String>,
    timeout_ms: u64,
    max_output_bytes: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LeaseProof {
    attempt_id: String,
    target_id: String,
    epoch: u64,
    fencing_token: String,
}

#[test]
fn command_digest_vectors_match_go_and_console_bytes() {
    let fixture = decode(FIXTURE).expect("strict digest vector fixture");
    assert_eq!(fixture.schema_version, SCHEMA_VERSION);
    assert_eq!(fixture.digest_domain, "forge.runtime.runner-command.v1");
    assert_eq!(fixture.vectors.len(), 3);

    let mut names = BTreeSet::new();
    for vector in fixture.vectors {
        assert!(!vector.name.is_empty());
        assert!(names.insert(vector.name.clone()));
        assert_eq!(command_sha256(&vector.command), vector.command_sha256);
    }
}

#[test]
fn command_digest_vectors_reject_unknown_duplicate_and_trailing_values() {
    let mut unknown: serde_json::Value = serde_json::from_slice(FIXTURE).unwrap();
    unknown["unexpected"] = serde_json::Value::Bool(true);
    assert!(decode(&serde_json::to_vec(&unknown).unwrap()).is_err());

    let duplicate = format!(
        "{},\"schema_version\":\"forge.runner-command-digest/v1\"}}",
        String::from_utf8_lossy(FIXTURE).trim_end_matches('}')
    );
    assert!(decode(duplicate.as_bytes()).is_err());
    assert!(decode(format!("{}\n{{}}", String::from_utf8_lossy(FIXTURE)).as_bytes()).is_err());
}

fn decode(raw: &[u8]) -> Result<Fixture, String> {
    reject_duplicate_json_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value = Fixture::deserialize(&mut decoder).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    Ok(value)
}

fn command_sha256(command: &Command) -> String {
    let bytes = serde_json::to_vec(command).expect("canonical command JSON");
    let mut digest = Sha256::new();
    digest.update(DIGEST_DOMAIN);
    digest.update(bytes);
    format!("{:x}", digest.finalize())
}

fn reject_duplicate_json_keys(raw: &[u8]) -> Result<(), String> {
    let mut decoder = Deserializer::from_slice(raw);
    serde::de::Deserializer::deserialize_any(&mut decoder, ScanVisitor)
        .map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())
}

struct ScanSeed;
struct ScanVisitor;

impl<'de> serde::de::DeserializeSeed<'de> for ScanSeed {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(ScanVisitor)
    }
}

impl<'de> serde::de::Visitor<'de> for ScanVisitor {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON without duplicate object keys")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        let mut keys = BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(serde::de::Error::custom(format!(
                    "duplicate JSON key {key:?}"
                )));
            }
            map.next_value_seed(ScanSeed)?;
        }
        Ok(())
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        while sequence.next_element_seed(ScanSeed)?.is_some() {}
        Ok(())
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(())
    }

    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(())
    }

    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(())
    }

    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(())
    }

    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(())
    }

    fn visit_string<E>(self, _: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(())
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(())
    }
}
