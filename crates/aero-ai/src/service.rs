//! High-level AI service composed from an Anthropic client + an embedder + storage repos.
//!
//! This is the surface the HTTP layer and the worker call. Two real features:
//! - [`AiService::summarize_room`] — pull recent messages, ask Claude for a
//!   Chinese bullet summary (the project's UI is Chinese). Falls back to a
//!   "last 5 lines" heuristic when Anthropic is disabled.
//! - [`AiService::answer_question`] — embed the question, vector-search the
//!   room, hand the top-k hits to Claude as context, return answer + citation
//!   message IDs. Without Anthropic, returns concatenated context as the answer.
//!
//! Everything is plumbed through `Arc` so the same instance can be shared by
//! the Axum router and the background worker without contention.

use std::sync::Arc;

use aero_common::{Message, MessageId, RoomId};
use aero_storage::{AiJobRepo, MessageRepo, RoomRepo, SearchHit};

use crate::anthropic::{AnthropicClient, ChatMsg};
use crate::embed::{default_embedder, Embedder};
use crate::error::{AiError, Result};
use crate::transcribe::{default_transcriber, Transcriber};

/// Result of an `answer_question` call.
///
/// `answer` is human-readable text suitable for direct display. `citations`
/// references the underlying messages so the UI can render "source" chips
/// linking back into the conversation.
#[derive(Debug, Clone)]
pub struct AnswerResult {
    pub answer: String,
    pub citations: Vec<MessageId>,
}

/// Composition root for AI features.
#[derive(Clone)]
pub struct AiService {
    anthropic: Option<Arc<AnthropicClient>>,
    embedder: Arc<dyn Embedder + Send + Sync>,
    transcriber: Arc<dyn Transcriber>,
    ai_jobs: AiJobRepo,
    messages: MessageRepo,
    rooms: RoomRepo,
}

impl AiService {
    pub fn new(
        anthropic: Option<Arc<AnthropicClient>>,
        embedder: Arc<dyn Embedder + Send + Sync>,
        transcriber: Arc<dyn Transcriber>,
        ai_jobs: AiJobRepo,
        messages: MessageRepo,
        rooms: RoomRepo,
    ) -> Self {
        Self { anthropic, embedder, transcriber, ai_jobs, messages, rooms }
    }

    /// Construct from env: Anthropic optional, embedder picks Voyage if configured
    /// else local hash. Repos are required since they're owned by the server.
    pub fn from_env(ai_jobs: AiJobRepo, messages: MessageRepo, rooms: RoomRepo) -> Self {
        Self::new(
            AnthropicClient::from_env(),
            default_embedder(),
            default_transcriber(),
            ai_jobs,
            messages,
            rooms,
        )
    }

    /// Transcribe an audio blob via the configured transcriber.
    pub async fn transcribe(&self, bytes: bytes::Bytes, mime: &str) -> Result<String> {
        self.transcriber.transcribe(bytes, mime).await
    }

    // ---------- accessors (for the worker) ----------

    #[must_use]
    pub fn ai_jobs(&self) -> &AiJobRepo {
        &self.ai_jobs
    }

    #[must_use]
    pub fn messages(&self) -> &MessageRepo {
        &self.messages
    }

    #[must_use]
    pub fn rooms(&self) -> &RoomRepo {
        &self.rooms
    }

    #[must_use]
    pub fn embedder(&self) -> &Arc<dyn Embedder + Send + Sync> {
        &self.embedder
    }

    #[must_use]
    pub fn anthropic(&self) -> Option<&Arc<AnthropicClient>> {
        self.anthropic.as_ref()
    }

    /// True when an Anthropic API key was configured at startup.
    #[must_use]
    pub fn has_anthropic(&self) -> bool {
        self.anthropic.is_some()
    }

    // ---------- embeddings ----------

    /// Embed an arbitrary text via the configured embedder.
    pub async fn embed_text(&self, text: &str) -> Result<Vec<f32>> {
        self.embedder.embed_one(text).await
    }

    // ---------- summarize ----------

