//! Background worker that drains the `ai_jobs` queue.
//!
//! Loops on `claim()` → process per kind → `complete()`/`fail()`. Runs in the
//! server process; one instance can drive embed + summarize + answer + moderate
//! since the work is I/O bound. Concurrency *between* claimed jobs is sequential
//! by design — Postgres `FOR UPDATE SKIP LOCKED` lets us scale by running
//! multiple workers in separate processes when load demands it.

use std::sync::Arc;
use std::time::Duration;

use aero_common::{MessageId, RoomId};
use aero_storage::{AiJob, AiJobKind};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use crate::error::{AiError, Result};
use crate::service::AiService;

const MAX_ATTEMPTS: i32 = 5;
const BATCH_SIZE: i32 = 8;
const IDLE_SLEEP: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct AiWorker {
    svc: Arc<AiService>,
}

impl AiWorker {
    pub fn new(svc: Arc<AiService>) -> Self {
        Self { svc }
    }

    /// Run the worker until `shutdown` is cancelled.
    ///
    /// Errors fetching the next batch are logged but do not terminate the loop;
    /// individual job errors are persisted via `ai_jobs.fail()`. The outer task
    /// thus survives transient Postgres hiccups.
    pub async fn run(&self, shutdown: CancellationToken) {
        tracing::info!("ai worker: starting");
        loop {
            if shutdown.is_cancelled() {
                tracing::info!("ai worker: shutdown signal received, exiting");
                return;
            }

            let claim_fut = self.svc.ai_jobs().claim(BATCH_SIZE);
            let jobs = tokio::select! {
                () = shutdown.cancelled() => return,
                res = claim_fut => match res {
                    Ok(j) => j,
                    Err(e) => {
                        tracing::warn!(error = %e, "ai worker: claim failed");
                        if sleep_or_cancel(IDLE_SLEEP, &shutdown).await {
                            return;
                        }
                        continue;
                    }
                },
            };

            if jobs.is_empty() {
                if sleep_or_cancel(IDLE_SLEEP, &shutdown).await {
                    return;
                }
                continue;
            }

            for job in jobs {
                let id = job.id;
                let kind = job.kind;
                match self.process(job).await {
                    Ok(result) => {
                        if let Err(e) = self.svc.ai_jobs().complete(id, result).await {
                            tracing::error!(
                                job_id = %id,
                                error = %e,
                                "ai worker: completion write failed"
                            );
                        } else {
                            tracing::debug!(job_id = %id, ?kind, "ai worker: job done");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(job_id = %id, ?kind, error = %e, "ai worker: job failed");
                        if let Err(db_err) = self.svc.ai_jobs().fail(id, &e.to_string(), MAX_ATTEMPTS).await {
                            tracing::error!(
                                job_id = %id,
                                error = %db_err,
                                "ai worker: failure write failed"
                            );
                        }
                    }
                }
            }
        }
    }

    async fn process(&self, job: AiJob) -> Result<serde_json::Value> {
        match job.kind {
            AiJobKind::Embed => self.handle_embed(&job).await,
            AiJobKind::Summarize => self.handle_summarize(&job).await,
            AiJobKind::Moderate => self.handle_moderate(&job).await,
            AiJobKind::Answer => self.handle_answer(&job).await,
        }
    }

    async fn handle_embed(&self, job: &AiJob) -> Result<serde_json::Value> {
        let target = job
            .target_id
            .ok_or_else(|| AiError::Invalid("embed job missing target_id".into()))?;
        let id = MessageId::from_uuid(target);
        let msg = self
            .svc
            .messages()
            .get(id)
            .await?
            .ok_or_else(|| AiError::NotFound(format!("message {id}")))?;

        let text = msg.searchable_text();
        let embedding = self.svc.embed_text(&text).await?;
        let dim = embedding.len();
        let model = self.svc.embedder().model_id().to_string();

        let updated = self.svc.messages().update_embedding(id, embedding).await?;
        if !updated {
            // Message may have been deleted between get() and update — not an error.
            tracing::debug!(message_id = %id, "embed: row missing or deleted at update");
        }
        Ok(serde_json::json!({ "dim": dim, "model": model, "updated": updated }))
    }

    async fn handle_summarize(&self, job: &AiJob) -> Result<serde_json::Value> {
        let p: SummarizePayload = serde_json::from_value(job.payload.clone())?;
        let room = parse_room_id(&p.room_id)?;
        let last_n = p.last_n.unwrap_or(50);
        let summary = self.svc.summarize_room(room, last_n).await?;
        Ok(serde_json::json!({
            "summary": summary,
            "anthropic": self.svc.has_anthropic(),
            "last_n": last_n,
        }))
    }

    async fn handle_moderate(&self, _job: &AiJob) -> Result<serde_json::Value> {
        // P5 will plug in real moderation (OpenAI Moderation or local classifier).
        // For P2 we record a clean "ok" verdict so downstream consumers can wire
        // their plumbing now.
        Ok(serde_json::json!({ "verdict": "ok", "stub": true }))
    }

    async fn handle_answer(&self, job: &AiJob) -> Result<serde_json::Value> {
        let p: AnswerPayload = serde_json::from_value(job.payload.clone())?;
        let room = parse_room_id(&p.room_id)?;
        let k = p.k.unwrap_or(8);
        let result = self.svc.answer_question(room, &p.question, k).await?;
        let citations: Vec<String> = result.citations.iter().map(MessageId::to_string).collect();
        Ok(serde_json::json!({
            "answer": result.answer,
            "citations": citations,
            "anthropic": self.svc.has_anthropic(),
        }))
    }
}

// ---------- payload shapes ----------

#[derive(Debug, Deserialize)]
struct SummarizePayload {
    room_id: String,
    #[serde(default)]
    last_n: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct AnswerPayload {
    room_id: String,
    question: String,
    #[serde(default)]
    k: Option<usize>,
}

fn parse_room_id(s: &str) -> Result<RoomId> {
    // Accept either ULID or UUID — IDs are ULIDs in our domain but stored as
    // UUIDs in Postgres, so payloads coming from either source should work.
    if let Ok(u) = Ulid::from_string(s) {
        return Ok(RoomId::from_ulid(u));
    }
    if let Ok(u) = uuid::Uuid::parse_str(s) {
        return Ok(RoomId::from_uuid(u));
    }
    Err(AiError::Invalid(format!("not a valid room_id: {s}")))
}

/// Sleep for `dur`, returning `true` if cancellation fired during the wait.
async fn sleep_or_cancel(dur: Duration, shutdown: &CancellationToken) -> bool {
    tokio::select! {
        () = tokio::time::sleep(dur) => false,
        () = shutdown.cancelled() => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_room_id_accepts_ulid_and_uuid() {
        let id = RoomId::new();
        let parsed_ulid = parse_room_id(&id.to_string()).unwrap();
        assert_eq!(parsed_ulid, id);

        let uuid_str = id.to_uuid().to_string();
        let parsed_uuid = parse_room_id(&uuid_str).unwrap();
        assert_eq!(parsed_uuid, id);
    }

    #[test]
    fn parse_room_id_rejects_garbage() {
        assert!(parse_room_id("not-a-real-id").is_err());
    }

    #[test]
    fn summarize_payload_defaults() {
        let v = serde_json::json!({ "room_id": "01HXXXXXXXXXXXXXXXXXXXXXXX" });
        let p: SummarizePayload = serde_json::from_value(v).unwrap();
        assert!(p.last_n.is_none());
    }

    #[test]
    fn answer_payload_round_trip() {
        let v = serde_json::json!({
            "room_id": "01HXXXXXXXXXXXXXXXXXXXXXXX",
            "question": "what happened?",
            "k": 5
        });
        let p: AnswerPayload = serde_json::from_value(v).unwrap();
        assert_eq!(p.question, "what happened?");
        assert_eq!(p.k, Some(5));
    }
}
