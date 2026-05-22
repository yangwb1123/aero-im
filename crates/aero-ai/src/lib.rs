//! AI control plane: gateway, RAG, agent runtime, MCP host, moderation.
//!
//! Real implementation begins in P2 (semantic search, summarization).
//! In P1 we expose the trait-level surface so callers (e.g. embedding worker stubs)
//! compile against stable interfaces.

use async_trait::async_trait;

#[async_trait]
pub trait Embedder: Send + Sync {
    async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>>;
}

/// No-op embedder used in P1; real implementations land in P2.
pub struct NoopEmbedder;

#[async_trait]
impl Embedder for NoopEmbedder {
    async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
        Ok(Vec::new())
    }
}
