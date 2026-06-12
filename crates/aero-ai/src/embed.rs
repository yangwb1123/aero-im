//! Embedding providers.
//!
//! We expose a small `Embedder` trait with two implementations:
//! - [`VoyageEmbedder`] — calls Voyage AI's `voyage-3` model (1024-dim).
//!   Used in production when `VOYAGE_API_KEY` is set.
//! - [`HashEmbedder`] — deterministic local fallback that hashes tokens into a
//!   fixed-size dense vector. Pure CPU, zero dependencies, useful for dev/tests
//!   where we want a 1024-dim vector in the DB without requiring a network call.
//!
//! The embedding dimension is fixed at 1024 across implementations so the
//! `messages.embedding` column (pgvector) has a single, stable size and Voyage
//! results are directly substitutable for fallback hashes after a backfill.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;

use crate::error::{AiError, Result};

/// The fixed embedding dimension used across the system.
///
/// `voyage-3` returns 1024-dim vectors. The hash fallback matches this so DB
/// rows stay valid when callers swap implementations.
pub const EMBED_DIM: usize = 1024;

const ENV_VOYAGE_KEY: &str = "VOYAGE_API_KEY";
const ENV_VOYAGE_MODEL: &str = "VOYAGE_MODEL";
const VOYAGE_URL: &str = "https://api.voyageai.com/v1/embeddings";
const VOYAGE_DEFAULT_MODEL: &str = "voyage-3";

#[async_trait]
pub trait Embedder: Send + Sync {
    /// Embed a single text into a dense vector of length [`Embedder::dim`].
    /// This is the DOCUMENT/corpus side — used when storing message embeddings.
    async fn embed_one(&self, text: &str) -> Result<Vec<f32>>;

    /// Embed a search QUERY. Some providers (e.g. `voyage-3`) use asymmetric
    /// query/document embeddings: embedding a user question into the *document*
    /// space measurably degrades retrieval recall. Defaults to [`Self::embed_one`]
    /// for role-agnostic embedders (the keyless [`HashEmbedder`] fallback), so
    /// offline behaviour is unchanged. ROADMAP 方向四.
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        self.embed_one(text).await
    }

    /// Vector dimensionality. Stable for the lifetime of the instance.
    fn dim(&self) -> usize;

    /// Human-readable model identifier — recorded on completed Embed jobs so
    /// operators can tell which model produced which row.
    fn model_id(&self) -> &str;
}

// ---------- Voyage AI ----------

/// `voyage-3` API client. Constructed via [`VoyageEmbedder::from_env`] so unit
/// tests can run without network access.
#[derive(Clone)]
pub struct VoyageEmbedder {
    api_key: String,
    model: String,
    http: reqwest::Client,
}

impl VoyageEmbedder {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { api_key: api_key.into(), model: model.into(), http }
    }

    #[must_use]
    pub fn from_env() -> Option<Arc<Self>> {
        let key = std::env::var(ENV_VOYAGE_KEY).ok()?;
        if key.trim().is_empty() {
            return None;
        }
        let model = std::env::var(ENV_VOYAGE_MODEL)
            .unwrap_or_else(|_| VOYAGE_DEFAULT_MODEL.to_string());
        Some(Arc::new(Self::new(key, model)))
    }
}

impl VoyageEmbedder {
    /// Shared embed call parameterised by voyage's `input_type` ("document" for
    /// the corpus side, "query" for searches) — the only difference between the
    /// two roles (ROADMAP 方向四).
    async fn embed_with(&self, text: &str, input_type: &str) -> Result<Vec<f32>> {
        if text.is_empty() {
            // Voyage rejects empty input — return a deterministic zero vector
            // so the DB column stays populated. Zero-vector cosine similarity
            // is undefined but never selected since we filter on non-NULL.
            return Ok(vec![0.0; EMBED_DIM]);
        }

        let body = VoyageRequest {
            model: &self.model,
            input: vec![text],
            input_type: Some(input_type),
        };
        let resp = self
            .http
            .post(VOYAGE_URL)
            .bearer_auth(&self.api_key)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        let raw = resp.text().await?;
        if !status.is_success() {
            return Err(AiError::Embedding(format!(
                "voyage {}: {}",
                status.as_u16(),
                raw.chars().take(512).collect::<String>()
            )));
        }
        let parsed: VoyageResponse = serde_json::from_str(&raw)?;
        let first = parsed
            .data
            .into_iter()
            .next()
            .ok_or_else(|| AiError::Embedding("voyage returned no embeddings".into()))?;
        if first.embedding.len() != EMBED_DIM {
            return Err(AiError::Embedding(format!(
                "voyage returned dim {}, expected {}",
                first.embedding.len(),
                EMBED_DIM
            )));
        }
        Ok(first.embedding)
    }
}

#[async_trait]
impl Embedder for VoyageEmbedder {
    async fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        self.embed_with(text, "document").await
    }

    async fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        self.embed_with(text, "query").await
    }

    fn dim(&self) -> usize {
        EMBED_DIM
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}

#[derive(Serialize)]
struct VoyageRequest<'a> {
    model: &'a str,
    input: Vec<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_type: Option<&'a str>,
}

