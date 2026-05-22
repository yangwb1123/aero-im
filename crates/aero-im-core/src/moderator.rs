//! Lightweight pre-publish content moderator.
//!
//! Default impl reads `AERO_BLOCKED_WORDS` (comma-separated, case-insensitive)
//! and rejects messages whose textual block content contains any forbidden
//! substring. P5 stub — real moderation runs as an `AiJobKind::Moderate` job
//! in `aero-ai`, where the verdict can also short-circuit publish via this
//! interface or via post-publish soft-delete.
//!
//! Replace `KeywordModerator` with an LLM-backed implementation by providing a
//! different `Moderator` to `ImService`.

use aero_common::Block;

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
        let words: Vec<String> = raw.split(',').map(|s| s.to_owned()).collect();
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
        for b in blocks {
            if let Some(t) = b.searchable_text() {
                let lower = t.to_lowercase();
                for n in &self.needles {
                    if lower.contains(n) {
                        return ModerationVerdict::Block(format!(
                            "包含被屏蔽的关键词:{n}"
                        ));
                    }
                }
            }
        }
        ModerationVerdict::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_all_passes() {
        let m = AllowAllModerator;
        assert_eq!(m.check(&[Block::text("anything")]), ModerationVerdict::Allow);
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
        assert_eq!(m.check(&[Block::text("hello world")]), ModerationVerdict::Allow);
    }

    #[test]
    fn empty_blocklist_allows() {
        let m = KeywordModerator::new(vec![]);
        assert_eq!(m.check(&[Block::text("anything")]), ModerationVerdict::Allow);
    }
}
