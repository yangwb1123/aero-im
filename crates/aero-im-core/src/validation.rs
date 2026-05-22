//! Block-level validation for inbound messages.
//!
//! Enforced limits (P1):
//! - At least one [`Block`] per message.
//! - At most 50 blocks per message.
//! - [`Block::Text`] content ≤ 8 KiB.
//! - [`Block::Code`] content ≤ 64 KiB.
//!
//! Link allow-listing and nesting depth limits are deferred to P2.

use aero_common::Block;
use thiserror::Error;

/// Maximum number of blocks allowed in a single message.
pub const MAX_BLOCKS: usize = 50;

/// Maximum byte length of a `Text` block's `content` field.
pub const MAX_TEXT_BYTES: usize = 8 * 1024;

/// Maximum byte length of a `Code` block's `content` field.
pub const MAX_CODE_BYTES: usize = 64 * 1024;

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
}

impl From<ValidationError> for aero_common::Error {
    fn from(e: ValidationError) -> Self {
        aero_common::Error::Invalid(e.to_string())
    }
}

/// Validate a sequence of [`Block`]s against the P1 limits.
///
/// Returns `Ok(())` if all checks pass.
pub fn validate_blocks(blocks: &[Block]) -> Result<(), ValidationError> {
    if blocks.is_empty() {
        return Err(ValidationError::Empty);
    }
    if blocks.len() > MAX_BLOCKS {
        return Err(ValidationError::TooManyBlocks { actual: blocks.len() });
    }
    for (index, block) in blocks.iter().enumerate() {
        match block {
            Block::Text { content, .. } => {
                if content.len() > MAX_TEXT_BYTES {
                    return Err(ValidationError::TextTooLong {
                        index,
                        actual: content.len(),
                    });
                }
            }
            Block::Code { content, .. } => {
                if content.len() > MAX_CODE_BYTES {
                    return Err(ValidationError::CodeTooLong {
                        index,
                        actual: content.len(),
                    });
                }
            }
            // Other block variants have no length limits at this layer.
            Block::Mention { .. }
            | Block::File { .. }
            | Block::Voice { .. }
            | Block::Card { .. }
            | Block::ToolCall { .. }
            | Block::Thought { .. } => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_common::Block;
    use pretty_assertions::assert_eq;

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
        assert_eq!(err, ValidationError::TooManyBlocks { actual: MAX_BLOCKS + 1 });
    }

    #[test]
    fn text_too_long_rejected() {
        let big = "a".repeat(MAX_TEXT_BYTES + 1);
        let blocks = vec![Block::text("ok"), Block::text(big.clone())];
        let err = validate_blocks(&blocks).unwrap_err();
        assert_eq!(
            err,
            ValidationError::TextTooLong { index: 1, actual: MAX_TEXT_BYTES + 1 }
        );
    }

    #[test]
    fn code_too_long_rejected() {
        let big = "a".repeat(MAX_CODE_BYTES + 1);
        let blocks = vec![Block::Code { lang: "rs".into(), content: big }];
        let err = validate_blocks(&blocks).unwrap_err();
        assert_eq!(
            err,
            ValidationError::CodeTooLong { index: 0, actual: MAX_CODE_BYTES + 1 }
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
        validate_blocks(&[Block::Code { lang: "rs".into(), content: at_limit }]).unwrap();
    }

    #[test]
    fn at_max_blocks_ok() {
        let blocks: Vec<Block> = (0..MAX_BLOCKS).map(|_| Block::text("x")).collect();
        validate_blocks(&blocks).unwrap();
    }

    #[test]
    fn non_text_blocks_no_length_check() {
        let blocks = vec![
            Block::Mention {
                participant: aero_common::ParticipantId::new(),
            },
            Block::Thought { content: "thinking".into(), hidden: false },
            Block::Card { schema: "x".into(), payload: serde_json::json!({}) },
        ];
        validate_blocks(&blocks).unwrap();
    }

    #[test]
    fn validation_error_converts_to_common_error() {
        let err: aero_common::Error = ValidationError::Empty.into();
        assert!(matches!(err, aero_common::Error::Invalid(_)));
    }
}
