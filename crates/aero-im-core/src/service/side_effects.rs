//! Durable message side-effect relay (notifications + AI jobs).

use aero_common::{MessageId, Result};
use aero_storage::{AiJobKind, MessageSideEffectJob, MessageSideEffectKind, MessageSideEffectRepo};
use anyhow::{anyhow, Context};
use time::{Duration, OffsetDateTime};
use tracing::{debug, warn};

use crate::{moderation_text, service::ImService};

const SIDE_EFFECT_LEASE: Duration = Duration::seconds(30);

impl ImService {
    /// Claim and process up to `limit` durable message side effects.
    pub async fn dispatch_message_side_effect_batch(&self, limit: i64) -> Result<usize> {
        let repo = MessageSideEffectRepo::new(self.messages.pool.clone());
        let jobs = repo
            .claim_due(OffsetDateTime::now_utc(), SIDE_EFFECT_LEASE, limit)
            .await?;
        let mut completed = 0;
        for job in jobs {
            if self.finish_message_side_effect(&repo, job).await {
                completed += 1;
            }
        }
        Ok(completed)
    }

    /// Nudge and process unfinished work for one canonical message. Used by the
    /// post-commit fast path and sender-idempotency retries.
    pub async fn dispatch_message_side_effects_for(&self, message_id: MessageId) -> Result<usize> {
        let repo = MessageSideEffectRepo::new(self.messages.pool.clone());
        let now = OffsetDateTime::now_utc();
        repo.nudge_message(message_id, now, SIDE_EFFECT_LEASE)
            .await?;
        let jobs = repo
            .claim_for_message(message_id, now, SIDE_EFFECT_LEASE)
            .await?;
        let mut completed = 0;
        for job in jobs {
            if self.finish_message_side_effect(&repo, job).await {
                completed += 1;
            }
        }
        Ok(completed)
    }

    async fn finish_message_side_effect(
        &self,
        repo: &MessageSideEffectRepo,
        job: MessageSideEffectJob,
    ) -> bool {
        match self.process_message_side_effect(repo, &job).await {
            Ok(()) => {
                debug!(
                    job_id = %job.id,
                    message_id = %job.message_id,
                    kind = job.kind.as_str(),
                    attempts = job.attempts,
                    "message side effect completed"
                );
                true
            }
            Err(error) => {
                match repo
                    .mark_failed(
                        job.id,
                        job.attempts,
                        OffsetDateTime::now_utc(),
                        &error.to_string(),
                    )
                    .await
                {
                    Err(mark_error) => warn!(
                        ?mark_error,
                        %error,
                        job_id = %job.id,
                        message_id = %job.message_id,
                        kind = job.kind.as_str(),
                        "message side effect failed and could not be re-parked"
                    ),
                    Ok(true) => warn!(
                        %error,
                        job_id = %job.id,
                        message_id = %job.message_id,
                        kind = job.kind.as_str(),
                        "message side effect failed; retry scheduled"
                    ),
                    Ok(false) => debug!(
                        %error,
                        job_id = %job.id,
                        message_id = %job.message_id,
                        kind = job.kind.as_str(),
                        "message side-effect failure belongs to a superseded lease"
                    ),
                }
                false
            }
        }
    }

    async fn process_message_side_effect(
        &self,
        repo: &MessageSideEffectRepo,
        job: &MessageSideEffectJob,
    ) -> anyhow::Result<()> {
        let now = OffsetDateTime::now_utc();
        let current = self
            .messages
            .get(job.message_id)
            .await
            .context("load side-effect message")?;
        let Some(message) = current.filter(|message| {
            message.deleted_at.is_none()
                && message
                    .expires_at
                    .map_or(true, |expires_at| expires_at > now)
        }) else {
            complete_claim(repo, job, now).await?;
            return Ok(());
        };

        match job.kind {
            MessageSideEffectKind::Notifications => {
                // Notifications intentionally follow the current message if an
                // edit landed before this creation-time job ran. Membership is
                // also re-read now, never taken from the original send snapshot.
                let members = self
                    .rooms
                    .members(message.room_id)
                    .await
                    .context("resolve current notification members")?;
                self.dispatch_notifications(&message, &members, (job.id, job.attempts))
                    .await
                    .map_err(|error| anyhow!(error))?;
            }
            MessageSideEffectKind::Embed | MessageSideEffectKind::Moderate => {
                // An edit transaction appends fresh AI jobs. Older versions can
                // complete without spending on stale text.
                if message.version != job.mutation_version {
                    complete_claim(repo, job, now).await?;
                    return Ok(());
                }
                let text = moderation_text(&message.blocks);
                if text.is_empty() {
                    complete_claim(repo, job, now).await?;
                    return Ok(());
                }
                let workspace = self
                    .rooms
                    .room_workspace(message.room_id)
                    .await
                    .context("resolve side-effect workspace")?
                    .map(|workspace| workspace.to_uuid());
                let (kind, payload) = match job.kind {
                    MessageSideEffectKind::Embed => (
                        AiJobKind::Embed,
                        serde_json::json!({"room_id": message.room_id.to_string()}),
                    ),
                    MessageSideEffectKind::Moderate => {
                        (AiJobKind::Moderate, serde_json::json!({"text": text}))
                    }
                    MessageSideEffectKind::Notifications => unreachable!(),
                };
                let completed = repo
                    .complete_with_ai_job(
                        job.id,
                        job.attempts,
                        kind,
                        message.id,
                        workspace,
                        payload,
                        OffsetDateTime::now_utc(),
                    )
                    .await
                    .context("atomically enqueue AI job")?;
                if !completed {
                    return Err(anyhow!("message side-effect lease was superseded"));
                }
            }
        }
        Ok(())
    }
}

async fn complete_claim(
    repo: &MessageSideEffectRepo,
    job: &MessageSideEffectJob,
    now: OffsetDateTime,
) -> anyhow::Result<()> {
    if repo
        .complete(job.id, job.attempts, now)
        .await
        .context("complete message side effect")?
    {
        Ok(())
    } else {
        Err(anyhow!("message side-effect lease was superseded"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_is_longer_than_the_short_poll_interval() {
        assert!(SIDE_EFFECT_LEASE >= Duration::seconds(10));
    }
}
