//! Provider wrappers that make durable usage acceptance part of AI success.

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::agent::ToolChat;
use crate::anthropic::{AgentTurn, AnthropicClient, ChatMsg, ToolDef, Usage};
use crate::error::{AiError, Result};
use crate::metrics::CostModel;
use crate::usage::{
    UsageContext, UsageEvent, UsageOutcome, UsagePersistOutcome, UsageReservation,
    UsageReserveOutcome,
};
use aero_common::{RoomId, WorkspaceId};
use futures::{Stream, StreamExt};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use super::{
    heuristic_sentiment, heuristic_text_digest, parse_moderation_verdict, parse_sentiment_verdict,
    AiService, SentimentScore, MODERATE_SYSTEM_PROMPT, RECAP_SYSTEM_PROMPT,
    SENTIMENT_SYSTEM_PROMPT, TRANSLATE_SYSTEM_PROMPT,
};

/// Coarse one-minute Whisper estimate ($0.006). The API response does not expose
/// audio duration/billed units, so the ledger explicitly labels this an estimate.
const WHISPER_ESTIMATE_MICROS: u64 = 6_000;
const COMPLETION_OUTCOME: &str = "anthropic_completion_v1";
const AGENT_TURN_OUTCOME: &str = "anthropic_agent_turn_v1";
const EMBEDDING_OUTCOME: &str = "embedding_f32_v1";
const TRANSCRIPT_OUTCOME: &str = "transcript_v1";
const STREAM_OUTCOME: &str = "anthropic_stream_v1";

enum ProviderReservation {
    Acquired(UsageReservation),
    Replayed(UsageOutcome),
}

#[derive(Debug, Serialize, Deserialize)]
struct CompletionOutcome {
    text: String,
    usage: Usage,
}

#[derive(Debug, Serialize, Deserialize)]
struct EmbeddingOutcome {
    values: Vec<f32>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TextOutcome {
    text: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct StreamOutcome {
    chunks: Vec<String>,
    terminal_error: Option<String>,
}

/// Per-turn accounting adapter for the tool-use loop. Each successful provider
/// response is accepted durably before another model turn or tool can run.
pub(crate) struct AccountedToolChat<'a> {
    service: &'a AiService,
    inner: &'a dyn ToolChat,
    context: UsageContext,
    operation_prefix: &'static str,
    kind: &'static str,
    fallback_micros: u64,
    turn: AtomicUsize,
}

#[async_trait::async_trait]
impl ToolChat for AccountedToolChat<'_> {
    async fn next_turn(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        tools: &[ToolDef],
        max_tokens: u32,
    ) -> Result<AgentTurn> {
        let turn_number = self.turn.fetch_add(1, Ordering::Relaxed);
        let operation = format!("{}_turn_{turn_number}", self.operation_prefix);
        let reservation = match self
            .service
            .reserve_provider_micros(
                self.context,
                &operation,
                self.kind,
                self.fallback_micros,
                AGENT_TURN_OUTCOME,
            )
            .await?
        {
            ProviderReservation::Acquired(reservation) => reservation,
            ProviderReservation::Replayed(outcome) => {
                return AiService::decode_provider_outcome(outcome, AGENT_TURN_OUTCOME);
            }
        };
        let turn = match self
            .inner
            .next_turn(system, messages, tools, max_tokens)
            .await
        {
            Ok(turn) => turn,
            Err(error) => {
                return Err(self.service.settle_provider_error(reservation, error).await);
            }
        };
        let micros = AiService::anthropic_micros(turn.usage, self.fallback_micros);
        let outcome = AiService::encode_provider_outcome(AGENT_TURN_OUTCOME, &turn)?;
        self.service
            .finalize_provider_micros(reservation, self.context, self.kind, micros, outcome)
            .await?;
        Ok(turn)
    }
}

impl AiService {
    /// Transcribe an audio blob via the configured transcriber.
    pub async fn transcribe(&self, bytes: bytes::Bytes, mime: &str) -> Result<String> {
        self.transcribe_with_context(bytes, mime, UsageContext::new(None), "transcribe")
            .await
    }

