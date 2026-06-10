//! AI control plane for Aero IM (P2 milestone).
//!
//! Wires together:
//! - [`anthropic::AnthropicClient`] — thin Messages API client driven by env
//!   (`ANTHROPIC_API_KEY`, `ANTHROPIC_MODEL`).
//! - [`embed::Embedder`] — pluggable embedding interface with two impls:
//!   `VoyageEmbedder` (production) and `HashEmbedder` (deterministic fallback).
//! - [`service::AiService`] — high-level operations (embed / summarize / answer)
//!   composed from the above + `aero-storage` repos.
//! - [`worker::AiWorker`] — long-running drain of the `ai_jobs` queue.
//!
//! The server constructs a single `AiService` from env, hands an `Arc` of it to
//! its Axum router (for live request paths), and spawns an `AiWorker` task for
//! background work.

pub mod anthropic;
pub mod budget;
pub mod embed;
pub mod error;
pub mod metrics;
pub mod rerank;
pub mod service;
pub mod transcribe;
pub mod worker;

pub use anthropic::{AnthropicClient, ChatMsg, Usage};
pub use budget::{CostBudget, KeyedCostBudget};
pub use metrics::CostModel;
pub use embed::{default_embedder, Embedder, HashEmbedder, VoyageEmbedder, EMBED_DIM};
pub use error::{AiError, Result};
pub use rerank::fuse_rankings;
pub use service::{AiService, AnswerResult};
pub use transcribe::{default_transcriber, StubTranscriber, Transcriber, WhisperTranscriber};
pub use worker::{AiWorker, WorkerConfig, MAX_ATTEMPTS};
