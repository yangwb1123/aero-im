//! Strict receiver parity for the shared-session value envelope.
//!
//! This remains compatibility evidence only. Aero-IM does not authenticate an
//! owner, persist a Conversation, append a Prompt, authorize a Run, or publish
//! Audit from this caller-supplied fixture.

use serde::de::{
    self, DeserializeOwned, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor,
};
use serde::Deserialize;
use std::fmt;

const FIXTURE: &[u8] = include_bytes!("testdata/forge-shared-session-v1.json");
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scope {
    kind: String,
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Conversation {
    id: String,
    scope: Scope,
    title: String,
    created_at_ms: u64,
    updated_at_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    conversation: Conversation,
    aggregate_version: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConversationPage {
    conversations: Vec<Entry>,
    has_more: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Prompt {
    id: String,
    conversation_id: String,
    role: String,
    content: String,
    created_at_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptCursor {
    created_at_ms: u64,
    prompt_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptPage {
    conversation_id: String,
    prompts: Vec<Prompt>,
    next_cursor: Option<PromptCursor>,
    has_more: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    cursor: u64,
    schema_version: u64,
    conversation_id: String,
    entity_id: String,
    aggregate_version: u64,
    kind: String,
    created_at_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChangePage {
    after_cursor: u64,
    scanned_through_cursor: u64,
    has_more: bool,
    changes: Vec<Change>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Append {
    prompt: Prompt,
    aggregate_version: u64,
    replayed: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    conversation_page: ConversationPage,
    conversation_detail: Entry,
    prompt_page: PromptPage,
    change_page: ChangePage,
    append_prompt: Append,
}

fn decode<T: DeserializeOwned>(raw: &[u8]) -> Result<T, String> {
    reject_duplicate_keys(raw)?;
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value = T::deserialize(&mut decoder).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    Ok(value)
}

fn valid_conversation(
    value: &Conversation,
    id: &str,
    kind: &str,
    scope_id: Option<&str>,
    title: &str,
    created: u64,
    updated: u64,
) -> bool {
    value.id == id
        && value.scope.kind == kind
        && value.scope.id.as_deref() == scope_id
        && value.title == title
        && value.created_at_ms == created
        && value.updated_at_ms == updated
        && value.created_at_ms <= MAX_SAFE_INTEGER
        && value.updated_at_ms <= MAX_SAFE_INTEGER
        && !value.id.is_empty()
        && !value.id.contains('/')
}

fn validate(value: &Envelope) -> Result<(), String> {
    if value.conversation_page.conversations.len() != 2
        || value.conversation_page.has_more
        || value.conversation_detail.aggregate_version != 2
        || value.conversation_detail.conversation.id != "conversation-001"
        || value.prompt_page.conversation_id != "conversation-001"
        || !value.prompt_page.has_more
        || value
            .prompt_page
            .next_cursor
            .as_ref()
            .map(|cursor| (cursor.created_at_ms, cursor.prompt_id.as_str()))
            != Some((100, "prompt-001"))
        || value.change_page.after_cursor != 0
        || value.change_page.scanned_through_cursor != 2
        || value.change_page.has_more
        || value.change_page.changes.len() != 2
        || value.append_prompt.aggregate_version != 3
        || value.append_prompt.replayed
    {
        return Err("shared-session metadata drift".into());
    }
    let expected = [
        (
            "conversation-001",
            "project",
            Some("project-7"),
            "Shared build",
            100,
            150,
            2,
        ),
        ("conversation-002", "global", None, "Run tests", 200, 250, 1),
    ];
    for (entry, (id, kind, scope, title, created, updated, version)) in
        value.conversation_page.conversations.iter().zip(expected)
    {
        if entry.aggregate_version != version
            || !valid_conversation(
                &entry.conversation,
                id,
                kind,
                scope,
                title,
                created,
                updated,
            )
        {
            return Err("conversation page drift".into());
        }
    }
    if !valid_conversation(
        &value.conversation_detail.conversation,
        "conversation-001",
        "project",
        Some("project-7"),
        "Shared build",
        100,
        150,
    ) {
        return Err("conversation detail drift".into());
    }
    let prompts = [
        ("prompt-002", "continue the shared task", 200),
        ("prompt-001", "inspect the project", 100),
    ];
    for (index, (prompt, (id, content, created))) in
        value.prompt_page.prompts.iter().zip(prompts).enumerate()
    {
        if prompt.id != id
            || prompt.conversation_id != "conversation-001"
            || prompt.role != "user"
            || prompt.content != content
            || prompt.created_at_ms != created
            || (index > 0 && value.prompt_page.prompts[index - 1].created_at_ms <= created)
        {
            return Err("prompt page drift".into());
        }
    }
    for (index, change) in value.change_page.changes.iter().enumerate() {
        let (kind, entity) = if index == 0 {
            ("conversation_created", "conversation-001")
        } else {
            ("prompt_appended", "prompt-001")
        };
        if change.cursor != (index + 1) as u64
            || change.schema_version != 1
            || change.conversation_id != "conversation-001"
            || change.entity_id != entity
            || change.aggregate_version != (index + 1) as u64
            || change.kind != kind
            || change.created_at_ms != 100
            || (index > 0 && value.change_page.changes[index - 1].cursor >= change.cursor)
        {
            return Err("change page drift".into());
        }
    }
    let prompt = &value.append_prompt.prompt;
    if prompt.id != "prompt-003"
        || prompt.conversation_id != "conversation-001"
        || prompt.role != "user"
        || prompt.content != "send this from another client"
        || prompt.created_at_ms != 300
    {
        return Err("append prompt drift".into());
    }
    Ok(())
}

#[test]
fn shared_session_receiver_accepts_canonical_fixture() {
    let value: Envelope = decode(FIXTURE).expect("decode shared-session fixture");
    validate(&value).expect("validate shared-session fixture");
}

#[test]
fn shared_session_receiver_rejects_wire_and_binding_drift() {
    let base = String::from_utf8(FIXTURE.to_vec()).expect("fixture utf8");
    let mutations = [
        (
            "unknown",
            format!(
                r#"{},"authority":{{"execution_authorized":true}}}}"#,
                base.trim_end_matches('}')
            ),
        ),
        (
            "duplicate",
            format!(
                r#"{},"conversation_page":{{}}}}"#,
                base.trim_end_matches('}')
            ),
        ),
        ("trailing", format!("{base} {{}}")),
        (
            "foreign_detail",
            base.replace(
                "\"id\": \"conversation-001\"",
                "\"id\": \"conversation-foreign\"",
            ),
        ),
        (
            "prompt_role",
            base.replace("\"role\": \"user\"", "\"role\": \"assistant\""),
        ),
        (
            "change_cursor",
            base.replace("\"cursor\": 2", "\"cursor\": 1"),
        ),
        (
            "replayed",
            base.replace("\"replayed\": false", "\"replayed\": true"),
        ),
    ];
    for (name, raw) in mutations {
        let value = decode::<Envelope>(raw.as_bytes()).and_then(|value| {
            validate(&value)?;
            Ok(value)
        });
        assert!(value.is_err(), "accepted shared-session mutation {name}");
    }
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

    fn visit_map<A>(self, mut map: A) -> Result<(), A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut keys = std::collections::BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(de::Error::custom("duplicate JSON key"));
            }
            map.next_value_seed(ScanSeed)?;
        }
        Ok(())
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<(), A::Error>
    where
        A: SeqAccess<'de>,
    {
        while seq.next_element_seed(ScanSeed)?.is_some() {}
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