    /// Summarize the last `last_n` messages of a room.
    ///
    /// Uses Anthropic when configured; otherwise returns a deterministic
    /// "last 5 lines" fallback so the feature degrades gracefully in dev.
    pub async fn summarize_room(&self, room: RoomId, last_n: usize) -> Result<String> {
        // `last_n` is clamped to [1, 200] so this `as i64` is always safe.
        #[allow(clippy::cast_possible_wrap)]
        let limit = last_n.clamp(1, 200) as i64;
        let mut recent = self.messages.list_recent(room, None, limit).await?;
        // `list_recent` returns newest-first; reverse to chronological for the LLM.
        recent.reverse();

        if recent.is_empty() {
            return Ok(String::new());
        }

        let transcript = render_transcript(&recent);

        if let Some(client) = &self.anthropic {
            let system = SUMMARIZE_SYSTEM_PROMPT;
            let user = format!(
                "请阅读以下聊天记录,并按照系统指令给出要点摘要。\n\n聊天记录:\n{transcript}"
            );
            let msgs = vec![ChatMsg::user(user)];
            return client.complete(system, &msgs, 600).await;
        }

        Ok(heuristic_summary(&recent))
    }

    // ---------- question answering ----------

    /// Answer a question grounded in a room's history.
    ///
    /// 1. Embed the question via the configured embedder.
    /// 2. Top-k vector search inside the room.
    /// 3. Ask Anthropic to answer using only the retrieved context; cite the
    ///    message IDs of the hits we passed in.
    ///
    /// Without Anthropic, returns the joined context block + a note so the
    /// caller can still see *what* would have been used as grounding.
    pub async fn answer_question(
        &self,
        room: RoomId,
        question: &str,
        k: usize,
    ) -> Result<AnswerResult> {
        let q = question.trim();
        if q.is_empty() {
            return Err(AiError::Invalid("question must not be empty".into()));
        }
        // `k` is clamped to [1, 20] so this `as i64` is always safe.
        #[allow(clippy::cast_possible_wrap)]
        let k = k.clamp(1, 20) as i64;

        let query_vec = self.embedder.embed_one(q).await?;
        let hits: Vec<SearchHit> = self.messages.search_vector(room, query_vec, k).await?;

        let citations: Vec<MessageId> = hits.iter().map(|h| h.message.id).collect();
        let context = render_context(&hits);

        if let Some(client) = &self.anthropic {
            let system = ANSWER_SYSTEM_PROMPT;
            let user = format!(
                "问题: {q}\n\n相关聊天上下文(每段已附 ID,引用时使用):\n{context}\n\n请基于上述上下文作答,若信息不足请说明。"
            );
            let answer = client.complete(system, &[ChatMsg::user(user)], 800).await?;
            return Ok(AnswerResult { answer, citations });
        }

        // No Anthropic — return the raw context so the UI can still surface
        // grounded results. This is a dev-mode escape hatch, not a quality
        // claim.
        let fallback = if hits.is_empty() {
            "（未配置 Anthropic,也未在房间内检索到相关消息。）".to_string()
        } else {
            format!("（未配置 Anthropic,以下为检索到的相关消息上下文:）\n\n{context}")
        };
        Ok(AnswerResult { answer: fallback, citations })
    }

    // ---------- translation (P3 实时字幕翻译) ----------

    /// Translate `text` into `target_lang` (a human label or BCP-47 code).
    ///
    /// Uses Anthropic when configured; otherwise returns the source text
    /// unchanged so live captions still display (just untranslated). Tuned for
    /// short, low-latency caption lines.
    pub async fn translate(&self, text: &str, target_lang: &str) -> Result<String> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(String::new());
        }
        if let Some(client) = &self.anthropic {
            let system = TRANSLATE_SYSTEM_PROMPT;
            let user = format!(
                "目标语言: {target_lang}\n只输出译文本身,不要解释、不要引号。\n\n原文:\n{text}"
            );
            return client.complete(system, &[ChatMsg::user(user)], 400).await;
        }
        Ok(text.to_owned())
    }
}

// ---------- helpers ----------

const SUMMARIZE_SYSTEM_PROMPT: &str = "\
你是一个对话摘要助手,服务于即时通讯系统。请用中文以无序列表(每条 1-2 行)输出 3-5 条要点摘要,\
按以下结构组织:\n\
- 要点:对话的关键话题或事实\n\
- 决定:已经达成的共识或决策(若无可省略)\n\
- 行动项:谁在何时之前需要做什么(若无可省略)\n\
\n\
约束:不要复述原文,不要超过 200 字,不要使用 Markdown 标题,只输出无序列表项(以 `-` 开头)。";

