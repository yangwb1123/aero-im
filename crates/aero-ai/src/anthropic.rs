//! Thin client for the Anthropic Messages API.
//!
//! Only the subset we need: a single `complete()` call that takes a system prompt
//! plus an ordered list of `(role, content)` turns and returns the concatenated
//! text of the assistant's reply. Streaming and tool use are deliberately out of
//! scope for P2 — the surface stays small so callers (summarize / answer) don't
//! pay for features they don't use.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{AiError, Result};

const ENV_API_KEY: &str = "ANTHROPIC_API_KEY";
const ENV_MODEL: &str = "ANTHROPIC_MODEL";
const ENV_BASE_URL: &str = "ANTHROPIC_BASE_URL";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
const API_VERSION: &str = "2023-06-01";

/// A single chat turn passed to Anthropic.
///
/// `role` is `"user"` or `"assistant"`. We intentionally keep this as a plain
/// string rather than an enum so callers can use the same struct shape as the
/// wire format without a translation layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMsg {
    pub role: String,
    pub content: String,
}

impl ChatMsg {
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: "user".into(), content: content.into() }
    }

    #[must_use]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: "assistant".into(), content: content.into() }
    }
}

/// HTTP client for `POST /v1/messages`.
///
/// Holds a long-lived `reqwest::Client` so connections are pooled across calls.
#[derive(Clone)]
pub struct AnthropicClient {
    api_key: String,
    base_url: String,
    model: String,
    http: reqwest::Client,
}

impl AnthropicClient {
    /// Construct a client with explicit credentials.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            model: model.into(),
            http,
        }
    }

    /// Override the API base URL (useful for proxies and tests).
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Build a client from `ANTHROPIC_*` env vars. Returns `None` if the API key
    /// is absent — callers should treat that as "Anthropic disabled" and fall back
    /// to a heuristic path rather than failing the whole feature.
    #[must_use]
    pub fn from_env() -> Option<Arc<Self>> {
        let key = std::env::var(ENV_API_KEY).ok()?;
        if key.trim().is_empty() {
            return None;
        }
        let model = std::env::var(ENV_MODEL).unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        let mut client = Self::new(key, model);
        if let Ok(base) = std::env::var(ENV_BASE_URL) {
            if !base.trim().is_empty() {
                client = client.with_base_url(base);
            }
        }
        Some(Arc::new(client))
    }

    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Send a Messages request and return the concatenated text of all `text`
    /// content blocks in the assistant reply.
    ///
    /// Non-text blocks (e.g. `tool_use`) are skipped — this client deliberately
    /// only supports plain text generation for P2.
    pub async fn complete(
        &self,
        system: &str,
        messages: &[ChatMsg],
        max_tokens: u32,
    ) -> Result<String> {
        if messages.is_empty() {
            return Err(AiError::Invalid("messages must not be empty".into()));
        }

        let body = RequestBody {
            model: &self.model,
            max_tokens,
            system,
            messages,
        };

        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        let raw = resp.text().await?;

        if !status.is_success() {
            return Err(AiError::Anthropic {
                status: status.as_u16(),
                message: truncate(&raw, 1024),
            });
        }

        let parsed: ResponseBody = serde_json::from_str(&raw)?;
        Ok(parsed
            .content
            .into_iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text),
                ContentBlock::Other => None,
            })
            .collect::<String>())
    }
}

// ---------- wire types ----------

#[derive(Serialize)]
struct RequestBody<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a str,
    messages: &'a [ChatMsg],
}

#[derive(Deserialize)]
struct ResponseBody {
    #[serde(default)]
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text { text: String },
    #[serde(other)]
    Other,
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_body_shape_matches_anthropic_contract() {
        let msgs = vec![
            ChatMsg::user("hello"),
            ChatMsg::assistant("hi"),
            ChatMsg::user("how are you?"),
        ];
        let body = RequestBody { model: "claude-sonnet-4-6", max_tokens: 256, system: "be terse", messages: &msgs };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["model"], "claude-sonnet-4-6");
        assert_eq!(json["max_tokens"], 256);
        assert_eq!(json["system"], "be terse");
        let arr = json["messages"].as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[0]["content"], "hello");
        assert_eq!(arr[1]["role"], "assistant");
        assert_eq!(arr[2]["content"], "how are you?");
    }

    #[test]
    fn chat_msg_roles() {
        assert_eq!(ChatMsg::user("x").role, "user");
        assert_eq!(ChatMsg::assistant("x").role, "assistant");
    }

    #[test]
    fn response_parses_concatenated_text_blocks() {
        let raw = r#"{
            "content": [
                {"type":"text","text":"Hello "},
                {"type":"tool_use","id":"t1","name":"x","input":{}},
                {"type":"text","text":"world"}
            ]
        }"#;
        let parsed: ResponseBody = serde_json::from_str(raw).unwrap();
        let joined: String = parsed
            .content
            .into_iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text),
                ContentBlock::Other => None,
            })
            .collect::<String>();
        assert_eq!(joined, "Hello world");
    }

    #[test]
    fn truncate_at_char_boundary() {
        let s = "中文测试";
        let t = truncate(s, 4);
        assert!(t.ends_with("..."));
        // Should not panic on non-ASCII boundary
        assert!(t.is_char_boundary(t.len()));
    }
}
