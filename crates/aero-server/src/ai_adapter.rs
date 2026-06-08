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

use crate::state::{AiAnswer, AiBackend};

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

    async fn moderate(&self, text: &str) -> Result<Option<String>, String> {
        self.inner.moderate(text).await.map_err(|e| e.to_string())
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