    /// Embed arbitrary text, accounting only when the configured provider is paid.
    pub async fn embed_text(&self, text: &str) -> Result<Vec<f32>> {
        self.embed_text_with_context(text, UsageContext::new(None), "embed_document")
            .await
    }

    /// Summarize arbitrary text, with a local heuristic when Anthropic is absent.
    pub async fn summarize_text(&self, text: &str) -> Result<String> {
        self.summarize_text_with_usage_context(text, UsageContext::new(None))
            .await
    }

    pub async fn summarize_text_with_usage_context(
        &self,
        text: &str,
        usage_context: UsageContext,
    ) -> Result<String> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(String::new());
        }
        if let Some(client) = &self.anthropic {
            let user = format!(
                "请阅读以下通话/会议记录,并按照系统指令给出简短复盘与行动项。\n\n记录:\n{text}"
            );
            let (summary, _) = self
                .complete_accounted(
                    client,
                    usage_context,
                    "anthropic_recap",
                    "summarize_text",
                    CostModel::default().summarize_micros,
                    RECAP_SYSTEM_PROMPT,
                    &[ChatMsg::user(user)],
                    600,
                )
                .await?;
            return Ok(summary);
        }
        Ok(heuristic_text_digest(text))
    }

    /// Translate short text, or echo it when Anthropic is absent.
    pub async fn translate(&self, text: &str, target_lang: &str) -> Result<String> {
        self.translate_with_usage_context(text, target_lang, UsageContext::new(None))
            .await
    }

    pub async fn translate_with_usage_context(
        &self,
        text: &str,
        target_lang: &str,
        usage_context: UsageContext,
    ) -> Result<String> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(String::new());
        }
        if let Some(client) = &self.anthropic {
            let user = format!(
                "目标语言: {target_lang}\n只输出译文本身,不要解释、不要引号。\n\n原文:\n{text}"
            );
            let (translation, _) = self
                .complete_accounted(
                    client,
                    usage_context,
                    "anthropic_translate",
                    "translate",
                    CostModel::default().moderate_micros,
                    TRANSLATE_SYSTEM_PROMPT,
                    &[ChatMsg::user(user)],
                    400,
                )
                .await?;
            return Ok(translation);
        }
        Ok(text.to_owned())
    }

    /// Classify a message body; the no-key path conservatively allows it.
    pub async fn moderate(&self, text: &str) -> Result<Option<String>> {
        self.moderate_with_usage(text)
            .await
            .map(|(verdict, _usage)| verdict)
    }

    pub async fn moderate_with_usage(&self, text: &str) -> Result<(Option<String>, Option<Usage>)> {
        self.moderate_with_usage_context(text, UsageContext::new(None))
            .await
    }

    pub async fn moderate_with_usage_context(
        &self,
        text: &str,
        usage_context: UsageContext,
    ) -> Result<(Option<String>, Option<Usage>)> {
        let text = text.trim();
        if text.is_empty() {
            return Ok((None, None));
        }
        let Some(client) = &self.anthropic else {
            return Ok((None, None));
        };
        let (verdict, usage) = self
            .complete_accounted(
                client,
                usage_context,
                "anthropic_moderate",
                "moderate",
                CostModel::default().moderate_micros,
                MODERATE_SYSTEM_PROMPT,
                &[ChatMsg::user(format!("待审核内容:\n{text}"))],
                120,
            )
            .await?;
        Ok((parse_moderation_verdict(&verdict), Some(usage)))
    }

    /// Score message sentiment, using a deterministic local fallback without a key.
    pub async fn score_message_sentiment(&self, text: &str) -> Result<SentimentScore> {
        self.score_message_sentiment_with_usage_context(text, UsageContext::new(None))
            .await
    }

    pub async fn score_message_sentiment_with_usage_context(
        &self,
        text: &str,
        usage_context: UsageContext,
    ) -> Result<SentimentScore> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(SentimentScore::neutral());
        }
        let Some(client) = &self.anthropic else {
            return Ok(heuristic_sentiment(text));
        };
        let (verdict, _) = self
            .complete_accounted(
                client,
                usage_context,
                "anthropic_sentiment",
                "sentiment",
                CostModel::default().moderate_micros,
                SENTIMENT_SYSTEM_PROMPT,
                &[ChatMsg::user(format!("待评估内容:\n{text}"))],
                120,
            )
            .await?;
        Ok(parse_sentiment_verdict(&verdict).unwrap_or_else(|| heuristic_sentiment(text)))
    }

    pub(crate) fn accounted_tool_chat<'a>(
        &'a self,
        inner: &'a dyn ToolChat,
        context: UsageContext,
        operation_prefix: &'static str,
        kind: &'static str,
        fallback_micros: u64,
    ) -> AccountedToolChat<'a> {
        AccountedToolChat {
            service: self,
            inner,
            context,
            operation_prefix,
            kind,
            fallback_micros,
            turn: AtomicUsize::new(0),
        }
    }

    pub(crate) async fn fresh_room_usage_context(&self, room: RoomId) -> Result<UsageContext> {
        let workspace = self
            .rooms
            .room_workspace(room)
            .await?
            .map(|workspace| workspace.to_uuid());
        Ok(UsageContext::new(workspace))
    }

    async fn reserve_provider_micros(
        &self,
        context: UsageContext,
        operation: &str,
        kind: &str,
        estimated_micros: u64,
        outcome_kind: &str,
    ) -> Result<ProviderReservation> {
        let usage_id = context.operation_id(operation);
        if estimated_micros == 0 {
            return Err(AiError::Storage(format!(
                "AI usage accounting: paid reservation {usage_id} has zero estimate"
            )));
        }
        let sink = self.usage_sink.as_deref().ok_or_else(|| {
            AiError::Storage(format!(
                "AI usage accounting: paid usage sink is not configured \
                 (usage_id={usage_id}, kind={kind})"
            ))
        })?;
        match sink
            .reserve(UsageEvent {
                usage_id,
                workspace: context.workspace,
                kind: kind.to_owned(),
                micros: estimated_micros,
                outcome_kind: Some(outcome_kind.to_owned()),
            })
            .await
            .map_err(|error| AiError::Storage(format!("AI usage accounting: {error}")))?
        {
            UsageReserveOutcome::Acquired(reservation) => {
                Ok(ProviderReservation::Acquired(reservation))
            }
            UsageReserveOutcome::InFlight => Err(AiError::Storage(format!(
                "AI usage accounting: provider reservation is already active \
                 (usage_id={usage_id}, kind={kind})"
            ))),
            UsageReserveOutcome::AlreadyFinalized(Some(outcome)) => {
                if outcome.kind != outcome_kind {
                    return Err(AiError::Storage(format!(
                        "AI usage accounting: replay schema mismatch for {usage_id}: \
                         expected {outcome_kind}, got {}",
                        outcome.kind
                    )));
                }
                Ok(ProviderReservation::Replayed(outcome))
            }
            UsageReserveOutcome::AlreadyFinalized(None) => Err(AiError::Storage(format!(
                "AI usage accounting: provider operation {usage_id} was finalized \
                 without a replayable result after an ambiguous outcome"
            ))),
        }
    }

    fn encode_provider_outcome<T: Serialize>(kind: &str, value: &T) -> Result<UsageOutcome> {
        Ok(UsageOutcome {
            kind: kind.to_owned(),
            payload: serde_json::to_value(value).map_err(|error| {
                AiError::Storage(format!("AI usage outcome serialization failed: {error}"))
            })?,
        })
    }

    fn decode_provider_outcome<T: DeserializeOwned>(
        outcome: UsageOutcome,
        expected_kind: &str,
    ) -> Result<T> {
        if outcome.kind != expected_kind {
            return Err(AiError::Storage(format!(
                "AI usage replay schema mismatch: expected {expected_kind}, got {}",
                outcome.kind
            )));
        }
        serde_json::from_value(outcome.payload).map_err(|error| {
            AiError::Storage(format!(
                "AI usage replay payload for {expected_kind} is invalid: {error}"
            ))
        })
    }

    async fn finalize_provider_micros(
        &self,
        reservation: UsageReservation,
        context: UsageContext,
        kind: &str,
        micros: u64,
        outcome: UsageOutcome,
    ) -> Result<()> {
        let sink = self.usage_sink.as_deref().ok_or_else(|| {
            AiError::Storage(format!(
                "AI usage accounting: paid usage sink disappeared \
                 (usage_id={}, kind={kind})",
                reservation.usage_id
            ))
        })?;
        let outcome = sink
            .finalize(reservation, micros, Some(outcome))
            .await
            .map_err(|error| AiError::Storage(format!("AI usage accounting: {error}")))?;
        if outcome == UsagePersistOutcome::Inserted {
            crate::metrics::charge_cost_label(
                aero_common::metrics::global(),
                kind,
                context.workspace,
                micros,
            );
        }
        Ok(())
    }

    async fn settle_provider_error(
        &self,
        reservation: UsageReservation,
        provider_error: AiError,
    ) -> AiError {
        if !definitive_no_charge(&provider_error) {
            tracing::warn!(
                usage_id = %reservation.usage_id,
                error = %provider_error,
                "paid provider outcome is ambiguous; retaining conservative reservation"
            );
            return AiError::Storage(format!(
                "AI usage accounting: provider outcome is ambiguous; reservation {} \
                 retained for conservative settlement; provider error: {provider_error}",
                reservation.usage_id
            ));
        }
        let Some(sink) = self.usage_sink.as_deref() else {
            return AiError::Storage(format!(
                "AI usage accounting: sink disappeared while cancelling {}; \
                 provider error: {provider_error}",
                reservation.usage_id
            ));
        };
        match sink.cancel(reservation).await {
            Ok(true) => provider_error,
            Ok(false) => AiError::Storage(format!(
                "AI usage accounting: reservation {} could not be cancelled and \
                 remains conservatively chargeable; provider error: {provider_error}",
                reservation.usage_id
            )),
            Err(error) => AiError::Storage(format!(
                "AI usage accounting: reservation {} cancellation failed: {error}; \
                 provider error: {provider_error}",
                reservation.usage_id
            )),
        }
    }

    fn anthropic_micros(usage: Usage, fallback_micros: u64) -> u64 {
        let measured = CostModel::default().token_micros(usage.input_tokens, usage.output_tokens);
        // A successful Anthropic response should carry usage. A malformed/older
        // response that omitted it must not turn a paid call into a free one.
        if measured == 0 {
            fallback_micros
        } else {
            measured
        }
    }

    // Internal accounting helper with a fixed signature; grouping params into a
    // struct would churn the single caller for no behavioral gain.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn complete_accounted(
        &self,
        client: &AnthropicClient,
        context: UsageContext,
        operation: &str,
        kind: &str,
        fallback_micros: u64,
        system: &str,
        messages: &[ChatMsg],
        max_tokens: u32,
    ) -> Result<(String, Usage)> {
        self.complete_model_accounted(
            client,
            context,
            operation,
            kind,
            fallback_micros,
            None,
            system,
            messages,
            max_tokens,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn complete_model_accounted(
        &self,
        client: &AnthropicClient,
        context: UsageContext,
        operation: &str,
        kind: &str,
        fallback_micros: u64,
        model: Option<&str>,
        system: &str,
        messages: &[ChatMsg],
        max_tokens: u32,
    ) -> Result<(String, Usage)> {
        let reservation = match self
            .reserve_provider_micros(
                context,
                operation,
                kind,
                fallback_micros,
                COMPLETION_OUTCOME,
            )
            .await?
        {
            ProviderReservation::Acquired(reservation) => reservation,
            ProviderReservation::Replayed(outcome) => {
                let saved: CompletionOutcome =
                    Self::decode_provider_outcome(outcome, COMPLETION_OUTCOME)?;
                return Ok((saved.text, saved.usage));
            }
        };
        let (text, tokens) = match client
            .complete_with_usage_model(model, system, messages, max_tokens)
            .await
        {
            Ok(response) => response,
            Err(error) => return Err(self.settle_provider_error(reservation, error).await),
        };
        let micros = Self::anthropic_micros(tokens, fallback_micros);
        let outcome = Self::encode_provider_outcome(
            COMPLETION_OUTCOME,
            &CompletionOutcome {
                text: text.clone(),
                usage: tokens,
            },
        )?;
        self.finalize_provider_micros(reservation, context, kind, micros, outcome)
            .await?;
        Ok((text, tokens))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn complete_stream_accounted(
        &self,
        client: &AnthropicClient,
        context: UsageContext,
        operation: &str,
        kind: &str,
        fallback_micros: u64,
        system: &str,
        messages: &[ChatMsg],
        max_tokens: u32,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send + 'static>>> {
        let reservation = match self
            .reserve_provider_micros(context, operation, kind, fallback_micros, STREAM_OUTCOME)
            .await?
        {
            ProviderReservation::Acquired(reservation) => reservation,
            ProviderReservation::Replayed(outcome) => {
                let saved: StreamOutcome = Self::decode_provider_outcome(outcome, STREAM_OUTCOME)?;
                let mut items: Vec<Result<String>> = saved.chunks.into_iter().map(Ok).collect();
                if let Some(error) = saved.terminal_error {
                    items.push(Err(AiError::Http(format!(
                        "durable provider stream ended with: {error}"
                    ))));
                }
                return Ok(Box::pin(futures::stream::iter(items)));
            }
        };
        let stream = match client.complete_stream(system, messages, max_tokens).await {
            Ok(stream) => stream,
            Err(error) => return Err(self.settle_provider_error(reservation, error).await),
        };
        let sink = self.usage_sink.clone().ok_or_else(|| {
            AiError::Storage("AI usage sink disappeared before stream completion".into())
        })?;
        let kind = kind.to_owned();
        // The provider response is capped by `max_tokens` (800 at the route),
        // so this queue is semantically bounded even though it is unbounded at
        // the channel type. Decoupling it from a slow/disconnected HTTP writer
        // lets the background task finish and durably save the replay outcome.
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<String>>();
        tokio::spawn(async move {
            let mut stream = Box::pin(stream);
            let mut chunks = Vec::new();
            let mut pending = None;
            let mut terminal_error = None;
            while let Some(item) = stream.next().await {
                match item {
                    Ok(chunk) => {
                        chunks.push(chunk.clone());
                        if let Some(previous) = pending.replace(chunk) {
                            let _ = tx.send(Ok(previous));
                        }
                    }
                    Err(error) => {
                        terminal_error = Some(error.to_string());
                        break;
                    }
                }
            }
            let payload = serde_json::to_value(StreamOutcome {
                chunks,
                terminal_error: terminal_error.clone(),
            });
            let persist = match payload {
                Ok(payload) => sink
                    .finalize(
                        reservation,
                        fallback_micros,
                        Some(UsageOutcome {
                            kind: STREAM_OUTCOME.to_owned(),
                            payload,
                        }),
                    )
                    .await
                    .map_err(|error| {
                        AiError::Storage(format!("AI usage stream finalization failed: {error}"))
                    }),
                Err(error) => Err(AiError::Storage(format!(
                    "AI usage stream outcome serialization failed: {error}"
                ))),
            };
            match persist {
                Ok(UsagePersistOutcome::Inserted) => crate::metrics::charge_cost_label(
                    aero_common::metrics::global(),
                    &kind,
                    context.workspace,
                    fallback_micros,
                ),
                Ok(UsagePersistOutcome::Duplicate) => {}
                Err(error) => {
                    let _ = tx.send(Err(error));
                    return;
                }
            }
            // Keep the final chunk back until the complete replay payload and
            // charge are durable. Earlier chunks retain normal SSE latency.
            if let Some(last) = pending {
                let _ = tx.send(Ok(last));
            }
            if let Some(error) = terminal_error {
                let _ = tx.send(Err(AiError::Http(format!(
                    "provider stream ended with: {error}"
                ))));
            }
        });
        Ok(Box::pin(futures::stream::unfold(rx, |mut rx| async {
            rx.recv().await.map(|item| (item, rx))
        })))
    }

    pub async fn embed_text_with_context(
        &self,
        text: &str,
        context: UsageContext,
        operation: &str,
    ) -> Result<Vec<f32>> {
        if !self.embedder.is_paid_provider() {
            return self.embedder.embed_one(text).await;
        }
        let micros = CostModel::default().embed_micros;
        let reservation = match self
            .reserve_provider_micros(context, operation, "embed", micros, EMBEDDING_OUTCOME)
            .await?
        {
            ProviderReservation::Acquired(reservation) => reservation,
            ProviderReservation::Replayed(outcome) => {
                let saved: EmbeddingOutcome =
                    Self::decode_provider_outcome(outcome, EMBEDDING_OUTCOME)?;
                return Ok(saved.values);
            }
        };
        let embedding = match self.embedder.embed_one(text).await {
            Ok(embedding) => embedding,
            Err(error) => return Err(self.settle_provider_error(reservation, error).await),
        };
        let outcome = Self::encode_provider_outcome(
            EMBEDDING_OUTCOME,
            &EmbeddingOutcome {
                values: embedding.clone(),
            },
        )?;
        self.finalize_provider_micros(reservation, context, "embed", micros, outcome)
            .await?;
        Ok(embedding)
    }

    pub(crate) async fn embed_query_with_context(
        &self,
        text: &str,
        context: UsageContext,
        operation: &str,
    ) -> Result<Vec<f32>> {
        if !self.embedder.is_paid_provider() {
            return self.embedder.embed_query(text).await;
        }
        let micros = CostModel::default().embed_micros;
        let reservation = match self
            .reserve_provider_micros(context, operation, "embed_query", micros, EMBEDDING_OUTCOME)
            .await?
        {
            ProviderReservation::Acquired(reservation) => reservation,
            ProviderReservation::Replayed(outcome) => {
                let saved: EmbeddingOutcome =
                    Self::decode_provider_outcome(outcome, EMBEDDING_OUTCOME)?;
                return Ok(saved.values);
            }
        };
        let embedding = match self.embedder.embed_query(text).await {
            Ok(embedding) => embedding,
            Err(error) => return Err(self.settle_provider_error(reservation, error).await),
        };
        let outcome = Self::encode_provider_outcome(
            EMBEDDING_OUTCOME,
            &EmbeddingOutcome {
                values: embedding.clone(),
            },
        )?;
        self.finalize_provider_micros(reservation, context, "embed_query", micros, outcome)
            .await?;
        Ok(embedding)
    }

    pub async fn transcribe_with_context(
        &self,
        bytes: bytes::Bytes,
        mime: &str,
        context: UsageContext,
        operation: &str,
    ) -> Result<String> {
        if !self.transcriber.is_paid_provider() {
            return self.transcriber.transcribe(bytes, mime).await;
        }
        let reservation = match self
            .reserve_provider_micros(
                context,
                operation,
                "transcribe_estimate",
                WHISPER_ESTIMATE_MICROS,
                TRANSCRIPT_OUTCOME,
            )
            .await?
        {
            ProviderReservation::Acquired(reservation) => reservation,
            ProviderReservation::Replayed(outcome) => {
                let saved: TextOutcome =
                    Self::decode_provider_outcome(outcome, TRANSCRIPT_OUTCOME)?;
                return Ok(saved.text);
            }
        };
        let transcript = match self.transcriber.transcribe(bytes, mime).await {
            Ok(transcript) => transcript,
            Err(error) => return Err(self.settle_provider_error(reservation, error).await),
        };
        let outcome = Self::encode_provider_outcome(
            TRANSCRIPT_OUTCOME,
            &TextOutcome {
                text: transcript.clone(),
            },
        )?;
        self.finalize_provider_micros(
            reservation,
            context,
            "transcribe_estimate",
            WHISPER_ESTIMATE_MICROS,
            outcome,
        )
        .await?;
        Ok(transcript)
    }
}

fn definitive_no_charge(error: &AiError) -> bool {
    match error {
        AiError::Invalid(_) | AiError::Config(_) | AiError::Anthropic { .. } => true,
        AiError::Embedding(message) => has_http_failure_status(message, "voyage "),
        AiError::Internal(message) => {
            message.starts_with("multipart mime:") || has_http_failure_status(message, "whisper ")
        }
        AiError::Http(_) | AiError::Json(_) | AiError::Storage(_) | AiError::NotFound(_) => false,
    }
}

fn has_http_failure_status(message: &str, prefix: &str) -> bool {
    message
        .strip_prefix(prefix)
        .and_then(|rest| rest.split(':').next())
        .and_then(|status| status.trim().parse::<u16>().ok())
        .is_some_and(|status| (400..600).contains(&status))
}

#[cfg(test)]
#[path = "accounting/tests.rs"]
mod tests;
