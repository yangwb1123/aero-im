use std::sync::{Arc, Mutex};
use aero_common::{Message, MessageId, ParticipantId, RoomId, WorkspaceId};
use aero_storage::{AiContextStore, AiJobRepo, MessageRepo, RoomRepo, SearchHit};
use futures::stream::Stream;
use std::pin::Pin;
use crate::anthropic::{AnthropicClient, ChatMsg, Usage};
use crate::embed::{default_embedder, Embedder};
use crate::error::{AiError, Result};
use crate::transcribe::{default_transcriber, Transcriber};
use serde_json::Value;

use super::{AiService, AnswerResult, ChannelRec, Expert, PersonRec, Sentiment, SentimentScore};
use super::tools::*;

impl AiService {
    pub fn new(
        anthropic: Option<Arc<AnthropicClient>>,
        embedder: Arc<dyn Embedder + Send + Sync>,
        transcriber: Arc<dyn Transcriber>,
        ai_jobs: AiJobRepo,
        messages: MessageRepo,
        rooms: RoomRepo,
        context: Option<AiContextStore>,
    ) -> Self {
        Self {
            anthropic,
            embedder,
            transcriber,
            ai_jobs,
            messages,
            rooms,
            context,
            blob_store: None,
            ai_profiles: None,
        }
    }

    /// Attach a blob store so the agentic answer loop can read text attachments
    /// (方向三). Builder-style; composes with [`Self::new`] / [`Self::from_env`].
    #[must_use]
    pub fn with_blob_store(mut self, blob_store: Arc<dyn aero_storage::BlobStore>) -> Self {
        self.blob_store = Some(blob_store);
        self
    }

    /// Attach the persistent cross-room AI profile store (持久跨房 AI 用户画像).
    ///
    /// Builder-style; composes with [`Self::new`] / [`Self::from_env`]. Wiring it
    /// does NOT enable the feature — extraction and personalised use additionally
    /// require the opt-in `AERO_AI_CROSS_ROOM_PROFILE` env flag (default OFF), so
    /// a server can hand over the repo unconditionally and the feature stays dark
    /// until an operator turns it on.
    #[must_use]
    pub fn with_ai_profiles(mut self, ai_profiles: aero_storage::AiProfileRepo) -> Self {
        self.ai_profiles = Some(ai_profiles);
        self
    }

