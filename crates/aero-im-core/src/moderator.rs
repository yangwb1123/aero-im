//! Lightweight pre-publish content moderator.
//!
//! Default impl reads `AERO_BLOCKED_WORDS` (comma-separated, case-insensitive)
//! and rejects messages whose textual block content contains any forbidden
//! substring. This synchronous pre-publish guard is complemented by the
//! budgeted `AiJobKind::Moderate` post-publish path in `aero-ai`, which
//! soft-deletes content when the provider returns a blocking verdict.
//!
//! Replace `KeywordModerator` with an LLM-backed implementation by providing a
//! different `Moderator` to `ImService`.

use aero_common::{Block, SpanStyle};

/// Canonical moderation projection for every user-visible message field.
///
/// This is intentionally separate from the full-text/search projection. Search
/// may omit presentation metadata, but moderation must cover everything the Web
/// client renders: card payload values, tool names/arguments/results, and select
/// placeholders in addition to ordinary searchable prose. Hidden thoughts and
/// machine-only interaction identifiers/values remain excluded.
#[must_use]
pub fn moderation_text(blocks: &[Block]) -> String {
    let mut text = String::new();
    for block in blocks {
        match block {
            Block::Text { content, spans } => {
                push_line(&mut text, content);
                for span in spans {
                    if let SpanStyle::Link { href } = &span.style {
                        push_line(&mut text, href);
                    }
                }
            }
            Block::Mention { .. } => {
                // The visible mention name comes from the referenced participant
                // profile rather than attacker-controlled message text.
            }
            Block::Code { lang, content } => {
                push_line(&mut text, lang);
                push_line(&mut text, content);
            }
            Block::File { name, .. } => push_line(&mut text, name),
            Block::Voice { transcript, .. } => {
                if let Some(transcript) = transcript {
                    push_line(&mut text, transcript);
                }
            }
            Block::Card { schema, payload } => {
                push_line(&mut text, schema);
                push_json_text(&mut text, payload);
            }
            Block::ToolCall { tool, args, result } => {
                push_line(&mut text, tool);
                push_json_text(&mut text, args);
                if let Some(result) = result {
                    push_json_text(&mut text, result);
                }
            }
            Block::Select {
                placeholder,
                options,
                ..
            } => {
                if let Some(placeholder) = placeholder {
                    push_line(&mut text, placeholder);
                }
                for option in options {
                    push_line(&mut text, &option.label);
                }
            }
            Block::Thought { content, hidden } => {
                if !hidden {
                    push_line(&mut text, content);
                }
            }
            Block::Button { label, url, .. } => {
                push_line(&mut text, label);
                if let Some(url) = url {
                    push_line(&mut text, url);
                }
            }
        }
    }
    text
}

fn push_json_text(text: &mut String, value: &serde_json::Value) {
    match value {
        serde_json::Value::Null => {}
        serde_json::Value::Bool(value) => push_line(text, if *value { "true" } else { "false" }),
        serde_json::Value::Number(value) => push_line(text, &value.to_string()),
        serde_json::Value::String(value) => push_line(text, value),
        serde_json::Value::Array(values) => {
            for value in values {
                push_json_text(text, value);
            }
        }
        serde_json::Value::Object(values) => {
            for (key, value) in values {
                // Tool-call JSON is rendered with JSON.stringify, so both object
                // keys and scalar values are visible to users.
                push_line(text, key);
                push_json_text(text, value);
            }
        }
    }
}

fn push_line(text: &mut String, value: &str) {
    if value.is_empty() {
        return;
    }
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(value);
}

/// Outcome of a moderation check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModerationVerdict {
    /// Allow the message to be published.
    Allow,
    /// Reject the message; carry a short reason for the user.
    Block(String),
}

pub trait Moderator: Send + Sync {
    fn check(&self, blocks: &[Block]) -> ModerationVerdict;
}

/// No-op moderator. Useful in tests + when explicitly disabled.
#[derive(Debug, Default, Clone, Copy)]
pub struct AllowAllModerator;

impl Moderator for AllowAllModerator {
    fn check(&self, _: &[Block]) -> ModerationVerdict {
        ModerationVerdict::Allow
    }
}

/// Case-insensitive substring blocklist.
#[derive(Debug, Clone)]
pub struct KeywordModerator {
    needles: Vec<String>,
}

impl KeywordModerator {
    #[must_use]
    pub fn new(words: Vec<String>) -> Self {
        let needles = words
            .into_iter()
            .map(|w| w.trim().to_lowercase())
            .filter(|w| !w.is_empty())
            .collect();
        Self { needles }
    }

    /// Build from the `AERO_BLOCKED_WORDS` environment variable.
    /// Returns `None` (no moderation) when the env var is empty or unset.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let raw = std::env::var("AERO_BLOCKED_WORDS").ok()?;
        let words: Vec<String> = raw.split(',').map(ToOwned::to_owned).collect();
        let m = Self::new(words);
        if m.needles.is_empty() {
            None
        } else {
            Some(m)
        }
    }
}

