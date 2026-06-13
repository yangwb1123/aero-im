//! Thin client for the Anthropic Messages API.
//!
//! Two call modes:
//! - [`AnthropicClient::complete`] — batch: waits for the full reply, returns `String`.
//! - [`AnthropicClient::complete_stream`] — streaming: sets `"stream":true`, parses
//!   the SSE response, and yields text-delta chunks as they arrive. Callers get
//!   lower time-to-first-token without changing the billing model.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::stream::{Stream, StreamExt as _};
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

/// Real token usage parsed from an Anthropic Messages API response.
///
/// The response JSON carries a top-level `usage` object — these are the actual
/// billed token counts (not an estimate), used to compute real per-job cost (方向三
/// AI cost realism). Cache-related token fields the API also returns
/// (`cache_creation_input_tokens` / `cache_read_input_tokens`) are ignored here:
/// this client does not use prompt caching, so they are always zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens consumed by the prompt (system + messages).
    #[serde(default)]
    pub input_tokens: u32,
    /// Tokens generated in the assistant reply.
    #[serde(default)]
    pub output_tokens: u32,
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
    ///
    /// This is a thin wrapper over [`Self::complete_with_usage`] that discards the
    /// usage — preserved for callers (CLI, [`crate::service::AiService`] helpers)
    /// that only need the text.
    pub async fn complete(
        &self,
        system: &str,
        messages: &[ChatMsg],
        max_tokens: u32,
    ) -> Result<String> {
        self.complete_with_usage(system, messages, max_tokens).await.map(|(text, _usage)| text)
    }

    /// Like [`Self::complete`] but also returns the real token [`Usage`] parsed
    /// from the response's top-level `usage` object.
    ///
    /// Callers that record cost (the worker) use this; callers that only need the
    /// text use [`Self::complete`]. Keeping both avoids churning every call site
    /// while still surfacing real token counts where they matter (方向三).
    pub async fn complete_with_usage(
        &self,
        system: &str,
        messages: &[ChatMsg],
        max_tokens: u32,
    ) -> Result<(String, Usage)> {
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
        let usage = parsed.usage.unwrap_or_default();
        let text = parsed
            .content
            .into_iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text),
                ContentBlock::ToolUse { .. } | ContentBlock::Other => None,
            })
            .collect::<String>();
        Ok((text, usage))
    }

    /// One turn of a tool-use loop: send the conversation + advertised `tools` and
    /// return the [`AgentTurn`] (any text + the `tool_use` blocks the model wants
    /// run). The agent driver ([`crate::agent::run_agent_loop`]) executes the tools
    /// and calls this again with the results appended.
    ///
    /// `messages` are raw Anthropic message objects (`{"role","content"}`) so the
    /// driver can append assistant `tool_use` turns and user `tool_result` turns
    /// without a rigid typed model; the driver owns that construction.
    ///
    /// # Errors
    /// `AiError::Invalid` for empty `messages`; `AiError::Http`/`AiError::Anthropic`
    /// on transport / non-2xx.
    pub async fn complete_with_tools(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        tools: &[ToolDef],
        max_tokens: u32,
    ) -> Result<AgentTurn> {
        if messages.is_empty() {
            return Err(AiError::Invalid("messages must not be empty".into()));
        }
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": max_tokens,
            "system": system,
            "messages": messages,
            "tools": tools,
        });
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
            return Err(AiError::Anthropic { status: status.as_u16(), message: truncate(&raw, 1024) });
        }
        parse_agent_turn(&raw)
    }

    /// Stream the assistant's reply as text-delta chunks.
    ///
    /// Sets `"stream": true`; the Anthropic API responds with Server-Sent Events.
    /// Only `content_block_delta` / `text_delta` events are yielded — all other
    /// event types (`ping`, `message_start`, etc.) are silently dropped.
    ///
    /// # Errors
    /// Returns `AiError::Invalid` if `messages` is empty, `AiError::Http` on
    /// network errors, or `AiError::Anthropic` for non-2xx HTTP status.
    pub async fn complete_stream(
        &self,
        system: &str,
        messages: &[ChatMsg],
        max_tokens: u32,
    ) -> Result<impl Stream<Item = Result<String>> + Send + 'static> {
        if messages.is_empty() {
            return Err(AiError::Invalid("messages must not be empty".into()));
        }

        let body = StreamRequestBody {
            model: self.model.clone(),
            max_tokens,
            system: system.to_string(),
            messages: messages.to_vec(),
            stream: true,
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
        if !status.is_success() {
            let raw = resp.text().await?;
            return Err(AiError::Anthropic {
                status: status.as_u16(),
                message: truncate(&raw, 1024),
            });
        }

        let byte_stream: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>> =
            Box::pin(resp.bytes_stream());

        let stream = futures::stream::unfold(
            (byte_stream, SseParser::default()),
            |(mut bs, mut parser)| async move {
                loop {
                    if let Some(text) = parser.next_chunk() {
                        return Some((Ok(text), (bs, parser)));
                    }
                    match bs.next().await {
                        None => return None,
                        Some(Err(e)) => {
                            return Some((Err(AiError::Http(e.to_string())), (bs, parser)));
                        }
                        Some(Ok(bytes)) => {
                            parser.push_bytes(&bytes);
                        }
                    }
                }
            },
        );

        Ok(stream)
    }
}