    /// Construct from env: Anthropic optional, embedder picks Voyage if configured
    /// else local hash. Repos are required since they're owned by the server.
    pub fn from_env(
        ai_jobs: AiJobRepo,
        messages: MessageRepo,
        rooms: RoomRepo,
        context: Option<AiContextStore>,
    ) -> Self {
        Self::new(
            AnthropicClient::from_env(),
            default_embedder(),
            default_transcriber(),
            ai_jobs,
            messages,
            rooms,
            context,
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

    /// Best-effort: extract the *body text* of the first extractable document
    /// attached to `msg` (方向三-2 file content search). Used by the indexing
    /// worker to fold a `report.pdf` / `notes.docx`'s prose into the message's
    /// `searchable_text` + embedding, so the document's content — not just its
    /// file name — is searchable.
    ///
    /// Reuses the same blob read + [`extract_text`] path as the agentic
    /// `read_attachment` tool (no new extractor): it picks the first
    /// [`Block::File`] within the [`MAX_ATTACHMENT_BYTES`] size cap, fetches its
    /// bytes, and runs document/plain-text extraction.
    ///
    /// **Fail-open by contract**: returns `None` — never `Err` — for every
    /// non-fatal condition (no blob store wired, no file block, over-cap,
    /// blob-read failure, or unextractable/binary bytes). Callers treat `None` as
    /// "nothing extra to index" and proceed with the message's own text, so a bad
    /// attachment can never crash or fail an indexing job.
    pub async fn extract_attachment_text(&self, msg: &Message) -> Option<String> {
        let store = self.blob_store.as_ref()?;
        let (blob_id, _name, size) = first_extractable_file(&msg.blocks)?;
        if size > MAX_ATTACHMENT_BYTES as u64 {
            return None;
        }
        let bytes = match store.get(blob_id).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, %blob_id, "extract_attachment_text: blob read failed (fail-open)");
                return None;
            }
        };
        extract_text(&bytes, MAX_ATTACHMENT_BYTES)
    }

    // ---------- summarize ----------

    /// Summarize the last `last_n` messages of a room.
    ///
    /// Uses Anthropic when configured; otherwise returns a deterministic
    /// "last 5 lines" fallback so the feature degrades gracefully in dev.
    pub async fn summarize_room(&self, room: RoomId, last_n: usize) -> Result<String> {
        self.summarize_room_with_usage(room, last_n).await.map(|(summary, _usage)| summary)
    }

    /// Like [`Self::summarize_room`] but also returns the real Anthropic token
    /// [`Usage`] when a completion was made.
    ///
    /// `usage` is `None` on the no-Anthropic heuristic path (or an empty room) so
    /// the worker can fall back to the flat cost estimate; it is `Some` whenever a
    /// paid completion produced the summary, letting the worker record REAL cost
    /// (方向三).
    pub async fn summarize_room_with_usage(
        &self,
        room: RoomId,
        last_n: usize,
    ) -> Result<(String, Option<Usage>)> {
        // `last_n` is clamped to [1, 200] so this `as i64` is always safe.
        #[allow(clippy::cast_possible_wrap)]
        let limit = last_n.clamp(1, 200) as i64;
        let mut recent = self.messages.list_recent(room, None, limit).await?;
        // `list_recent` returns newest-first; reverse to chronological for the LLM.
        recent.reverse();

        if recent.is_empty() {
            return Ok((String::new(), None));
        }

        let transcript = render_transcript(&recent);

        if let Some(client) = &self.anthropic {
            let system = SUMMARIZE_SYSTEM_PROMPT;
            let user = format!(
                "请阅读以下聊天记录,并按照系统指令给出要点摘要。\n\n聊天记录:\n{transcript}"
            );
            let msgs = vec![ChatMsg::user(user)];
            let (summary, usage) = client.complete_with_usage(system, &msgs, 600).await?;
            return Ok((summary, Some(usage)));
        }

        Ok((heuristic_summary(&recent), None))
    }

    /// Summarize an arbitrary block of text into a short recap + action items.
    ///
    /// Mirrors [`Self::summarize_room`] but summarizes the GIVEN text rather than
    /// a room's message history — used for the post-call meeting recap, where the
    /// caller joins a call's persisted transcript lines into one block. With an
    /// Anthropic key it prompts the `complete` primitive for a concise recap with
    /// action items; without a key it falls back to the SAME heuristic style
    /// [`Self::summarize_room`] uses (a truncated first-lines digest). Never errors
    /// on a missing key. Empty/blank input returns an empty string.
    ///
    /// # Errors
    /// Propagates an Anthropic failure when a key IS configured; the no-key path
    /// is infallible.
    pub async fn summarize_text(&self, text: &str) -> Result<String> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(String::new());
        }

        if let Some(client) = &self.anthropic {
            let system = RECAP_SYSTEM_PROMPT;
            let user = format!(
                "请阅读以下通话/会议记录,并按照系统指令给出简短复盘与行动项。\n\n记录:\n{text}"
            );
            let msgs = vec![ChatMsg::user(user)];
            return client.complete(system, &msgs, 600).await;
        }

        Ok(heuristic_text_digest(text))
    }

    // ---------- retrieval (RAG rerank, 方向三) ----------

    /// Retrieve the top-`k` grounding hits for a room-scoped question: a wide
    /// vector candidate pool fused (Reciprocal Rank Fusion) with FTS candidates
    /// over the same room boundary, so lexical agreement can promote a hit past
    /// semantically-near-but-wrong neighbours. An FTS failure degrades to pure
    /// vector order — it warns but never fails the ask.
    pub(crate) async fn retrieve_room(&self, room: RoomId, q: &str, k: usize) -> Result<Vec<SearchHit>> {
        let query_vec = self.embedder.embed_query(q).await?;
        let vector = self.messages.search_vector(room, query_vec, RETRIEVAL_POOL).await?;
        let fts = match self.messages.fts_candidates(room, q, RETRIEVAL_POOL).await {
            Ok(hits) => hits,
            Err(e) => {
                tracing::warn!(error = %e, "rag rerank: room FTS candidates failed; using vector order");
                Vec::new()
            }
        };
        Ok(crate::rerank::fuse_rankings(&vector, &fts, k))
    }

    /// Workspace-scoped twin of [`Self::retrieve_room`]: both retrievers run
    /// over the identical `JOIN room_members` / `rooms.workspace_id` boundary,
    /// so fusion can never widen what either retriever was allowed to see.
    pub(crate) async fn retrieve_workspace(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        q: &str,
        k: usize,
    ) -> Result<Vec<SearchHit>> {
        let query_vec = self.embedder.embed_query(q).await?;
        let vector = self
            .messages
            .search_vector_workspace(participant, workspace, query_vec, RETRIEVAL_POOL)
            .await?;
        let fts = match self
            .messages
            .fts_candidates_workspace(participant, workspace, q, RETRIEVAL_POOL)
            .await
        {
            Ok(hits) => hits,
            Err(e) => {
                tracing::warn!(error = %e, "rag rerank: workspace FTS candidates failed; using vector order");
                Vec::new()
            }
        };
        Ok(crate::rerank::fuse_rankings(&vector, &fts, k))
    }

    // ---------- question answering ----------

    /// Answer a question grounded in a room's history.
    ///
    /// 1. Embed the question via the configured embedder.
    /// 2. Retrieve a wide vector candidate pool inside the room, RRF-fuse it
    ///    with FTS candidates ([`crate::rerank::fuse_rankings`]), keep top-k.
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
        self.answer_question_with_usage(room, question, k).await.map(|(res, _usage)| res)
    }

    /// Like [`Self::answer_question`] but also returns the real Anthropic token
    /// [`Usage`] when a completion was made.
    ///
    /// `usage` is `None` on the no-Anthropic fallback path so the worker can fall
    /// back to the flat cost estimate; it is `Some` whenever a paid completion
    /// produced the answer, letting the worker record REAL cost (方向三).
    pub async fn answer_question_with_usage(
        &self,
        room: RoomId,
        question: &str,
        k: usize,
    ) -> Result<(AnswerResult, Option<Usage>)> {
        let q = question.trim();
        if q.is_empty() {
            return Err(AiError::Invalid("question must not be empty".into()));
        }
        let hits = self.retrieve_room(room, q, k.clamp(1, 20)).await?;

        let citations: Vec<MessageId> = hits.iter().map(|h| h.message.id).collect();
        let context = render_context(&hits);

        if let Some(client) = &self.anthropic {
            let system = ANSWER_SYSTEM_PROMPT;
            let user = format!(
                "问题: {q}\n\n相关聊天上下文(每段已附 ID,引用时使用):\n{context}\n\n请基于上述上下文作答,若信息不足请说明。"
            );
            let (answer, usage) =
                client.complete_with_usage(system, &[ChatMsg::user(user)], 800).await?;
            return Ok((AnswerResult { answer, citations }, Some(usage)));
        }

        // No Anthropic — return the raw context so the UI can still surface
        // grounded results. This is a dev-mode escape hatch, not a quality
        // claim.
        let fallback = if hits.is_empty() {
            "（未配置 Anthropic,也未在房间内检索到相关消息。）".to_string()
        } else {
            format!("（未配置 Anthropic,以下为检索到的相关消息上下文:）\n\n{context}")
        };
        Ok((AnswerResult { answer: fallback, citations }, None))
    }

    /// Agentic answer (方向三 "能动AI"): instead of one fixed retrieval, give the
    /// model a room-scoped `search_messages` tool and let it search — possibly
    /// several times with refined queries — before answering. Citations are the
    /// (deduplicated) message ids the tool actually surfaced across the loop.
    ///
    /// Authz is safe by construction: the only tool is scoped to `room`, which the
    /// caller is already a member of (the question is asked *in* that room), so the
    /// agent can reach nothing the asker couldn't already read. Without an Anthropic
    /// key it falls back to the one-shot [`Self::answer_question_with_usage`].
    ///
    /// Takes `Arc<Self>` because the tool holds a handle back to the service to run
    /// retrieval. `max_iters` is clamped to a small bound so a misbehaving model
    /// can't loop unboundedly (each iteration is a billed model call).
    ///
    /// # Errors
    /// [`AiError::Invalid`] for an empty question; otherwise propagates model /
    /// retrieval failures.
    pub async fn answer_question_agentic(
        self: &Arc<Self>,
        room: RoomId,
        question: &str,
        max_iters: usize,
    ) -> Result<(AnswerResult, Option<Usage>)> {
        let q = question.trim();
        if q.is_empty() {
            return Err(AiError::Invalid("question must not be empty".into()));
        }
        // No key → no agency; fall back to the grounded one-shot answer.
        let Some(client) = self.anthropic.clone() else {
            return self.answer_question_with_usage(room, q, 8).await;
        };
        let search_tool = Arc::new(SearchMessagesTool {
            svc: Arc::clone(self),
            room,
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let mut tools: Vec<Arc<dyn crate::agent::AgentTool>> = vec![search_tool.clone()];
        // Offer attachment reading only when a blob store is wired (方向三 file RAG).
        if self.blob_store.is_some() {
            tools.push(Arc::new(ReadAttachmentTool { svc: Arc::clone(self), room }));
        }
        let outcome = crate::agent::run_agent_loop(
            client.as_ref(),
            &tools,
            AGENT_SYSTEM_PROMPT,
            q,
            max_iters.clamp(1, 6),
            800,
        )
        .await?;
        // Dedup citations in first-seen order.
        let mut deduped = Vec::new();
        {
            let seen = search_tool.seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut set = std::collections::HashSet::new();
            for id in seen.iter() {
                if set.insert(*id) {
                    deduped.push(*id);
                }
            }
        }
        Ok((AnswerResult { answer: outcome.answer, citations: deduped }, Some(outcome.usage)))
    }

    /// Streaming variant of [`Self::answer_question`].
    ///
    /// Returns `(citations, stream)` where `citations` is available immediately
    /// (before the first token arrives) so the UI can render source chips without
    /// waiting for generation to finish.
    ///
    /// Without Anthropic the fallback answer is returned as a single-item stream
    /// so callers need not special-case the no-key path.
    ///
    /// # Errors
    /// Returns [`AiError::Invalid`] for an empty question; otherwise propagates
    /// embedder, storage, or Anthropic failures.
    pub async fn answer_question_stream(
        &self,
        room: RoomId,
        question: &str,
        k: usize,
    ) -> Result<(Vec<MessageId>, Pin<Box<dyn Stream<Item = Result<String>> + Send + 'static>>)> {
        let q = question.trim();
        if q.is_empty() {
            return Err(AiError::Invalid("question must not be empty".into()));
        }
        let hits = self.retrieve_room(room, q, k.clamp(1, 20)).await?;

        let citations: Vec<MessageId> = hits.iter().map(|h| h.message.id).collect();
        let context = render_context(&hits);

        if let Some(client) = &self.anthropic {
            let user = format!(
                "问题: {q}\n\n相关聊天上下文(每段已附 ID,引用时使用):\n{context}\n\n请基于上述上下文作答,若信息不足请说明。"
            );
            let stream = client
                .complete_stream(ANSWER_SYSTEM_PROMPT, &[ChatMsg::user(user)], 800)
                .await?;
            return Ok((citations, Box::pin(stream)));
        }

        let fallback = if hits.is_empty() {
            "（未配置 Anthropic,也未在房间内检索到相关消息。）".to_string()
        } else {
            format!("（未配置 Anthropic,以下为检索到的相关消息上下文:）\n\n{context}")
        };
        let stream: Pin<Box<dyn Stream<Item = Result<String>> + Send + 'static>> =
            Box::pin(futures::stream::once(async move { Ok(fallback) }));
        Ok((citations, stream))
    }

    /// Answer a question using both RAG context and rolling conversational memory.
    ///
    /// Extends [`Self::answer_question`] with session continuity: prior Q&A turns
    /// for this `(participant, room)` pair are prepended as alternating
    /// `user`/`assistant` messages so the model can refer back to them. After
    /// the reply the new question and answer are appended to the session history
    /// (trimmed to the last [`AI_CONTEXT_MAX_TURNS`] turns via Redis ZREMRANGEBYRANK).
    ///
    /// Without a context store or without Anthropic the method degrades silently
    /// to [`Self::answer_question`] behaviour.
    ///
    /// # Errors
    /// Returns [`AiError::Invalid`] for an empty question; otherwise propagates
    /// embedder, storage, or Anthropic failures. Context store errors are logged
    /// and swallowed — they never fail the answer.
    pub async fn ask_with_context(
        &self,
        participant: ParticipantId,
        room: RoomId,
        question: &str,
        k: usize,
    ) -> Result<AnswerResult> {
        let q = question.trim();
        if q.is_empty() {
            return Err(AiError::Invalid("question must not be empty".into()));
        }
        let hits = self.retrieve_room(room, q, k.clamp(1, 20)).await?;
        let citations: Vec<MessageId> = hits.iter().map(|h| h.message.id).collect();
        let context = render_context(&hits);

        if let Some(client) = &self.anthropic {
            // Load prior turns (best-effort; ignore errors so context store
            // unavailability never blocks answering).
            let prior_turns = if let Some(ctx) = &self.context {
                ctx.get_turns(participant, room, 6).await.unwrap_or_default()
            } else {
                Vec::new()
            };

            let mut messages: Vec<ChatMsg> = prior_turns
                .into_iter()
                .map(|(role, text)| ChatMsg { role, content: text })
                .collect();

            let user_msg = format!(
                "问题: {q}\n\n相关聊天上下文(每段已附 ID,引用时使用):\n{context}\n\n请基于上述上下文作答,若信息不足请说明。"
            );
            messages.push(ChatMsg::user(user_msg));

            // Model-tier routing (方向一·1): classify difficulty from the query +
            // grounding, route to the tier's model. Opt-in/default-OFF ⇒ `None` ⇒
            // the client's default model (unchanged unless a tier env is set).
            let model = crate::tier::tier_model(crate::tier::classify_tier(q, hits.len()));
            let answer = client
                .complete_model(model.as_deref(), ANSWER_SYSTEM_PROMPT, &messages, 800)
                .await?;

            // Persist new Q&A turns; log but never fail on Redis errors.
            // Store the raw question/answer (not the prompt with RAG context)
            // so the session history reads as a natural conversation.
            if let Some(ctx) = &self.context {
                if let Err(e) = ctx.push_turn(participant, room, "user", q).await {
                    tracing::warn!(error = %e, "ai context: failed to save user turn");
                }
                if let Err(e) = ctx.push_turn(participant, room, "assistant", &answer).await {
                    tracing::warn!(error = %e, "ai context: failed to save assistant turn");
                }
            }

            return Ok(AnswerResult { answer, citations });
        }

        // No Anthropic — fall back to raw context display.
        let fallback = if hits.is_empty() {
            "（未配置 Anthropic,也未在房间内检索到相关消息。）".to_string()
        } else {
            format!("（未配置 Anthropic,以下为检索到的相关消息上下文:）\n\n{context}")
        };
        Ok(AnswerResult { answer: fallback, citations })
    }

    /// Answer a question grounded in EVERY room the caller belongs to within a
    /// workspace — the flagship "ask your workspace" RAG flow.
    ///
    /// Mirrors [`Self::answer_question`] step-for-step (embed the question,
    /// RRF-fused vector + FTS retrieval, ask Anthropic to answer using only the
    /// retrieved context and cite the message IDs), but retrieves cross-room via
    /// [`MessageRepo::search_vector_workspace`](aero_storage::MessageRepo::search_vector_workspace)
    /// and its FTS twin, whose `JOIN room_members` / `rooms.workspace_id` boundary
    /// keeps results to rooms the caller is a member of within `workspace`. Without Anthropic it
    /// degrades identically to [`Self::answer_question`]: the joined context block
    /// is returned as the answer so the UI can still surface grounded results.
    ///
    /// # Errors
    /// Returns [`AiError::Invalid`] for an empty question; otherwise propagates
    /// embedder, storage, or Anthropic failures.
    pub async fn answer_question_workspace(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        question: &str,
        k: usize,
    ) -> Result<AnswerResult> {
        let q = question.trim();
        if q.is_empty() {
            return Err(AiError::Invalid("question must not be empty".into()));
        }
        let hits = self.retrieve_workspace(participant, workspace, q, k.clamp(1, 20)).await?;

        let citations: Vec<MessageId> = hits.iter().map(|h| h.message.id).collect();
        let context = render_context(&hits);

        if let Some(client) = &self.anthropic {
            // CONSERVATIVE personalisation (持久跨房 AI 用户画像 使用点): when the
            // operator opted in AND this participant has a stored profile, prepend a
            // short profile hint to the system prompt so the answer tracks their
            // focus/tone. Default OFF → `prefix` is `None` and the prompt is
            // byte-identical to before; a profile read failure is swallowed (it must
            // never fail the answer). Retrieval/citations are untouched.
            let system = match self.profile_personalization_prefix(participant, workspace).await {
                Ok(Some(prefix)) => format!("{prefix}\n\n{ANSWER_SYSTEM_PROMPT}"),
                Ok(None) => ANSWER_SYSTEM_PROMPT.to_owned(),
                Err(e) => {
                    tracing::warn!(error = %e, "ai profile: personalization read failed (ignored)");
                    ANSWER_SYSTEM_PROMPT.to_owned()
                }
            };
            let user = format!(
                "问题: {q}\n\n相关聊天上下文(每段已附 ID,引用时使用):\n{context}\n\n请基于上述上下文作答,若信息不足请说明。"
            );
            // Model-tier routing (方向一·1) — same opt-in/default-OFF contract.
            let model = crate::tier::tier_model(crate::tier::classify_tier(q, hits.len()));
            let answer = client
                .complete_model(model.as_deref(), &system, &[ChatMsg::user(user)], 800)
                .await?;
            return Ok(AnswerResult { answer, citations });
        }

        // No Anthropic — return the raw context so the UI can still surface
        // grounded results. This is a dev-mode escape hatch, not a quality claim.
        let fallback = if hits.is_empty() {
            "（未配置 Anthropic,也未在工作区内检索到相关消息。）".to_string()
        } else {
            format!("（未配置 Anthropic,以下为检索到的相关消息上下文:）\n\n{context}")
        };
        Ok(AnswerResult { answer: fallback, citations })
    }

    // ---------- thread-scoped summarization ----------

    /// Summarize a thread — the flat chain of (non-deleted) replies hanging off a
    /// root message.
    ///
    /// Mirrors [`Self::summarize_room`] exactly (same Chinese bullet prompt, same
    /// LLM primitive, same heuristic fallback) but sources the transcript from
    /// [`MessageRepo::thread_replies`](aero_storage::MessageRepo::thread_replies)
    /// (oldest-first) instead of a room's recent window. `max_replies` caps how
    /// many replies are read (clamped to `[1, 200]`). An empty thread (no replies)
    /// returns an empty string. Degrades to the deterministic "last 5 lines"
    /// heuristic when Anthropic is disabled; never errors on a missing key.
    ///
    /// # Errors
    /// Propagates a storage or Anthropic failure when a key IS configured; the
    /// no-key path is infallible.
    pub async fn summarize_thread(
        &self,
        root: MessageId,
        max_replies: usize,
    ) -> Result<String> {
        // `max_replies` is clamped to [1, 200] so this `as i64` is always safe.
        #[allow(clippy::cast_possible_wrap)]
        let limit = max_replies.clamp(1, 200) as i64;
        // `thread_replies` is oldest-first (chronological for the LLM); honour the
        // caller's reply cap (the split had dropped `limit`, feeding the model up to
        // 500 replies newest-first regardless of `max_replies`).
        let replies = self.messages.thread_replies(root, None, limit).await?;

        if replies.is_empty() {
            return Ok(String::new());
        }

        let transcript = render_transcript(&replies);

        if let Some(client) = &self.anthropic {
            let system = SUMMARIZE_SYSTEM_PROMPT;
            let user = format!(
                "请阅读以下话题串(thread)的回复记录,并按照系统指令给出要点摘要。\n\n回复记录:\n{transcript}"
            );
            let msgs = vec![ChatMsg::user(user)];
            return client.complete(system, &msgs, 600).await;
        }

        Ok(heuristic_summary(&replies))
    }

    /// Generate a short (5-10 word) title for a thread, given its root message and
    /// (oldest-first) reply chain.
    ///
    /// Mirrors [`Self::summarize_thread`]'s sourcing — the SAME
    /// [`MessageRepo::thread_replies`](aero_storage::MessageRepo::thread_replies)
    /// flat-thread set (deleted replies excluded) — but asks for a concise *title*
    /// rather than a bullet summary. The root message itself anchors the title (the
    /// reply chain is supporting context); `max_replies` caps how many replies are
    /// read (clamped to `[1, 200]`).
    ///
    /// With an Anthropic key it prompts for a 5-10 word title. Without a key it
    /// DEGRADES SAFELY to a deterministic heuristic — the first ~8 words of the root
    /// message — so the route is verifiable as 200-with-heuristic. A missing root
    /// returns an empty title; the no-key path is infallible.
    ///
    /// # Errors
    /// Propagates a storage or Anthropic failure when a key IS configured; the
    /// no-key path never errors on a missing key.
    pub async fn generate_thread_title(
        &self,
        root: MessageId,
        max_replies: usize,
    ) -> Result<String> {
        // Resolve the root message — it anchors the title.
        let Some(root_msg) = self.messages.get(root).await? else {
            return Ok(String::new());
        };
        let root_text = root_msg.searchable_text();

        // `max_replies` is clamped to [1, 200] so this `as i64` is always safe.
        #[allow(clippy::cast_possible_wrap)]
        let limit = max_replies.clamp(1, 200) as i64;
        // `thread_replies` is oldest-first (chronological for the LLM); honour the
        // caller's reply cap (the split had dropped `limit`/ordering).
        let replies = self.messages.thread_replies(root, None, limit).await?;

        if let Some(client) = &self.anthropic {
            let mut context = String::new();
            {
                use std::fmt::Write as _;
                let _ = writeln!(context, "根消息: {root_text}");
                let reply_transcript = render_transcript(&replies);
                if !reply_transcript.is_empty() {
                    let _ = write!(context, "\n回复:\n{reply_transcript}");
                }
            }
            let system = THREAD_TITLE_SYSTEM_PROMPT;
            let user = format!(
                "请为以下话题串生成一个简短标题(5-10 个词),概括其主旨。\n\n{context}"
            );
            let msgs = vec![ChatMsg::user(user)];
            let title = client.complete(system, &msgs, 60).await?;
            return Ok(clean_title(&title));
        }

        // Degrade safely: synthesize a title from the first ~8 words of the root.
        Ok(heuristic_title(&root_text))
    }

    /// Summarize the most recent activity across EVERY channel the caller belongs
    /// to within a workspace — the workspace twin of [`Self::summarize_room`].
    ///
    /// Pulls the caller's recent cross-room messages over the SAME
    /// membership/workspace boundary the workspace RAG uses
    /// ([`MessageRepo::recent_workspace`](aero_storage::MessageRepo::recent_workspace)),
    /// renders them chronologically, and asks Anthropic for the same Chinese bullet
    /// summary. `last_n` is clamped to `[1, 200]`. With no messages returns an empty
    /// string; without Anthropic degrades to the deterministic "last 5 lines"
    /// heuristic. Backs the scheduled workspace digest.
    ///
    /// # Errors
    /// Propagates a storage or Anthropic failure when a key IS configured; the
    /// no-key path is infallible.
    pub async fn summarize_workspace(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        last_n: usize,
    ) -> Result<String> {
        // `last_n` is clamped to [1, 200] so this `as i64` is always safe.
        #[allow(clippy::cast_possible_wrap)]
        let limit = last_n.clamp(1, 200) as i64;
        let mut recent = self.messages.recent_workspace(participant, workspace, limit).await?;
        // `recent_workspace` returns newest-first; reverse to chronological.
        recent.reverse();

        if recent.is_empty() {
            return Ok(String::new());
        }

        let transcript = render_transcript(&recent);

        if let Some(client) = &self.anthropic {
            let system = SUMMARIZE_SYSTEM_PROMPT;
            let user = format!(
                "请阅读以下工作区近期聊天记录(跨多个频道),并按照系统指令给出要点摘要。\n\n聊天记录:\n{transcript}"
            );
            let msgs = vec![ChatMsg::user(user)];
            return client.complete(system, &msgs, 600).await;
        }

        Ok(heuristic_summary(&recent))
    }

    // ---------- find expert ----------

    /// Rank workspace members by topical authority on `topic`.
    ///
    /// Embeds the topic, runs the membership-bounded cross-room vector search
    /// ([`MessageRepo::search_vector_workspace`](aero_storage::MessageRepo::search_vector_workspace)),
    /// then aggregates the hits by author: each author's relevance is the SUM of
    /// their matching messages' similarity scores, and a few of their highest-
    /// scoring message ids are kept as citations. Returns the top-`k` authors,
    /// strongest first. NO LLM call — purely retrieval + aggregation, so it never
    /// errors on a missing Anthropic key (it degrades to whatever the embedder and
    /// vector index return, which is empty rather than an error when nothing
    /// matches). `pool` bounds how wide a candidate set is aggregated.
    ///
    /// # Errors
    /// Propagates embedder or storage failures.
    pub async fn find_expert(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        topic: &str,
        k: usize,
        pool: usize,
    ) -> Result<Vec<Expert>> {
        let topic = topic.trim();
        if topic.is_empty() {
            return Err(AiError::Invalid("topic must not be empty".into()));
        }
        let query_vec = self.embedder.embed_query(topic).await?;
        // `pool` bounds the candidate breadth aggregated; clamp to a sane window.
        #[allow(clippy::cast_possible_wrap)]
        let pool_limit = pool.clamp(1, 200) as i64;
        let hits = self
            .messages
            .search_vector_workspace(participant, workspace, query_vec, pool_limit)
            .await?;
        Ok(rank_experts(&hits, k.clamp(1, 50)))
    }

    // ---------- recommendations (suggested channels & people) ----------

    /// Rank channel candidates the caller isn't in into "channels to join"
    /// recommendations.
    ///
    /// `candidates` is the pre-fetched candidate set — each `(room, name, activity)`
    /// is a non-private, non-archived workspace channel the caller is NOT a member
    /// of, paired with a recent-activity count
    /// ([`RoomRepo::list_workspace_channels_not_member`](aero_storage::RoomRepo::list_workspace_channels_not_member)).
    /// Ranking is a pure aggregation over those counts ([`rank_channels`]) so it is
    /// deterministic and unit-tested offline: busier channels rank higher, ties
    /// broken by room id, truncated to top-`k`. This is the no-embeddings degrade
    /// path — it never calls an LLM and never errors, returning an empty list when
    /// there are no candidates.
    ///
    /// The method lives on the AI service (rather than inline in the handler) so a
    /// future embeddings-aware ranker — scoring candidate channels by similarity of
    /// their recent messages to the caller's authored messages — can slot in behind
    /// the same seam without touching the route.
    #[must_use]
    pub fn recommend_channels(
        &self,
        candidates: &[(RoomId, String, i64)],
        k: usize,
    ) -> Vec<ChannelRec> {
        rank_channels(candidates, k.clamp(1, 50))
    }

    /// Rank people candidates the caller doesn't already follow into "people to
    /// follow" recommendations.
    ///
    /// `candidates` is the pre-filtered candidate set — each `(participant, shared)`
    /// is a workspace member the caller is not (and does not already follow), paired
    /// with the number of rooms they share with the caller
    /// ([`RoomRepo::shared_room_counts_in_workspace`](aero_storage::RoomRepo::shared_room_counts_in_workspace)
    /// minus the already-followed set). Ranking is a pure aggregation
    /// ([`rank_people`]): more shared rooms ranks higher, ties broken by
    /// participant id, truncated to top-`k`. Deterministic, never errors, empty in
    /// → empty out. This is the shared-channel-count degrade path.
    #[must_use]
    pub fn recommend_people(
        &self,
        candidates: &[(ParticipantId, i64)],
        k: usize,
    ) -> Vec<PersonRec> {
        rank_people(candidates, k.clamp(1, 50))
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

    // ---------- moderation (P5 AI 内容审核) ----------

    /// Classify a message body. Returns `Some(reason)` to block, `None` to allow.
    ///
    /// Requires Anthropic; without it returns `None` (the synchronous
    /// `AERO_BLOCKED_WORDS` keyword filter in `ImService` remains the only gate).
    /// Conservative by construction: only an explicit `BLOCK` verdict blocks.
    pub async fn moderate(&self, text: &str) -> Result<Option<String>> {
        self.moderate_with_usage(text).await.map(|(verdict, _usage)| verdict)
    }

    /// Like [`Self::moderate`] but also returns the real token [`Usage`] of the
    /// classification call, so the worker can charge BILLED cost instead of a
    /// flat estimate (ROADMAP 方向四). `usage` is `None` when no paid call was
    /// made (empty text or no Anthropic key), in which case cost is zero.
    pub async fn moderate_with_usage(
        &self,
        text: &str,
    ) -> Result<(Option<String>, Option<Usage>)> {
        let text = text.trim();
        if text.is_empty() {
            return Ok((None, None));
        }
        let Some(client) = &self.anthropic else {
            return Ok((None, None));
        };
        let (verdict, usage) = client
            .complete_with_usage(
                MODERATE_SYSTEM_PROMPT,
                &[ChatMsg::user(format!("待审核内容:\n{text}"))],
                120,
            )
            .await?;
        Ok((parse_moderation_verdict(&verdict), Some(usage)))
    }

    /// Score a message's affect: coarse sentiment, a `[0.0, 1.0]` toxicity
    /// likelihood, and a short tone label.
    ///
    /// Additive to the binary [`Self::moderate`] gate — this NEVER blocks, it just
    /// describes tone for a UI affordance. With an Anthropic key it prompts for a
    /// structured one-line verdict ([`parse_sentiment_verdict`]). Without a key it
    /// DEGRADES SAFELY to a deterministic keyword/punctuation heuristic
    /// ([`heuristic_sentiment`]): ALL-CAPS or insult keywords raise toxicity and an
    /// angry tone; exclamation/positive words read positive; otherwise neutral with
    /// low toxicity. Empty text scores neutral. Never errors on a missing key.
    ///
    /// # Errors
    /// Propagates an Anthropic failure when a key IS configured; the no-key path is
    /// infallible.
    pub async fn score_message_sentiment(&self, text: &str) -> Result<SentimentScore> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(SentimentScore::neutral());
        }
        let Some(client) = &self.anthropic else {
            return Ok(heuristic_sentiment(text));
        };
        let verdict = client
            .complete(
                SENTIMENT_SYSTEM_PROMPT,
                &[ChatMsg::user(format!("待评估内容:\n{text}"))],
                120,
            )
            .await?;
        // A malformed model line degrades to the heuristic rather than erroring,
        // so the route always returns a well-formed score.
        Ok(parse_sentiment_verdict(&verdict).unwrap_or_else(|| heuristic_sentiment(text)))
    }
}

/// Parse a sentiment verdict line. Protocol (one line, pipe-separated):
/// `SENTIMENT|TOXICITY|TONE` — e.g. `negative|0.82|angry`. Returns `None` when the
/// line cannot be parsed into all three fields so the caller can fall back to the
/// heuristic. Tolerant of surrounding whitespace and case; toxicity is clamped to
/// `[0.0, 1.0]`.
pub(crate) fn parse_sentiment_verdict(raw: &str) -> Option<SentimentScore> {
    // Take the first non-blank line — models occasionally add a trailing note.
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    let mut parts = line.splitn(3, '|');
    let sentiment = Sentiment::from_label(parts.next()?);
    let toxicity = parts.next()?.trim().parse::<f32>().ok()?.clamp(0.0, 1.0);
    let tone = parts.next()?.trim();
    let tone = if tone.is_empty() { sentiment.as_str() } else { tone };
    Some(SentimentScore { sentiment, toxicity, tone: tone.to_owned() })
}