const TRANSLATE_SYSTEM_PROMPT: &str = "\
你是一个实时字幕翻译引擎。把用户提供的口语化文本翻译成目标语言,保持简洁口语风格。\
只输出译文本身,不要添加任何解释、注释、标点修饰或引号。";

const ANSWER_SYSTEM_PROMPT: &str = "\
你是一个基于检索增强生成(RAG)的问答助手。请严格基于提供的聊天上下文回答用户问题,\
用中文回答。回答规则:\n\
- 若上下文足以回答,给出简洁明确的答案。\n\
- 若上下文不足,直说\"上下文不足以回答\"并说明缺什么。\n\
- 不要编造未在上下文中出现的事实。\n\
- 引用证据时使用上下文中给出的消息 ID。";

/// Render messages as a plain chronological transcript for the LLM.
///
/// Format: `[ts] sender_id: text`. Empty messages (no searchable content)
/// are skipped to keep the context window tight.
fn render_transcript(messages: &[Message]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for m in messages {
        let text = m.searchable_text();
        if text.is_empty() {
            continue;
        }
        // `write!` to a String is infallible; the `Result` it returns is the
        // error of the inner formatter (always Ok for String).
        let _ = writeln!(out, "[{}] {}: {}", m.created_at, m.sender_id, text);
    }
    out
}

/// Render search hits as a numbered context block with explicit IDs the LLM
/// can cite. Keeps the score so the model can prefer higher-confidence rows.
fn render_context(hits: &[SearchHit]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for (i, h) in hits.iter().enumerate() {
        let text = h.message.searchable_text();
        if text.is_empty() {
            continue;
        }
        let _ = writeln!(
            out,
            "[{}] id={} score={:.3}\n{}\n",
            i + 1,
            h.message.id,
            h.score,
            text
        );
    }
    out
}

/// Last-5-lines fallback summary used when Anthropic is disabled.
fn heuristic_summary(messages: &[Message]) -> String {
    let tail: Vec<&Message> = messages.iter().rev().take(5).collect();
    let mut lines = Vec::with_capacity(tail.len());
    for m in tail.iter().rev() {
        let text = m.searchable_text();
        if text.is_empty() {
            continue;
        }
        // Single-line, trimmed; bullet-style to mirror the LLM output shape.
        let one_line = text.replace('\n', " ");
        lines.push(format!("- {}", truncate_for_summary(&one_line, 120)));
    }
    if lines.is_empty() {
        return String::new();
    }
    lines.join("\n")
}

fn truncate_for_summary(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_common::{Block, ParticipantId};
    use time::OffsetDateTime;

    fn mk_msg(text: &str) -> Message {
        Message {
            id: MessageId::new(),
            room_id: RoomId::new(),
            sender_id: ParticipantId::new(),
            blocks: vec![Block::text(text)],
            reply_to: None,
            metadata: serde_json::Value::Null,
            created_at: OffsetDateTime::now_utc(),
            edited_at: None,
            deleted_at: None,
        }
    }

    #[test]
    fn heuristic_summary_uses_last_five() {
        let mut msgs = Vec::new();
        for i in 0..7 {
            msgs.push(mk_msg(&format!("line {i}")));
        }
        let s = heuristic_summary(&msgs);
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 5);
        assert!(lines[0].contains("line 2"));
        assert!(lines[4].contains("line 6"));
        for l in &lines {
            assert!(l.starts_with("- "), "bullet form, got {l}");
        }
    }

    #[test]
    fn heuristic_summary_empty_when_no_messages() {
        assert_eq!(heuristic_summary(&[]), "");
    }

    #[test]
    fn render_transcript_skips_empty_messages() {
        let mut empty = mk_msg("");
        empty.blocks = Vec::new();
        let msgs = vec![empty, mk_msg("hello")];
        let t = render_transcript(&msgs);
        assert_eq!(t.lines().count(), 1);
        assert!(t.contains("hello"));
    }

    #[test]
    fn truncate_for_summary_keeps_under_limit() {
        let s = "a".repeat(50);
        assert_eq!(truncate_for_summary(&s, 120), s);
        let long = "汉".repeat(200);
        let t = truncate_for_summary(&long, 120);
        assert!(t.ends_with("…"));
        assert_eq!(t.chars().count(), 121);
    }
}
