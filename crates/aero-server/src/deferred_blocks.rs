//! Shared validation for message bodies persisted for deferred replay.
//!
//! Scheduled messages, recurring messages, and templates must accept exactly
//! the same block payloads as the ordinary message send path. Keeping the JSON
//! decoding and [`aero_im_core::validate_blocks`] call here prevents a deferred
//! write from bypassing the canonical block-count and content-size limits.

use aero_common::{Block, Error as AeroError};

/// Decode a JSON value into the canonical wire representation and validate it
/// before any deferred payload is written to `PostgreSQL`.
pub(crate) fn decode(value: serde_json::Value) -> Result<Vec<Block>, AeroError> {
    let blocks = serde_json::from_value::<Vec<Block>>(value)
        .map_err(|error| AeroError::Invalid(format!("blocks: {error}")))?;
    validate(&blocks)?;
    Ok(blocks)
}

/// Apply the ordinary message-send limits to an already decoded block list.
pub(crate) fn validate(blocks: &[Block]) -> Result<(), AeroError> {
    aero_im_core::validate_blocks(blocks).map_err(AeroError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_im_core::{MAX_BLOCKS, MAX_CODE_BYTES, MAX_MESSAGE_BYTES, MAX_TEXT_BYTES};

    #[test]
    fn malformed_and_empty_payloads_are_rejected() {
        let malformed_shape = decode(serde_json::json!({ "type": "text", "content": "hi" }))
            .expect_err("blocks must be an array");
        assert!(matches!(malformed_shape, AeroError::Invalid(_)));

        let malformed_block =
            decode(serde_json::json!([{ "type": "not_a_block", "content": "hi" }]))
                .expect_err("unknown block variants must fail");
        assert!(matches!(malformed_block, AeroError::Invalid(_)));

        let empty = decode(serde_json::json!([])).expect_err("empty messages must fail");
        assert!(matches!(empty, AeroError::Invalid(message) if message.contains("at least one")));
    }

    #[test]
    fn count_and_content_limits_match_the_message_send_path() {
        let too_many = serde_json::Value::Array(
            (0..=MAX_BLOCKS)
                .map(|_| serde_json::json!({ "type": "text", "content": "x" }))
                .collect(),
        );
        let count_error = decode(too_many).expect_err("too many blocks must fail");
        assert!(
            matches!(count_error, AeroError::Invalid(message) if message.contains("too many blocks"))
        );

        let text_error = decode(serde_json::json!([{
            "type": "text",
            "content": "t".repeat(MAX_TEXT_BYTES + 1),
        }]))
        .expect_err("oversized text must fail");
        assert!(
            matches!(text_error, AeroError::Invalid(message) if message.contains("text block"))
        );

        let code_error = decode(serde_json::json!([{
            "type": "code",
            "lang": "rust",
            "content": "c".repeat(MAX_CODE_BYTES + 1),
        }]))
        .expect_err("oversized code must fail");
        assert!(
            matches!(code_error, AeroError::Invalid(message) if message.contains("code block"))
        );

        let card_error = decode(serde_json::json!([{
            "type": "card",
            "schema": "custom",
            "payload": "x".repeat(MAX_MESSAGE_BYTES),
        }]))
        .expect_err("oversized card payload must fail through canonical validation");
        assert!(
            matches!(card_error, AeroError::Invalid(message) if message.contains("card.payload"))
        );
    }

    #[test]
    fn valid_wire_payload_is_decoded_without_shape_changes() {
        let blocks = decode(serde_json::json!([
            { "type": "text", "content": "ship it" },
            { "type": "code", "lang": "rust", "content": "fn main() {}" }
        ]))
        .expect("valid blocks");

        assert_eq!(blocks.len(), 2);
        assert!(matches!(
            &blocks[0],
            Block::Text { content, .. } if content == "ship it"
        ));
        assert!(matches!(
            &blocks[1],
            Block::Code { lang, content } if lang == "rust" && content == "fn main() {}"
        ));
    }
}