impl Moderator for KeywordModerator {
    fn check(&self, blocks: &[Block]) -> ModerationVerdict {
        if self.needles.is_empty() {
            return ModerationVerdict::Allow;
        }
        let lower = moderation_text(blocks).to_lowercase();
        for needle in &self.needles {
            if lower.contains(needle) {
                return ModerationVerdict::Block(format!("包含被屏蔽的关键词:{needle}"));
            }
        }
        ModerationVerdict::Allow
    }
}

#[cfg(test)]
mod tests {
    use aero_common::{BlobId, FileKind, ParticipantId, SelectOption, Span};

    use super::*;

    #[test]
    fn allow_all_passes() {
        let m = AllowAllModerator;
        assert_eq!(
            m.check(&[Block::text("anything")]),
            ModerationVerdict::Allow
        );
    }

    #[test]
    fn keyword_blocks_case_insensitive() {
        let m = KeywordModerator::new(vec!["BadWord".into(), "  ".into()]);
        assert!(matches!(
            m.check(&[Block::text("contains badword inside")]),
            ModerationVerdict::Block(_)
        ));
        assert_eq!(m.needles.len(), 1);
    }

    #[test]
    fn keyword_allows_clean_text() {
        let m = KeywordModerator::new(vec!["spam".into()]);
        assert_eq!(
            m.check(&[Block::text("hello world")]),
            ModerationVerdict::Allow
        );
    }

    #[test]
    fn empty_blocklist_allows() {
        let m = KeywordModerator::new(vec![]);
        assert_eq!(
            m.check(&[Block::text("anything")]),
            ModerationVerdict::Allow
        );
    }

    #[test]
    fn moderation_projection_covers_every_visible_structured_field() {
        let blocks = vec![
            Block::Text {
                content: "text-visible".into(),
                spans: vec![Span {
                    start: 0,
                    end: 4,
                    style: SpanStyle::Link {
                        href: "https://text-link-visible.example".into(),
                    },
                }],
            },
            Block::Code {
                lang: "code-lang-visible".into(),
                content: "code-visible".into(),
            },
            Block::Thought {
                content: "thought-visible".into(),
                hidden: false,
            },
            Block::Thought {
                content: "thought-hidden".into(),
                hidden: true,
            },
            Block::Button {
                action_id: "approve".into(),
                label: "button-visible".into(),
                style: None,
                url: Some("https://button-url-visible.example".into()),
            },
            Block::File {
                blob_id: BlobId::new(),
                kind: FileKind::Document,
                name: "file-visible.pdf".into(),
                size: 42,
            },
            Block::Voice {
                blob_id: BlobId::new(),
                duration_ms: 1_000,
                transcript: Some("voice-visible".into()),
            },
            Block::Voice {
                blob_id: BlobId::new(),
                duration_ms: 500,
                transcript: None,
            },
            Block::Select {
                action_id: "choose".into(),
                placeholder: Some("machine-placeholder".into()),
                options: vec![SelectOption {
                    value: "machine-value".into(),
                    label: "select-visible".into(),
                }],
            },
            Block::Mention {
                participant: ParticipantId::new(),
            },
            Block::Card {
                schema: "example".into(),
                payload: serde_json::json!({
                    "title": "card-visible",
                    "body": {"nested": "card-nested-visible"}
                }),
            },
            Block::ToolCall {
                tool: "tool-visible".into(),
                args: serde_json::json!({"prompt-visible": "args-visible"}),
                result: Some(serde_json::json!({"text": "result-visible"})),
            },
        ];

        let text = moderation_text(&blocks);
        for visible in [
            "text-visible",
            "text-link-visible.example",
            "code-lang-visible",
            "code-visible",
            "thought-visible",
            "button-visible",
            "button-url-visible.example",
            "file-visible.pdf",
            "voice-visible",
            "machine-placeholder",
            "select-visible",
            "example",
            "card-visible",
            "card-nested-visible",
            "tool-visible",
            "prompt-visible",
            "args-visible",
            "result-visible",
        ] {
            assert!(text.contains(visible), "{visible} must be screened: {text}");
        }
        for opaque in ["thought-hidden", "machine-value"] {
            assert!(
                !text.contains(opaque),
                "{opaque} is machine-only or intentionally hidden: {text}"
            );
        }

        let moderator = KeywordModerator::new(vec!["card-nested-visible".into()]);
        assert!(matches!(
            moderator.check(&blocks),
            ModerationVerdict::Block(_)
        ));
        let url_moderator = KeywordModerator::new(vec!["button-url-visible.example".into()]);
        assert!(matches!(
            url_moderator.check(&blocks),
            ModerationVerdict::Block(_)
        ));
    }
}