// ---------- tool-use (agentic loop) types ----------

/// A tool advertised to the model in the `tools` array. `input_schema` is a JSON
/// Schema object describing the tool's parameters (the model fills it in).
#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// A `tool_use` block the model emitted: run `name` with `input`, then reply with
/// a `tool_result` echoing `id` so the API can correlate the result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolUse {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

/// One assistant turn in a tool-use loop: any text it produced, the tool calls it
/// wants executed (empty ⇒ it's done answering), and the real token [`Usage`].
#[derive(Debug, Clone)]
pub struct AgentTurn {
    pub text: String,
    pub tool_uses: Vec<ToolUse>,
    pub usage: Usage,
}

impl AgentTurn {
    /// True when the model requested no tools — the loop stops and uses `text`.
    #[must_use]
    pub fn is_final(&self) -> bool {
        self.tool_uses.is_empty()
    }
}

/// Parse a non-streaming Messages response into an [`AgentTurn`], splitting the
/// content blocks into concatenated text + the ordered `tool_use` calls. Pure, so
/// the tool-loop parsing is unit-tested against canned JSON without any HTTP.
fn parse_agent_turn(raw: &str) -> Result<AgentTurn> {
    let parsed: ResponseBody = serde_json::from_str(raw)?;
    let usage = parsed.usage.unwrap_or_default();
    let mut text = String::new();
    let mut tool_uses = Vec::new();
    for block in parsed.content {
        match block {
            ContentBlock::Text { text: t } => text.push_str(&t),
            ContentBlock::ToolUse { id, name, input } => tool_uses.push(ToolUse { id, name, input }),
            ContentBlock::Other => {}
        }
    }
    Ok(AgentTurn { text, tool_uses, usage })
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
    /// Top-level `usage` object — present on every successful non-streaming
    /// Messages response. `Option` so a malformed/older response without it
    /// degrades to zero usage rather than failing the whole job.
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text { text: String },
    /// A tool the model wants run — surfaced by [`AnthropicClient::complete_with_tools`]
    /// for the agentic loop; ignored by the plain-text [`AnthropicClient::complete`] path.
    ToolUse { id: String, name: String, input: serde_json::Value },
    #[serde(other)]
    Other,
}

/// Streaming request body — identical to [`RequestBody`] plus `"stream": true`.
#[derive(Serialize)]
struct StreamRequestBody {
    model: String,
    max_tokens: u32,
    system: String,
    messages: Vec<ChatMsg>,
    stream: bool,
}

