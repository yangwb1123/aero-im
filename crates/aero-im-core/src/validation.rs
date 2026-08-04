//! Resource-bound validation for inbound message blocks.
//!
//! The HTTP gateway also accepts blob uploads, so its request-body limit is
//! intentionally much larger than a reasonable structured message. Keep the
//! canonical limit here: every immediate and deferred message write reuses
//! [`validate_blocks`] before persistence.

use std::io::{self, Write};

use aero_common::{Block, SpanStyle};
use thiserror::Error;

/// Maximum number of blocks allowed in a single message.
pub const MAX_BLOCKS: usize = 50;

/// Maximum byte length of a `Text` block's `content` field.
pub const MAX_TEXT_BYTES: usize = 8 * 1024;

/// Maximum byte length of a `Code` block's `content` field.
pub const MAX_CODE_BYTES: usize = 64 * 1024;

/// Maximum serialized JSON size of all blocks in one message.
pub const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

const MAX_TEXT_SPANS: usize = 1_024;
const MAX_LINK_HREF_BYTES: usize = 4 * 1024;
const MAX_CODE_LANG_BYTES: usize = 128;
const MAX_FILE_NAME_BYTES: usize = 1_024;
const MAX_VOICE_TRANSCRIPT_BYTES: usize = 64 * 1024;
const MAX_CARD_SCHEMA_BYTES: usize = 128;
const MAX_CARD_PAYLOAD_BYTES: usize = 256 * 1024;
const MAX_TOOL_NAME_BYTES: usize = 128;
const MAX_TOOL_VALUE_BYTES: usize = 256 * 1024;
const MAX_THOUGHT_BYTES: usize = 64 * 1024;
const MAX_ACTION_ID_BYTES: usize = 256;
const MAX_LABEL_BYTES: usize = 1_024;
const MAX_STYLE_BYTES: usize = 64;
const MAX_URL_BYTES: usize = 4 * 1024;
const MAX_SELECT_OPTIONS: usize = 100;
const MAX_SELECT_VALUE_BYTES: usize = 512;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("message must contain at least one block")]
    Empty,

    #[error("too many blocks: {actual} > {max}", max = MAX_BLOCKS)]
    TooManyBlocks { actual: usize },

    #[error(
        "text block #{index} exceeds {max} bytes (got {actual})",
        max = MAX_TEXT_BYTES
    )]
    TextTooLong { index: usize, actual: usize },

    #[error(
        "code block #{index} exceeds {max} bytes (got {actual})",
        max = MAX_CODE_BYTES
    )]
    CodeTooLong { index: usize, actual: usize },

    #[error("{field} in block #{index} exceeds {max} bytes (got {actual})")]
    FieldTooLong {
        index: usize,
        field: &'static str,
        actual: usize,
        max: usize,
    },

    #[error("{field} in block #{index} has too many items: {actual} > {max}")]
    TooManyItems {
        index: usize,
        field: &'static str,
        actual: usize,
        max: usize,
    },

    #[error("{field} in block #{index} exceeds {max} serialized bytes (got {actual})")]
    JsonTooLarge {
        index: usize,
        field: &'static str,
        actual: usize,
        max: usize,
    },

    #[error(
        "message blocks exceed {max} serialized bytes (got {actual})",
        max = MAX_MESSAGE_BYTES
    )]
    MessageTooLarge { actual: usize },

    #[error("message blocks could not be serialized for validation")]
    SerializationFailed,
}

impl From<ValidationError> for aero_common::Error {
    fn from(e: ValidationError) -> Self {
        aero_common::Error::Invalid(e.to_string())
    }
}

#[derive(Default)]
struct ByteCounter {
    bytes: usize,
}

impl Write for ByteCounter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buf.len());
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialized_len<T: serde::Serialize + ?Sized>(value: &T) -> Result<usize, ValidationError> {
    let mut counter = ByteCounter::default();
    serde_json::to_writer(&mut counter, value).map_err(|_| ValidationError::SerializationFailed)?;
    Ok(counter.bytes)
}

fn check_field(
    index: usize,
    field: &'static str,
    value: &str,
    max: usize,
) -> Result<(), ValidationError> {
    if value.len() > max {
        return Err(ValidationError::FieldTooLong {
            index,
            field,
            actual: value.len(),
            max,
        });
    }
    Ok(())
}

fn check_items(
    index: usize,
    field: &'static str,
    actual: usize,
    max: usize,
) -> Result<(), ValidationError> {
    if actual > max {
        return Err(ValidationError::TooManyItems {
            index,
            field,
            actual,
            max,
        });
    }
    Ok(())
}

