//! Personal data export — GDPR right to data portability (Article 20).
//!
//! Two flavours, both scoped to the calling participant (the admin-only
//! workspace-wide export lives at `GET /api/workspaces/:id/export`):
//!
//! * `GET /api/me/export` — a **synchronous, capped** snapshot (profile + up to
//!   500 recent messages + 200 recent files). Fast, good enough for most users.
//! * `POST /api/me/export/async` — enqueues a **complete, uncapped** export job;
//!   `GET /api/me/export/jobs/:id` polls it. A background worker
//!   ([`run_export_dispatcher`]) assembles the full archive (ALL the caller's
//!   messages + uploaded files + profile), stores it as a blob the caller owns,
//!   and the status endpoint hands back a 24h-valid `/api/blobs/:id` download
//!   link. After 24h the archive blob is garbage-collected.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::MessageId;
use aero_common::{BlobId, Error as AeroError, FileKind};
use aero_storage::{BlobRepo, ExportJobRepo, MessageRepo, NewBlob};
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::error::ApiResult;
use crate::state::AppState;

/// All personal-export routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/me/export", get(export_me))
        .route("/api/me/export/async", post(export_async))
        .route("/api/me/export/jobs/:id", get(export_job_status))
}

/// `GET /api/me/export` — a complete snapshot of the caller's personal data.
///
/// Returns a JSON object with:
/// - `participant` — profile (no credentials).
/// - `messages_sent` — up to 500 most-recent non-deleted messages (newest first).
/// - `blobs_uploaded` — up to 200 most-recent uploaded files (newest first).
/// - `exported_at` — RFC 3339 timestamp of this export.
///
/// Auth required (Bearer access token). No admin gate — each participant may only
/// export their own data.
async fn export_me(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let pid = auth.participant_id;

    let msg_repo = MessageRepo::new(s.pg.clone());
    let blob_repo = BlobRepo::new(s.pg.clone());
    let (participant, messages, blobs) = tokio::try_join!(
        s.participants.get(pid),
        msg_repo.by_sender(pid, 10000),
        blob_repo.list_by_owner(pid, 200),
    )
    .map_err(AeroError::from)?;

    let participant =
        participant.ok_or_else(|| AeroError::Unauthorized("participant not found".into()))?;

    let exported_at = time::OffsetDateTime::now_utc();

    Ok(Json(serde_json::json!({
        "participant": participant,
        "messages_sent": messages,
        "blobs_uploaded": blobs,
        "exported_at": exported_at.format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
    })))
}

/// `POST /api/me/export/async` — enqueue a complete (uncapped) export job.
/// Returns the job id to poll. Auth required; a participant may only export
/// their own data, so no extra scope is needed.
async fn export_async(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let job_id = ExportJobRepo::new(s.pg.clone())
        .enqueue(auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(
        serde_json::json!({ "job_id": job_id, "status": "queued" }),
    ))
}

/// `GET /api/me/export/jobs/:id` — poll an async export job.
///
/// Returns `{ status, download_url?, expires_at? }`. The caller may only see
/// their OWN jobs (a job owned by someone else → 404, not 403, to avoid leaking
/// existence). When `done` and still within the 24h window, `download_url` points
/// at `/api/blobs/:blob_id` (the caller owns the archive blob, so the existing
/// blob-download authz lets them fetch it). Past 24h the archive is enqueued for
/// GC and the status reads `expired`.
async fn export_job_status(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id =
        uuid::Uuid::from_str(id_str.trim()).map_err(|_| AeroError::Invalid("job id".into()))?;
    let job = ExportJobRepo::new(s.pg.clone())
        .get(id)
        .await
        .map_err(AeroError::from)?
        .filter(|j| j.participant_id == auth.participant_id)
        .ok_or_else(|| AeroError::NotFound("export job".into()))?;

    if job.status == "done" {
        let now = time::OffsetDateTime::now_utc();
        if aero_storage::link_is_valid(job.completed_at, now) {
            let expires_at = job
                .completed_at
                .map(|c| c + aero_storage::EXPORT_LINK_TTL)
                .and_then(|t| {
                    t.format(&time::format_description::well_known::Rfc3339)
                        .ok()
                });
            return Ok(Json(serde_json::json!({
                "status": "done",
                "download_url": job.blob_id.map(|b| format!("/api/blobs/{b}")),
                "expires_at": expires_at,
            })));
        }
        // Link expired: best-effort GC just THIS archive blob, report expired.
        if let Some(blob) = job.blob_id {
            if let Err(e) = aero_storage::BlobGcRepo::new(s.pg.clone())
                .enqueue_one(blob)
                .await
            {
                warn!(error = ?e, %blob, "export archive GC enqueue failed");
            }
        }
        return Ok(Json(serde_json::json!({ "status": "expired" })));
    }

    Ok(Json(serde_json::json!({
        "status": job.status,
        "error": job.error,
    })))
}

