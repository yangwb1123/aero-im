//! Bridge from the `aero-ai` crate to the [`AiBackend`] trait declared in
//! `state.rs`. Keeping this tiny isolates the rest of the server from the
//! aero-ai surface and lets us swap implementations later (e.g. local model,
//! a managed agent service) without touching the routes.

use std::pin::Pin;
use std::sync::Arc;

use aero_ai::AiService;
use aero_common::{MessageId, ParticipantId, RoomId, WorkspaceId};
use async_trait::async_trait;
use futures::StreamExt as _;

use crate::state::{AiAnswer, AiBackend, AiChannelRec, AiExpert, AiPersonRec, AiSentiment};

/// Candidate pool breadth aggregated by [`AiService::find_expert`]. Wider than the
/// returned top-k so a deep-but-relevant author can still surface.
const EXPERT_POOL: usize = 60;

pub struct AiServiceAdapter {
    inner: Arc<AiService>,
}

impl AiServiceAdapter {
    pub fn new(inner: Arc<AiService>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl AiBackend for AiServiceAdapter {
    async fn summarize_room(&self, room: RoomId, last_n: usize) -> Result<String, String> {
        self.inner
            .summarize_room(room, last_n)
            .await
            .map_err(|e| e.to_string())
    }

    async fn answer_question(
        &self,
        room: RoomId,
        question: &str,
        k: usize,
    ) -> Result<AiAnswer, String> {
        let result = self
            .inner
            .answer_question(room, question, k)
            .await
            .map_err(|e| e.to_string())?;
        Ok(AiAnswer {
            answer: result.answer,
            citations: result.citations.into_iter().collect::<Vec<MessageId>>(),
        })
    }

    async fn answer_question_workspace(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        question: &str,
        k: usize,
    ) -> Result<AiAnswer, String> {
        let result = self
            .inner
            .answer_question_workspace(participant, workspace, question, k)
            .await
            .map_err(|e| e.to_string())?;
        Ok(AiAnswer {
            answer: result.answer,
            citations: result.citations.into_iter().collect::<Vec<MessageId>>(),
        })
    }

    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, String> {
        self.inner.embed_text(text).await.map_err(|e| e.to_string())
    }

    async fn translate(&self, text: &str, target_lang: &str) -> Result<String, String> {
        self.inner.translate(text, target_lang).await.map_err(|e| e.to_string())
    }

    async fn summarize_text(&self, text: &str) -> Result<String, String> {
        self.inner.summarize_text(text).await.map_err(|e| e.to_string())
    }

    async fn summarize_thread(
        &self,
        root: MessageId,
        max_replies: usize,
    ) -> Result<String, String> {
        self.inner
            .summarize_thread(root, max_replies)
            .await
            .map_err(|e| e.to_string())
    }

    async fn summarize_workspace(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        last_n: usize,
    ) -> Result<String, String> {
        self.inner
            .summarize_workspace(participant, workspace, last_n)
            .await
            .map_err(|e| e.to_string())
    }

    async fn find_expert(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        topic: &str,
        k: usize,
    ) -> Result<Vec<AiExpert>, String> {
        let experts = self
            .inner
            .find_expert(participant, workspace, topic, k, EXPERT_POOL)
            .await
            .map_err(|e| e.to_string())?;
        Ok(experts
            .into_iter()
            .map(|e| AiExpert {
                participant: e.participant,
                score: e.score,
                citations: e.citations,
            })
            .collect())
    }

    async fn recommend_channels(
        &self,
        candidates: Vec<(RoomId, String, i64)>,
        k: usize,
    ) -> Result<Vec<AiChannelRec>, String> {
        // Pure, deterministic ranking — infallible, so this never errors.
        Ok(self
            .inner
            .recommend_channels(&candidates, k)
            .into_iter()
            .map(|c| AiChannelRec {
                room: c.room,
                name: c.name,
                score: c.score,
                reason: c.reason,
            })
            .collect())
    }

    async fn recommend_people(
        &self,
        candidates: Vec<(ParticipantId, i64)>,
        k: usize,
    ) -> Result<Vec<AiPersonRec>, String> {
        Ok(self
            .inner
            .recommend_people(&candidates, k)
            .into_iter()
            .map(|p| AiPersonRec {
                participant: p.participant,
                score: p.score,
                reason: p.reason,
            })
            .collect())
    }

    async fn moderate(&self, text: &str) -> Result<Option<String>, String> {
        self.inner.moderate(text).await.map_err(|e| e.to_string())
    }

    async fn generate_thread_title(
        &self,
        root: MessageId,
        max_replies: usize,
    ) -> Result<String, String> {
        self.inner
            .generate_thread_title(root, max_replies)
            .await
            .map_err(|e| e.to_string())
    }

    async fn score_sentiment(&self, text: &str) -> Result<AiSentiment, String> {
        let score = self
            .inner
            .score_message_sentiment(text)
            .await
            .map_err(|e| e.to_string())?;
        Ok(AiSentiment {
            sentiment: score.sentiment.as_str().to_owned(),
            toxicity: score.toxicity,
            tone: score.tone,
        })
    }

    async fn answer_question_stream(
        &self,
        room: RoomId,
        question: &str,
        k: usize,
    ) -> Result<
        (
            Vec<MessageId>,
            Pin<Box<dyn futures::Stream<Item = Result<String, String>> + Send + 'static>>,
        ),
        String,
    > {
        let (citations, stream) = self
            .inner
            .answer_question_stream(room, question, k)
            .await
            .map_err(|e| e.to_string())?;
        let boxed: Pin<Box<dyn futures::Stream<Item = Result<String, String>> + Send + 'static>> =
            Box::pin(stream.map(|r| r.map_err(|e| e.to_string())));
        Ok((citations, boxed))
    }

    async fn ask_with_context(
        &self,
        participant: ParticipantId,
        room: RoomId,
        question: &str,
        k: usize,
    ) -> Result<AiAnswer, String> {
        let result = self
            .inner
            .ask_with_context(participant, room, question, k)
            .await
            .map_err(|e| e.to_string())?;
        Ok(AiAnswer { answer: result.answer, citations: result.citations })
    }
}
