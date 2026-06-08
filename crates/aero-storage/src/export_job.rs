//! Asynchronous personal-data export jobs (ROADMAP 方向四 — GDPR Art. 20).
//!
//! Backs `migrations/0070_export_jobs.sql`. A participant requests a *complete*
//! export; a background worker (in `aero-server`) claims the job, assembles the
//! full JSON archive, stores it as a blob the participant owns, and marks the
//! job `done` with that blob id. The HTTP layer then hands back a 24h download
//! link to `/api/blobs/:id`.
//!
//! Claiming mirrors [`crate::ai_job::AiJobRepo::claim`]: `FOR UPDATE SKIP LOCKED`
//! so multiple workers never grab the same job.

use aero_common::{BlobId, ParticipantId};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

/// One export job row.
#[derive(Debug, Clone, Serialize)]
pub struct ExportJob {
    pub id: Uuid,
    pub participant_id: ParticipantId,
    /// `queued` / `processing` / `done` / `failed`.
    pub status: String,
    /// The produced archive blob, once `done`.
    pub blob_id: Option<BlobId>,
    pub error: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub completed_at: Option<time::OffsetDateTime>,
}

#[derive(Clone)]
pub struct ExportJobRepo {
    pool: PgPool,
}

impl ExportJobRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Enqueue a new export job for `participant`; returns its id.
    pub async fn enqueue(&self, participant: ParticipantId) -> Result<Uuid, sqlx::Error> {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO export_jobs (id, participant_id) VALUES ($1, $2)")
            .bind(id)
            .bind(participant.to_uuid())
            .execute(&self.pool)
            .await?;
        Ok(id)
    }

    /// Atomically claim the oldest queued job, marking it `processing`.
    /// `None` when the queue is empty. `FOR UPDATE SKIP LOCKED` keeps concurrent
    /// workers from claiming the same row.
    pub async fn claim_next(&self) -> Result<Option<ExportJob>, sqlx::Error> {
        let row = sqlx::query_as::<_, ExportJobRow>(
            r"UPDATE export_jobs SET status = 'processing'
                WHERE id IN (
                    SELECT id FROM export_jobs
                     WHERE status = 'queued'
                     ORDER BY created_at ASC
                     FOR UPDATE SKIP LOCKED
                     LIMIT 1
                )
                RETURNING id, participant_id, status, blob_id, error, created_at, completed_at",
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(ExportJob::from))
    }

    /// Mark a job `done`, recording the produced archive blob.
    pub async fn complete(&self, id: Uuid, blob: BlobId) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE export_jobs SET status = 'done', blob_id = $2, completed_at = NOW() WHERE id = $1",
        )
        .bind(id)
        .bind(blob.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Mark a job `failed` with an error message.
    pub async fn fail(&self, id: Uuid, error: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE export_jobs SET status = 'failed', error = $2, completed_at = NOW() WHERE id = $1",
        )
        .bind(id)
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetch a job by id (for the status endpoint).
    pub async fn get(&self, id: Uuid) -> Result<Option<ExportJob>, sqlx::Error> {
        let row = sqlx::query_as::<_, ExportJobRow>(
            r"SELECT id, participant_id, status, blob_id, error, created_at, completed_at
               FROM export_jobs WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(ExportJob::from))
    }
}

#[derive(sqlx::FromRow)]
struct ExportJobRow {
    id: Uuid,
    participant_id: Uuid,
    status: String,
    blob_id: Option<Uuid>,
    error: Option<String>,
    created_at: time::OffsetDateTime,
    completed_at: Option<time::OffsetDateTime>,
}

impl From<ExportJobRow> for ExportJob {
    fn from(r: ExportJobRow) -> Self {
        Self {
            id: r.id,
            participant_id: ParticipantId::from_uuid(r.participant_id),
            status: r.status,
            blob_id: r.blob_id.map(BlobId::from_uuid),
            error: r.error,
            created_at: r.created_at,
            completed_at: r.completed_at,
        }
    }
}

/// The download link for a completed export stays valid for this long after
/// `completed_at`; afterwards the archive blob is garbage-collected.
pub const EXPORT_LINK_TTL: time::Duration = time::Duration::hours(24);

/// Whether a completed job's download link is still valid at `now`.
#[must_use]
pub fn link_is_valid(completed_at: Option<time::OffsetDateTime>, now: time::OffsetDateTime) -> bool {
    matches!(completed_at, Some(done) if now < done + EXPORT_LINK_TTL)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_validity_window() {
        let t0 = time::OffsetDateTime::UNIX_EPOCH;
        // Not completed ⇒ never valid.
        assert!(!link_is_valid(None, t0));
        // Just completed ⇒ valid.
        assert!(link_is_valid(Some(t0), t0));
        // 23h later ⇒ still valid.
        assert!(link_is_valid(Some(t0), t0 + time::Duration::hours(23)));
        // 25h later ⇒ expired.
        assert!(!link_is_valid(Some(t0), t0 + time::Duration::hours(25)));
    }
}