/// Incremental SSE parser for the Anthropic streaming response.
///
/// Anthropic frames each event as two header lines plus a blank separator:
/// ```text
/// event: content_block_delta
/// data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"..."}}
///
/// ```
/// We buffer raw bytes, split on `\n\n`, and extract text only from
/// `content_block_delta` / `text_delta` pairs. Other event types are dropped.
#[derive(Default)]
struct SseParser {
    buffer: String,
    pending: VecDeque<String>,
}

impl SseParser {
    fn push_bytes(&mut self, bytes: &Bytes) {
        if let Ok(s) = std::str::from_utf8(bytes) {
            self.buffer.push_str(s);
            self.drain_events();
        }
    }

    fn drain_events(&mut self) {
        while let Some(pos) = self.buffer.find("\n\n") {
            let block = self.buffer[..pos].to_string();
            self.buffer.drain(..pos + 2);
            if let Some(text) = extract_text_delta(&block) {
                self.pending.push_back(text);
            }
        }
    }

    fn next_chunk(&mut self) -> Option<String> {
        self.pending.pop_front()
    }
}

/// Extract a text fragment from a single SSE event block.
///
/// Returns `Some(text)` only for `event: content_block_delta` blocks where
/// `delta.type == "text_delta"`. All other events return `None`.
fn extract_text_delta(block: &str) -> Option<String> {
    let mut event_type: Option<&str> = None;
    let mut data_json: Option<&str> = None;

    for line in block.lines() {
        if let Some(v) = line.strip_prefix("event: ") {
            event_type = Some(v.trim());
        } else if let Some(v) = line.strip_prefix("data: ") {
            data_json = Some(v.trim());
        }
    }

    if event_type? != "content_block_delta" {
        return None;
    }
    let json: serde_json::Value = serde_json::from_str(data_json?).ok()?;
    let delta = json.get("delta")?;
    if delta.get("type")?.as_str()? != "text_delta" {
        return None;
    }
    Some(delta.get("text")?.as_str()?.to_string())
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
                ContentBlock::ToolUse { .. } | ContentBlock::Other => None,
            })
            .collect::<String>();
        assert_eq!(joined, "Hello world");
    }

    #[test]
    fn parse_agent_turn_splits_text_and_tool_calls() {
        // A real tool-use response: interleaved text + two tool_use blocks.
        let raw = r#"{
            "content": [
                {"type":"text","text":"let me check"},
                {"type":"tool_use","id":"tu_1","name":"search","input":{"q":"deploys"}},
                {"type":"tool_use","id":"tu_2","name":"clock","input":{}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 7}
        }"#;
        let turn = parse_agent_turn(raw).unwrap();
        assert_eq!(turn.text, "let me check");
        assert!(!turn.is_final(), "tool calls present ⇒ not final");
        assert_eq!(turn.tool_uses.len(), 2);
        assert_eq!(turn.tool_uses[0].id, "tu_1");
        assert_eq!(turn.tool_uses[0].name, "search");
        assert_eq!(turn.tool_uses[0].input["q"], "deploys");
        assert_eq!(turn.tool_uses[1].name, "clock");
        assert_eq!(turn.usage.input_tokens, 10);
        assert_eq!(turn.usage.output_tokens, 7);
    }

    #[test]
    fn parse_agent_turn_text_only_is_final() {
        let raw = r#"{"content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}"#;
        let turn = parse_agent_turn(raw).unwrap();
        assert!(turn.is_final());
        assert_eq!(turn.text, "done");
        assert!(turn.tool_uses.is_empty());
    }

    #[test]
    fn tool_def_serializes_to_anthropic_shape() {
        let def = ToolDef {
            name: "search_messages".into(),
            description: "Search the room".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            }),
        };
        let json = serde_json::to_value(&def).unwrap();
        assert_eq!(json["name"], "search_messages");
        assert_eq!(json["description"], "Search the room");
        assert_eq!(json["input_schema"]["type"], "object");
        assert_eq!(json["input_schema"]["required"][0], "query");
    }

    #[test]
    fn response_parses_usage_from_anthropic_shape() {
        // Mirrors a real Anthropic Messages API response: a top-level `usage`
        // object alongside `content`, including the cache fields we ignore.
        let raw = r#"{
            "id": "msg_01XYZ",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-6",
            "content": [{"type":"text","text":"42"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 1234,
                "output_tokens": 56,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0
            }
        }"#;
        let parsed: ResponseBody = serde_json::from_str(raw).unwrap();
        let usage = parsed.usage.expect("usage must be present");
        assert_eq!(usage.input_tokens, 1234);
        assert_eq!(usage.output_tokens, 56);
    }

    #[test]
    fn usage_deserializes_standalone_object() {
        let u: Usage = serde_json::from_str(r#"{"input_tokens":10,"output_tokens":20}"#).unwrap();
        assert_eq!(u, Usage { input_tokens: 10, output_tokens: 20 });
    }

    #[test]
    fn response_without_usage_defaults_to_none() {
        // An older/malformed response missing `usage` must not fail to parse —
        // it degrades to None (callers treat that as zero usage).
        let raw = r#"{"content":[{"type":"text","text":"hi"}]}"#;
        let parsed: ResponseBody = serde_json::from_str(raw).unwrap();
        assert!(parsed.usage.is_none());
    }

    #[test]
    fn usage_fields_default_to_zero_when_partial() {
        // Defensive: a usage object missing one field defaults it to zero rather
        // than failing (the `#[serde(default)]` on each field).
        let u: Usage = serde_json::from_str(r#"{"input_tokens":7}"#).unwrap();
        assert_eq!(u, Usage { input_tokens: 7, output_tokens: 0 });
    }

    #[test]
    fn truncate_at_char_boundary() {
        let s = "中文测试";
        let t = truncate(s, 4);
        assert!(t.ends_with("..."));
        // Should not panic on non-ASCII boundary
        assert!(t.is_char_boundary(t.len()));
    }

    #[test]
    fn sse_parser_extracts_text_delta() {
        let block = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}";
        assert_eq!(extract_text_delta(block), Some("Hello".into()));
    }

    #[test]
    fn sse_parser_ignores_non_delta_events() {
        let ping = "event: ping\ndata: {}";
        assert_eq!(extract_text_delta(ping), None);

        let start = "event: message_start\ndata: {\"type\":\"message_start\"}";
        assert_eq!(extract_text_delta(start), None);
    }

    #[test]
    fn sse_parser_ignores_non_text_delta_type() {
        let block = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\"}}";
        assert_eq!(extract_text_delta(block), None);
    }

    #[test]
    fn sse_parser_buffers_across_chunks() {
        let mut p = SseParser::default();
        // Split an event across two byte deliveries.
        let part1 = b"event: content_block_delta\ndata: {\"type\":\"content_block_delt";
        let part2 = b"a\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n";
        p.push_bytes(&Bytes::from(part1.as_slice()));
        assert_eq!(p.next_chunk(), None, "event not yet complete");
        p.push_bytes(&Bytes::from(part2.as_slice()));
        assert_eq!(p.next_chunk(), Some("Hi".into()));
        assert_eq!(p.next_chunk(), None);
    }

    #[test]
    fn sse_parser_queues_multiple_chunks() {
        let mut p = SseParser::default();
        let raw = concat!(
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"A\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"B\"}}\n\n",
        );
        p.push_bytes(&Bytes::from(raw.as_bytes()));
        assert_eq!(p.next_chunk(), Some("A".into()));
        assert_eq!(p.next_chunk(), Some("B".into()));
        assert_eq!(p.next_chunk(), None);
    }
}
