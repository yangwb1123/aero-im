//! AiService — the high-level AI facade.
//!
//! Split from monolithic `service.rs` (2011 lines) as part of REFACTOR_PLAN.md
//! Step 3. The main `impl AiService` blocks live in sub-modules.

use std::sync::Arc;

use aero_common::{Message, MessageId, ParticipantId, RoomId};
use aero_storage::{AiContextStore, AiJobRepo, AiProfileRepo, MessageRepo, RoomRepo, SearchHit};

use crate::anthropic::AnthropicClient;
use crate::embed::Embedder;
use crate::transcribe::Transcriber;

pub mod profile;
pub mod service_impl;
pub mod tools;
#[cfg(test)]
pub mod tests;

pub use profile::{cross_room_profile_enabled, ExtractedProfile};

pub(crate) use tools::*;

pub(crate) use service_impl::parse_sentiment_verdict;

///
/// `answer` is human-readable text suitable for direct display. `citations`
/// references the underlying messages so the UI can render "source" chips
/// linking back into the conversation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AnswerResult {
    pub answer: String,
    pub citations: Vec<MessageId>,
}

/// One ranked expert returned by [`AiService::find_expert`].
///
/// `participant` is the candidate authority on the topic; `score` is the summed
/// relevance of their topic-matching messages (higher = stronger signal); and
/// `citations` are a few of the message ids that drove the score so the UI can
/// show "why" — exactly like the RAG citation chips.
#[derive(Debug, Clone)]
pub struct Expert {
    pub participant: ParticipantId,
    pub score: f32,
    pub citations: Vec<MessageId>,
}

/// One recommended channel returned by [`AiService::recommend_channels`].
///
/// A channel the caller is NOT already in, suggested by affinity to their own
/// activity. `room` is the channel id; `name` is its display name (or a fallback
/// when unnamed); `score` is the (normalized) affinity strength, higher = stronger;
/// and `reason` is a short human-readable "why this channel" string the UI shows.
#[derive(Debug, Clone)]
pub struct ChannelRec {
    pub room: RoomId,
    pub name: String,
    pub score: f32,
    pub reason: String,
}

/// One recommended person returned by [`AiService::recommend_people`].
///
/// A workspace member the caller does NOT already follow (and is not themselves),
/// suggested by shared-room overlap. `participant` is the candidate; `score` is the
/// affinity strength; and `reason` is a short "why follow them" string.
#[derive(Debug, Clone)]
pub struct PersonRec {
    pub participant: ParticipantId,
    pub score: f32,
    pub reason: String,
}

/// Coarse polarity bucket for [`SentimentScore::sentiment`].
///
/// Three-way ordinal so the UI can render a single chip (👍 / 😐 / 👎) without
/// having to interpret a continuous score. Serializes lowercase
/// (`"negative" | "neutral" | "positive"`) to match the wire shape the HTTP layer
/// returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sentiment {
    Negative,
    Neutral,
    Positive,
}

impl Sentiment {
    /// The lowercase wire label (`"negative" | "neutral" | "positive"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Sentiment::Negative => "negative",
            Sentiment::Neutral => "neutral",
            Sentiment::Positive => "positive",
        }
    }

    /// Parse a model/heuristic label back into the enum, tolerant of case and
    /// surrounding whitespace. Anything unrecognized falls back to `Neutral` so a
    /// stray LLM line never errors the score.
    #[must_use]
    pub fn from_label(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "negative" | "neg" => Sentiment::Negative,
            "positive" | "pos" => Sentiment::Positive,
            _ => Sentiment::Neutral,
        }
    }
}

/// Result of [`AiService::score_message_sentiment`] — an additive, non-blocking
/// affect read on a single message.
///
/// Distinct from [`AiService::moderate`] (a binary block/allow gate): this never
/// blocks anything, it just describes tone. `sentiment` is the coarse polarity;
/// `toxicity` is a `[0.0, 1.0]` likelihood the text is hostile/abusive (higher =
/// more toxic); and `tone` is a short human-readable label (e.g. `"angry"`,
/// `"friendly"`, `"neutral"`) suitable for a UI chip.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SentimentScore {
    pub sentiment: Sentiment,
    pub toxicity: f32,
    pub tone: String,
}

impl SentimentScore {
    /// The neutral baseline — used for empty text and as the conservative default
    /// when a model verdict cannot be parsed.
    #[must_use]
    pub fn neutral() -> Self {
        Self { sentiment: Sentiment::Neutral, toxicity: 0.0, tone: "neutral".to_owned() }
    }
}

/// Composition root for AI features.
#[derive(Clone)]
pub struct AiService {
    pub(crate) anthropic: Option<Arc<AnthropicClient>>,
    pub(crate) embedder: Arc<dyn Embedder + Send + Sync>,
    pub(crate) transcriber: Arc<dyn Transcriber>,
    pub(crate) ai_jobs: AiJobRepo,
    pub(crate) messages: MessageRepo,
    pub(crate) rooms: RoomRepo,
    /// Optional Redis-backed rolling conversation context.
    /// `None` when Redis is not configured or is unavailable at startup.
    pub(crate) context: Option<AiContextStore>,
    /// Optional blob store, enabling the agentic loop's `read_attachment` tool to
    /// pull a text attachment's bytes (方向三 file RAG). `None` ⇒ the tool isn't
    /// offered and the agent works from message text alone.
    pub(crate) blob_store: Option<Arc<dyn aero_storage::BlobStore>>,
    /// Optional persistent cross-room AI profile store (持久跨房 AI 用户画像).
    ///
    /// PRIVACY-SENSITIVE and OPT-IN: even when wired, the extraction/use paths
    /// only touch it when `AERO_AI_CROSS_ROOM_PROFILE` is set (default OFF), so a
    /// fresh deploy never reads or writes a profile. `None` ⇒ the feature is
    /// entirely absent and every profile path is a no-op.
    pub(crate) ai_profiles: Option<AiProfileRepo>,
}