#[derive(Deserialize)]
struct VoyageResponse {
    data: Vec<VoyageEmbedding>,
}

#[derive(Deserialize)]
struct VoyageEmbedding {
    embedding: Vec<f32>,
}

// ---------- Hash fallback ----------

/// Deterministic local embedder.
///
/// Tokenizes on Unicode whitespace, lower-cases ASCII, hashes each token into
/// a bucket via `DefaultHasher`, and L2-normalizes the result. The output is
/// 1024-dim like `voyage-3` so DB rows produced by either embedder share a
/// schema.
///
/// Cosine similarity over hash vectors is rough but correctly identifies
/// shared-vocabulary messages — good enough for dev/tests and for `cargo test`
/// to exercise the same code path as production.
pub struct HashEmbedder {
    dim: usize,
}

impl HashEmbedder {
    #[must_use]
    pub fn new() -> Self {
        Self { dim: EMBED_DIM }
    }
}

impl Default for HashEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Embedder for HashEmbedder {
    async fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut v = vec![0.0f32; self.dim];
        for token in tokenize(text) {
            let mut hasher = DefaultHasher::new();
            token.hash(&mut hasher);
            let h = hasher.finish();
            // Wrap into bucket index — truncation is intentional (hash collision is fine).
            #[allow(clippy::cast_possible_truncation)]
            let bucket = (h as usize) % self.dim;
            // Sign derived from a different bit so collisions partially cancel.
            let sign = if (h >> 32) & 1 == 0 { 1.0f32 } else { -1.0f32 };
            v[bucket] += sign;
        }
        // L2 normalize so cosine == dot product, matching pgvector's `<=>` semantics.
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x /= norm;
            }
        }
        Ok(v)
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn model_id(&self) -> &'static str {
        "hash-1024"
    }
}

fn tokenize(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
}

// ---------- Selector ----------

/// Return the best embedder available given environment configuration.
///
/// Prefers `VoyageEmbedder` when `VOYAGE_API_KEY` is set. Falls back to the
/// in-process `HashEmbedder` so dev and tests work offline.
#[must_use]
pub fn default_embedder() -> Arc<dyn Embedder + Send + Sync> {
    if let Some(v) = VoyageEmbedder::from_env() {
        tracing::info!(model = v.model_id(), "ai: using Voyage embedder");
        return v;
    }
    tracing::info!(model = "hash-1024", "ai: using local hash embedder (no VOYAGE_API_KEY)");
    Arc::new(HashEmbedder::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hash_embedder_query_role_matches_document() {
        // The keyless fallback is role-agnostic: embed_query defaults to
        // embed_one, so offline retrieval is byte-identical to before (方向四).
        let e = HashEmbedder::new();
        let q = e.embed_query("when is the launch").await.unwrap();
        let d = e.embed_one("when is the launch").await.unwrap();
        assert_eq!(q, d, "HashEmbedder query == document (no asymmetry offline)");
        assert_eq!(q.len(), EMBED_DIM);
    }

    #[tokio::test]
    async fn hash_embedder_has_fixed_dim() {
        let e = HashEmbedder::new();
        assert_eq!(e.dim(), EMBED_DIM);
        let v = e.embed_one("hello world").await.unwrap();
        assert_eq!(v.len(), EMBED_DIM);
    }

    #[tokio::test]
    async fn hash_embedder_is_deterministic() {
        let e = HashEmbedder::new();
        let v1 = e.embed_one("the quick brown fox").await.unwrap();
        let v2 = e.embed_one("the quick brown fox").await.unwrap();
        assert_eq!(v1, v2);
    }

    #[tokio::test]
    async fn hash_embedder_is_normalized() {
        let e = HashEmbedder::new();
        let v = e.embed_one("hello there friend").await.unwrap();
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "vector should be L2-normalized, got norm={norm}");
    }

    #[tokio::test]
    async fn hash_embedder_differs_for_distinct_inputs() {
        let e = HashEmbedder::new();
        let a = e.embed_one("rust async programming").await.unwrap();
        let b = e.embed_one("javascript dom manipulation").await.unwrap();
        // Cosine similarity = dot product since both are unit vectors
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        // Different topics should not be near-identical
        assert!(dot < 0.9, "expected low similarity for unrelated text, got {dot}");
    }

    #[tokio::test]
    async fn hash_embedder_handles_empty_and_punctuation() {
        let e = HashEmbedder::new();
        let v = e.embed_one("").await.unwrap();
        assert_eq!(v.len(), EMBED_DIM);
        // No tokens -> all zeros, no NaN from division by zero
        assert!(v.iter().all(|x| x.is_finite()));

        let v2 = e.embed_one("!!!---///").await.unwrap();
        assert_eq!(v2.len(), EMBED_DIM);
        assert!(v2.iter().all(|x| x.is_finite()));
    }

    #[tokio::test]
    async fn hash_embedder_model_id_stable() {
        let e = HashEmbedder::new();
        assert_eq!(e.model_id(), "hash-1024");
    }

    #[test]
    fn tokenize_splits_on_punct_and_lowercases() {
        let toks: Vec<String> = tokenize("Hello, World! 你好").collect();
        assert_eq!(toks, vec!["hello", "world", "你好"]);
    }
}