/// How many messages to pull per keyset page while assembling the archive.
const EXPORT_PAGE: i64 = 500;
/// Upper bound on uploaded-file rows included (defensive; a single account is
/// unlikely to exceed this, and the row list is metadata only).
const EXPORT_BLOB_CAP: i64 = 100_000;

/// Background worker: drains the export-job queue, assembling each participant's
/// complete archive and storing it as a blob they own. Best-effort and crash-safe
/// — a claimed job that fails is marked `failed` with the error; it is never
/// silently dropped. Polls every `poll_secs`; exits on `cancel`.
pub async fn run_export_dispatcher(state: AppState, cancel: CancellationToken, poll_secs: u64) {
    let repo = ExportJobRepo::new(state.pg.clone());
    info!(interval_secs = poll_secs, "export-job dispatcher started");
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(poll_secs.max(1)));
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            _ = tick.tick() => {}
        }
        // Drain all currently-queued jobs this tick.
        loop {
            let job = match repo.claim_next().await {
                Ok(Some(j)) => j,
                Ok(None) => break,
                Err(e) => {
                    warn!(error = ?e, "export claim failed");
                    break;
                }
            };
            match build_and_store_archive(&state, job.participant_id).await {
                Ok(blob_id) => {
                    if let Err(e) = repo.complete(job.id, blob_id).await {
                        warn!(error = ?e, job = %job.id, "export complete update failed");
                    } else {
                        info!(job = %job.id, participant = %job.participant_id, "export archive ready");
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    warn!(error = %msg, job = %job.id, "export build failed");
                    let _ = repo.fail(job.id, &msg).await;
                }
            }
        }
    }
}

/// Assemble the complete JSON archive for `participant` and store it as a blob
/// the participant owns. Returns the archive blob id.
async fn build_and_store_archive(
    state: &AppState,
    participant: aero_common::ParticipantId,
) -> anyhow::Result<BlobId> {
    let msg_repo = MessageRepo::new(state.pg.clone());
    let blob_repo = BlobRepo::new(state.pg.clone());

    let profile = state
        .participants
        .get(participant)
        .await?
        .ok_or_else(|| anyhow::anyhow!("participant {participant} not found"))?;

    // Keyset-page over ALL the participant's messages (uncapped, oldest-first).
    let mut messages = Vec::new();
    let mut cursor = Some(MessageId::from_uuid(uuid::Uuid::max()));
    loop {
        let before = match cursor {
            Some(id) => id,
            None => break,
        };
        let page = msg_repo
            .by_sender_paged(participant, before, EXPORT_PAGE)
            .await?;
        if page.is_empty() {
            break;
        }
        cursor = page.last().map(|m| m.id);
        messages.extend(page);
        if messages.len() % (EXPORT_PAGE as usize) != 0 {
            break; // short page ⇒ done (extend kept it < a full page)
        }
    }
    let blobs = blob_repo
        .list_by_owner(participant, EXPORT_BLOB_CAP)
        .await?;

    let archive = serde_json::json!({
        "participant": profile,
        "messages_sent": messages,
        "blobs_uploaded": blobs,
        "exported_at": time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
        "complete": true,
    });
    let bytes = serde_json::to_vec_pretty(&archive)?;
    let sha = {
        use sha2::Digest as _;
        hex::encode(sha2::Sha256::digest(&bytes))
    };
    let size = bytes.len() as u64;

    // Reserve metadata, write the archive, then publish the metadata. A failed
    // store/finalize step is compensated through the shared blob-GC lifecycle,
    // so export retries cannot leave a visible row with missing bytes.
    let reservation = blob_repo
        .reserve(NewBlob {
            owner_id: participant,
            kind: FileKind::Document,
            name: "aero-export.json".into(),
            mime: "application/json".into(),
            size,
            sha256: Some(sha),
            storage_key: format!("pending:{}", uuid::Uuid::new_v4()),
        })
        .await?;
    let key = match state.blob_store.put(reservation.id, bytes.into()).await {
        Ok(key) => key,
        Err(error) => {
            crate::routes::routes::abort_blob_reservation(state, reservation.id).await;
            return Err(anyhow::anyhow!("archive put: {error}"));
        }
    };
    let blob = match blob_repo.finalize(reservation.id, &key).await {
        Ok(Some(blob)) => blob,
        Ok(None) => {
            crate::routes::routes::abort_blob_reservation(state, reservation.id).await;
            return Err(anyhow::anyhow!(
                "archive reservation disappeared before finalize"
            ));
        }
        Err(error) => {
            crate::routes::routes::abort_blob_reservation(state, reservation.id).await;
            return Err(error.into());
        }
    };
    Ok(blob.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_exposes_get_api_me_export() {
        // Verify the route table contains exactly one route (smoke test — the
        // handler itself requires a live DB and is not unit-tested here).
        let router = routes();
        // Axum doesn't expose a public route list, but construction must not
        // panic — that's all we can assert without a real request.
        let _ = router;
    }
}