fn check_json(
    index: usize,
    field: &'static str,
    value: &serde_json::Value,
    max: usize,
) -> Result<(), ValidationError> {
    let actual = serialized_len(value)?;
    if actual > max {
        return Err(ValidationError::JsonTooLarge {
            index,
            field,
            actual,
            max,
        });
    }
    Ok(())
}

/// Validate every variable-size part of a sequence of [`Block`]s and cap its
/// total serialized representation.
pub fn validate_blocks(blocks: &[Block]) -> Result<(), ValidationError> {
    if blocks.is_empty() {
        return Err(ValidationError::Empty);
    }
    if blocks.len() > MAX_BLOCKS {
        return Err(ValidationError::TooManyBlocks {
            actual: blocks.len(),
        });
    }
    for (index, block) in blocks.iter().enumerate() {
        match block {
            Block::Text { content, spans } => {
                if content.len() > MAX_TEXT_BYTES {
                    return Err(ValidationError::TextTooLong {
                        index,
                        actual: content.len(),
                    });
                }
                check_items(index, "text.spans", spans.len(), MAX_TEXT_SPANS)?;
                for span in spans {
                    if let SpanStyle::Link { href } = &span.style {
                        check_field(index, "text.spans[].href", href, MAX_LINK_HREF_BYTES)?;
                    }
                }
            }
            Block::Mention { .. } => {}
            Block::Code { lang, content } => {
                if content.len() > MAX_CODE_BYTES {
                    return Err(ValidationError::CodeTooLong {
                        index,
                        actual: content.len(),
                    });
                }
                check_field(index, "code.lang", lang, MAX_CODE_LANG_BYTES)?;
            }
            Block::File { name, .. } => {
                check_field(index, "file.name", name, MAX_FILE_NAME_BYTES)?;
            }
            Block::Voice { transcript, .. } => {
                if let Some(transcript) = transcript {
                    check_field(
                        index,
                        "voice.transcript",
                        transcript,
                        MAX_VOICE_TRANSCRIPT_BYTES,
                    )?;
                }
            }
            Block::Card { schema, payload } => {
                check_field(index, "card.schema", schema, MAX_CARD_SCHEMA_BYTES)?;
                check_json(index, "card.payload", payload, MAX_CARD_PAYLOAD_BYTES)?;
            }
            Block::ToolCall { tool, args, result } => {
                check_field(index, "tool_call.tool", tool, MAX_TOOL_NAME_BYTES)?;
                check_json(index, "tool_call.args", args, MAX_TOOL_VALUE_BYTES)?;
                if let Some(result) = result {
                    check_json(index, "tool_call.result", result, MAX_TOOL_VALUE_BYTES)?;
                }
            }
            Block::Thought { content, .. } => {
                check_field(index, "thought.content", content, MAX_THOUGHT_BYTES)?;
            }
            Block::Button {
                action_id,
                label,
                style,
                url,
            } => {
                check_field(index, "button.action_id", action_id, MAX_ACTION_ID_BYTES)?;
                check_field(index, "button.label", label, MAX_LABEL_BYTES)?;
                if let Some(style) = style {
                    check_field(index, "button.style", style, MAX_STYLE_BYTES)?;
                }
                if let Some(url) = url {
                    check_field(index, "button.url", url, MAX_URL_BYTES)?;
                }
            }
            Block::Select {
                action_id,
                placeholder,
                options,
            } => {
                check_field(index, "select.action_id", action_id, MAX_ACTION_ID_BYTES)?;
                if let Some(placeholder) = placeholder {
                    check_field(index, "select.placeholder", placeholder, MAX_LABEL_BYTES)?;
                }
                check_items(index, "select.options", options.len(), MAX_SELECT_OPTIONS)?;
                for option in options {
                    check_field(
                        index,
                        "select.options[].value",
                        &option.value,
                        MAX_SELECT_VALUE_BYTES,
                    )?;
                    check_field(
                        index,
                        "select.options[].label",
                        &option.label,
                        MAX_LABEL_BYTES,
                    )?;
                }
            }
        }
    }

    let actual = serialized_len(blocks)?;
    if actual > MAX_MESSAGE_BYTES {
        return Err(ValidationError::MessageTooLarge { actual });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_common::{BlobId, Block, FileKind, SelectOption, Span};
    use pretty_assertions::assert_eq;

    fn assert_field_limit(at_limit: Block, over_limit: Block, field: &'static str, max: usize) {
        validate_blocks(&[at_limit]).unwrap();
        assert_eq!(
            validate_blocks(&[over_limit]).unwrap_err(),
            ValidationError::FieldTooLong {
                index: 0,
                field,
                actual: max + 1,
                max,
            }
        );
    }

    fn json_string_with_size(size: usize) -> serde_json::Value {
        assert!(size >= 2);
        serde_json::Value::String("x".repeat(size - 2))
    }

    #[test]
    fn empty_blocks_rejected() {
        let err = validate_blocks(&[]).unwrap_err();
        assert_eq!(err, ValidationError::Empty);
    }

    #[test]
    fn single_text_block_ok() {
        validate_blocks(&[Block::text("hello")]).unwrap();
    }

    #[test]
    fn too_many_blocks_rejected() {
        let blocks: Vec<Block> = (0..=MAX_BLOCKS).map(|_| Block::text("x")).collect();
        let err = validate_blocks(&blocks).unwrap_err();
        assert_eq!(
            err,
            ValidationError::TooManyBlocks {
                actual: MAX_BLOCKS + 1
            }
        );
    }

    #[test]
    fn text_too_long_rejected() {
        let big = "a".repeat(MAX_TEXT_BYTES + 1);
        let blocks = vec![Block::text("ok"), Block::text(big.clone())];
        let err = validate_blocks(&blocks).unwrap_err();
        assert_eq!(
            err,
            ValidationError::TextTooLong {
                index: 1,
                actual: MAX_TEXT_BYTES + 1
            }
        );
    }

    #[test]
    fn code_too_long_rejected() {
        let big = "a".repeat(MAX_CODE_BYTES + 1);
        let blocks = vec![Block::Code {
            lang: "rs".into(),
            content: big,
        }];
        let err = validate_blocks(&blocks).unwrap_err();
        assert_eq!(
            err,
            ValidationError::CodeTooLong {
                index: 0,
                actual: MAX_CODE_BYTES + 1
            }
        );
    }

    #[test]
    fn text_at_limit_ok() {
        let at_limit = "a".repeat(MAX_TEXT_BYTES);
        validate_blocks(&[Block::text(at_limit)]).unwrap();
    }

    #[test]
    fn code_at_limit_ok() {
        let at_limit = "a".repeat(MAX_CODE_BYTES);
        validate_blocks(&[Block::Code {
            lang: "rs".into(),
            content: at_limit,
        }])
        .unwrap();
    }

    #[test]
    fn at_max_blocks_ok() {
        let blocks: Vec<Block> = (0..MAX_BLOCKS).map(|_| Block::text("x")).collect();
        validate_blocks(&blocks).unwrap();
    }

    #[test]
    fn text_span_collection_and_link_are_bounded() {
        let span = Span {
            start: 0,
            end: 0,
            style: SpanStyle::Bold,
        };
        validate_blocks(&[Block::Text {
            content: "ok".into(),
            spans: vec![span.clone(); MAX_TEXT_SPANS],
        }])
        .unwrap();
        assert_eq!(
            validate_blocks(&[Block::Text {
                content: "ok".into(),
                spans: vec![span; MAX_TEXT_SPANS + 1],
            }])
            .unwrap_err(),
            ValidationError::TooManyItems {
                index: 0,
                field: "text.spans",
                actual: MAX_TEXT_SPANS + 1,
                max: MAX_TEXT_SPANS,
            }
        );

        let link = |bytes| Block::Text {
            content: "ok".into(),
            spans: vec![Span {
                start: 0,
                end: 1,
                style: SpanStyle::Link {
                    href: "x".repeat(bytes),
                },
            }],
        };
        assert_field_limit(
            link(MAX_LINK_HREF_BYTES),
            link(MAX_LINK_HREF_BYTES + 1),
            "text.spans[].href",
            MAX_LINK_HREF_BYTES,
        );
    }

    #[test]
    fn mention_and_code_language_are_bounded() {
        validate_blocks(&[Block::Mention {
            participant: aero_common::ParticipantId::new(),
        }])
        .unwrap();
        let code = |bytes| Block::Code {
            lang: "x".repeat(bytes),
            content: String::new(),
        };
        assert_field_limit(
            code(MAX_CODE_LANG_BYTES),
            code(MAX_CODE_LANG_BYTES + 1),
            "code.lang",
            MAX_CODE_LANG_BYTES,
        );
    }

    #[test]
    fn file_name_is_bounded() {
        let file = |bytes| Block::File {
            blob_id: BlobId::new(),
            kind: FileKind::Document,
            name: "x".repeat(bytes),
            size: 1,
        };
        assert_field_limit(
            file(MAX_FILE_NAME_BYTES),
            file(MAX_FILE_NAME_BYTES + 1),
            "file.name",
            MAX_FILE_NAME_BYTES,
        );
    }

    #[test]
    fn voice_transcript_is_bounded() {
        let voice = |bytes| Block::Voice {
            blob_id: BlobId::new(),
            duration_ms: 1,
            transcript: Some("x".repeat(bytes)),
        };
        assert_field_limit(
            voice(MAX_VOICE_TRANSCRIPT_BYTES),
            voice(MAX_VOICE_TRANSCRIPT_BYTES + 1),
            "voice.transcript",
            MAX_VOICE_TRANSCRIPT_BYTES,
        );
    }

    #[test]
    fn card_schema_and_payload_are_bounded() {
        let card = |schema_bytes, payload| Block::Card {
            schema: "x".repeat(schema_bytes),
            payload,
        };
        assert_field_limit(
            card(MAX_CARD_SCHEMA_BYTES, serde_json::json!({})),
            card(MAX_CARD_SCHEMA_BYTES + 1, serde_json::json!({})),
            "card.schema",
            MAX_CARD_SCHEMA_BYTES,
        );
        validate_blocks(&[card(1, json_string_with_size(MAX_CARD_PAYLOAD_BYTES))]).unwrap();
        assert_eq!(
            validate_blocks(&[card(1, json_string_with_size(MAX_CARD_PAYLOAD_BYTES + 1),)])
                .unwrap_err(),
            ValidationError::JsonTooLarge {
                index: 0,
                field: "card.payload",
                actual: MAX_CARD_PAYLOAD_BYTES + 1,
                max: MAX_CARD_PAYLOAD_BYTES,
            }
        );
    }

    #[test]
    fn tool_call_fields_are_bounded() {
        let tool_call = |tool_bytes, args, result| Block::ToolCall {
            tool: "x".repeat(tool_bytes),
            args,
            result,
        };
        assert_field_limit(
            tool_call(MAX_TOOL_NAME_BYTES, serde_json::json!({}), None),
            tool_call(MAX_TOOL_NAME_BYTES + 1, serde_json::json!({}), None),
            "tool_call.tool",
            MAX_TOOL_NAME_BYTES,
        );
        validate_blocks(&[tool_call(
            1,
            json_string_with_size(MAX_TOOL_VALUE_BYTES),
            Some(json_string_with_size(MAX_TOOL_VALUE_BYTES)),
        )])
        .unwrap();
        for (field, args, result) in [
            (
                "tool_call.args",
                json_string_with_size(MAX_TOOL_VALUE_BYTES + 1),
                None,
            ),
            (
                "tool_call.result",
                serde_json::json!({}),
                Some(json_string_with_size(MAX_TOOL_VALUE_BYTES + 1)),
            ),
        ] {
            assert_eq!(
                validate_blocks(&[tool_call(1, args, result)]).unwrap_err(),
                ValidationError::JsonTooLarge {
                    index: 0,
                    field,
                    actual: MAX_TOOL_VALUE_BYTES + 1,
                    max: MAX_TOOL_VALUE_BYTES,
                }
            );
        }
    }

    #[test]
    fn thought_content_is_bounded() {
        let thought = |bytes| Block::Thought {
            content: "x".repeat(bytes),
            hidden: false,
        };
        assert_field_limit(
            thought(MAX_THOUGHT_BYTES),
            thought(MAX_THOUGHT_BYTES + 1),
            "thought.content",
            MAX_THOUGHT_BYTES,
        );
    }

    #[test]
    fn button_fields_are_bounded() {
        let button = |action, label, style, url| Block::Button {
            action_id: action,
            label,
            style,
            url,
        };
        for (field, max, at_limit, over_limit) in [
            (
                "button.action_id",
                MAX_ACTION_ID_BYTES,
                button("x".repeat(MAX_ACTION_ID_BYTES), String::new(), None, None),
                button(
                    "x".repeat(MAX_ACTION_ID_BYTES + 1),
                    String::new(),
                    None,
                    None,
                ),
            ),
            (
                "button.label",
                MAX_LABEL_BYTES,
                button(String::new(), "x".repeat(MAX_LABEL_BYTES), None, None),
                button(String::new(), "x".repeat(MAX_LABEL_BYTES + 1), None, None),
            ),
            (
                "button.style",
                MAX_STYLE_BYTES,
                button(
                    String::new(),
                    String::new(),
                    Some("x".repeat(MAX_STYLE_BYTES)),
                    None,
                ),
                button(
                    String::new(),
                    String::new(),
                    Some("x".repeat(MAX_STYLE_BYTES + 1)),
                    None,
                ),
            ),
            (
                "button.url",
                MAX_URL_BYTES,
                button(
                    String::new(),
                    String::new(),
                    None,
                    Some("x".repeat(MAX_URL_BYTES)),
                ),
                button(
                    String::new(),
                    String::new(),
                    None,
                    Some("x".repeat(MAX_URL_BYTES + 1)),
                ),
            ),
        ] {
            assert_field_limit(at_limit, over_limit, field, max);
        }
    }

    #[test]
    fn select_fields_and_options_are_bounded() {
        let select = |action_id, placeholder, options| Block::Select {
            action_id,
            placeholder,
            options,
        };
        assert_field_limit(
            select("x".repeat(MAX_ACTION_ID_BYTES), None, Vec::new()),
            select("x".repeat(MAX_ACTION_ID_BYTES + 1), None, Vec::new()),
            "select.action_id",
            MAX_ACTION_ID_BYTES,
        );
        assert_field_limit(
            select(String::new(), Some("x".repeat(MAX_LABEL_BYTES)), Vec::new()),
            select(
                String::new(),
                Some("x".repeat(MAX_LABEL_BYTES + 1)),
                Vec::new(),
            ),
            "select.placeholder",
            MAX_LABEL_BYTES,
        );

        let empty_option = SelectOption {
            value: String::new(),
            label: String::new(),
        };
        validate_blocks(&[select(
            String::new(),
            None,
            vec![empty_option.clone(); MAX_SELECT_OPTIONS],
        )])
        .unwrap();
        assert_eq!(
            validate_blocks(&[select(
                String::new(),
                None,
                vec![empty_option; MAX_SELECT_OPTIONS + 1],
            )])
            .unwrap_err(),
            ValidationError::TooManyItems {
                index: 0,
                field: "select.options",
                actual: MAX_SELECT_OPTIONS + 1,
                max: MAX_SELECT_OPTIONS,
            }
        );

        let option = |value_bytes, label_bytes| SelectOption {
            value: "x".repeat(value_bytes),
            label: "x".repeat(label_bytes),
        };
        for (field, max, at_limit, over_limit) in [
            (
                "select.options[].value",
                MAX_SELECT_VALUE_BYTES,
                option(MAX_SELECT_VALUE_BYTES, 0),
                option(MAX_SELECT_VALUE_BYTES + 1, 0),
            ),
            (
                "select.options[].label",
                MAX_LABEL_BYTES,
                option(0, MAX_LABEL_BYTES),
                option(0, MAX_LABEL_BYTES + 1),
            ),
        ] {
            assert_field_limit(
                select(String::new(), None, vec![at_limit]),
                select(String::new(), None, vec![over_limit]),
                field,
                max,
            );
        }
    }

    #[test]
    fn total_serialized_size_is_bounded_exactly() {
        let mut blocks: Vec<Block> = (0..15)
            .map(|_| Block::Card {
                schema: "x".into(),
                payload: json_string_with_size(MAX_CARD_PAYLOAD_BYTES),
            })
            .collect();
        blocks.push(Block::Card {
            schema: "x".into(),
            payload: serde_json::Value::String(String::new()),
        });
        let deficit = MAX_MESSAGE_BYTES - serialized_len(&blocks).unwrap();
        if let Block::Card { payload, .. } = blocks.last_mut().unwrap() {
            *payload = serde_json::Value::String("x".repeat(deficit));
        }
        assert_eq!(serialized_len(&blocks).unwrap(), MAX_MESSAGE_BYTES);
        validate_blocks(&blocks).unwrap();

        if let Block::Card {
            payload: serde_json::Value::String(last),
            ..
        } = blocks.last_mut().unwrap()
        {
            last.push('x');
        }
        assert_eq!(
            validate_blocks(&blocks).unwrap_err(),
            ValidationError::MessageTooLarge {
                actual: MAX_MESSAGE_BYTES + 1,
            }
        );
    }

    #[test]
    fn validation_error_converts_to_common_error() {
        let err: aero_common::Error = ValidationError::Empty.into();
        assert!(matches!(err, aero_common::Error::Invalid(_)));
    }
}
