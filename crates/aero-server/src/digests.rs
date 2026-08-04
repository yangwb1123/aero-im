//! Scheduled / recurring AI digests.
//!
//! A participant subscribes to a recurring AI digest of a ROOM or a WORKSPACE on a
//! `daily` / `weekly` cadence. Thin handlers over
//! [`DigestSubscriptionRepo`](aero_storage::DigestSubscriptionRepo); the actual
//! summarization + delivery happens in the background [`run_digest_dispatcher`],
//! which mirrors [`crate::recurring::run_recurring_dispatcher`]: lease due rows,
//! durably prepare the summary, deliver it idempotently, and only then advance
//! `next_run_at`.
//!
//! Membership-gated at create time exactly like the rest of the app: a room digest
//! requires room access ([`assert_room_access`](aero_im_core::ImService::assert_room_access));
//! a workspace digest requires workspace membership (via the shared
//! [`WorkspaceRepo`](aero_storage::WorkspaceRepo), mirroring [`crate::workspace_ask`]).
//! `GET` / `DELETE` are owner-scoped (the caller can only see / remove their own).
//! Mounted via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;
use std::sync::Arc;

use aero_auth::AuthUser;
use aero_common::{
    Block, DigestSubscriptionId, Error as AeroError, ParticipantId, RoomId, WorkspaceId,
};
use aero_im_core::ImService;
use aero_storage::{
    digest_next_run_at,
    digest_subscription::{DigestDeliveryClaim, DigestFailureDisposition, WorkspaceDigestDelivery},
    validate_digest_frequency, DigestSubscriptionRepo, DigestTarget,
};
use axum::{
    extract::{Path, State},
    routing::post,
    Json, Router,
};
use futures::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::ApiResult;
use crate::state::{AiBackend, AppState};

/// All digest-subscription routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/digests", post(create_digest).get(list_digests))
        .route("/api/digests/:id", axum::routing::delete(delete_digest))
}

/// Upper bound on how many messages a single digest summarizes (mirrors the
/// summarize/catchup window caps).
const DIGEST_WINDOW: usize = 200;
const DELIVERY_LEASE: time::Duration = time::Duration::minutes(10);
const DELIVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const CLAIM_BATCH: i64 = 8;
const DELIVERY_CONCURRENCY: usize = 4;
const MAX_ATTEMPTS: i32 = 8;

#[derive(Serialize)]
struct DigestMessageFingerprint<'a> {
    version: u8,
    room_id: RoomId,
    blocks: &'a [Block],
    reply_to: Option<aero_common::MessageId>,
    expires_after_secs: Option<u64>,
}

fn digest_message_hash(room: RoomId, blocks: &[Block]) -> Result<[u8; 32], AeroError> {
    let canonical = serde_json::to_vec(&DigestMessageFingerprint {
        version: 1,
        room_id: room,
        blocks,
        reply_to: None,
        expires_after_secs: None,
    })?;
    Ok(Sha256::digest(canonical).into())
}

fn retry_backoff(attempt: i32) -> time::Duration {
    let exponent = u32::try_from(attempt.saturating_sub(1))
        .unwrap_or(0)
        .min(20);
    let multiplier = 1_i64.checked_shl(exponent).unwrap_or(i64::MAX);
    time::Duration::seconds(30_i64.saturating_mul(multiplier).min(3600))
}

fn send_error_retryable(error: &AeroError) -> bool {
    matches!(
        error,
        AeroError::RateLimited
            | AeroError::Upstream(_)
            | AeroError::Database(_)
            | AeroError::Internal(_)
    )
}

/// Build a [`DigestSubscriptionRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> DigestSubscriptionRepo {
    DigestSubscriptionRepo::new(s.pg.clone())
}

fn parse_digest(s: &str) -> Result<DigestSubscriptionId, AeroError> {
    DigestSubscriptionId::from_str(s.trim())
        .map_err(|e| AeroError::Invalid(format!("digest id: {e}")))
}

#[derive(Deserialize)]
struct CreateDigestReq {
    /// The room to digest. Exactly one of `room_id` / `workspace_id` must be set.
    #[serde(default)]
    room_id: Option<String>,
    /// The workspace to digest. Exactly one of `room_id` / `workspace_id` must be set.
    #[serde(default)]
    workspace_id: Option<String>,
    /// The cadence (`daily` / `weekly`).
    frequency: String,
}

/// `POST /api/digests` — subscribe to a recurring AI digest of a room or a
/// workspace. Requires room access (room digest) or workspace membership
/// (workspace digest). Rejects an unknown frequency, or a body that does not set
/// exactly one target (`400`). The first run is one cadence step from now. Returns
/// the created subscription.
async fn create_digest(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateDigestReq>,
) -> ApiResult<Json<serde_json::Value>> {
    // Exactly one target — mirror the table's room-XOR-workspace CHECK at the edge.
    let target = match (req.room_id.as_deref(), req.workspace_id.as_deref()) {
        (Some(r), None) => {
            let room = RoomId::from_str(r.trim())
                .map_err(|e| AeroError::Invalid(format!("room id: {e}")))?;
            // Tenant + room-membership guard before staging a room digest.
            s.im.assert_room_access(auth.participant_id, room).await?;
            DigestTarget::Room(room)
        }
        (None, Some(w)) => {
            let ws = WorkspaceId::from_str(w.trim())
                .map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))?;
            assert_workspace_member(&s, ws, auth.participant_id).await?;
            DigestTarget::Workspace(ws)
        }
        _ => {
            return Err(AeroError::Invalid(
                "exactly one of room_id / workspace_id must be set".into(),
            )
            .into())
        }
    };

    let frequency = req.frequency.trim();
    if !validate_digest_frequency(frequency) {
        return Err(AeroError::Invalid("frequency must be daily or weekly".into()).into());
    }
    let now = time::OffsetDateTime::now_utc();
    let first_run = digest_next_run_at(frequency, now)
        .ok_or_else(|| AeroError::Invalid("frequency must be daily or weekly".into()))?;

    let id = repo(&s)
        .create(auth.participant_id, target, frequency, first_run)
        .await
        .map_err(AeroError::from)?;
    // Re-read so the response carries the full, canonical row.
    let row = repo(&s)
        .list_for(auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .into_iter()
        .find(|d| d.id == id)
        .ok_or_else(|| AeroError::NotFound("digest subscription".into()))?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

/// `GET /api/digests` — the caller's own digest subscriptions, newest first.
/// Owner-scoped (no per-target access check needed — only the caller's rows are
/// returned).
async fn list_digests(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let listed = repo(&s)
        .list_for(auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(listed).map_err(AeroError::from)?))
}

/// `DELETE /api/digests/:id` — cancel one of the caller's own digest
/// subscriptions. Owner-scoped: a `404` if it isn't the caller's (someone else's,
/// currently holding an occurrence lease, or unknown). A failed occurrence
/// waiting for retry can still be deleted.
async fn delete_digest(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_digest(&id_str)?;
    let deleted = repo(&s)
        .delete(id, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    if !deleted {
        return Err(AeroError::NotFound(format!("digest subscription {id}")).into());
    }
    Ok(Json(serde_json::json!({ "deleted": true })))
}

/// Assert the caller is an effective workspace member, including account,
/// deactivation, and mandatory-2FA gates.
async fn assert_workspace_member(
    s: &AppState,
    workspace: WorkspaceId,
    caller: ParticipantId,
) -> Result<(), AeroError> {
    s.workspaces
        .effective_member_role(workspace, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))?;
    Ok(())
}

async fn settle_failure(
    repo: &DigestSubscriptionRepo,
    claim: &DigestDeliveryClaim,
    error: &str,
    retryable: bool,
) {
    let retry_at = time::OffsetDateTime::now_utc() + retry_backoff(claim.attempt);
    match repo
        .record_failure(
            claim.subscription.id,
            claim.claim_token,
            claim.delivery_key,
            claim.attempt,
            retry_at,
            error,
            retryable,
            MAX_ATTEMPTS,
        )
        .await
    {
        Ok(DigestFailureDisposition::RetryScheduled) => tracing::warn!(
            digest_id = %claim.subscription.id,
            attempt = claim.attempt,
            retry_at = %retry_at,
            error,
            "digest dispatcher: occurrence re-parked"
        ),
        Ok(DigestFailureDisposition::Dead) => tracing::error!(
            digest_id = %claim.subscription.id,
            attempt = claim.attempt,
            retryable,
            error,
            "digest dispatcher: subscription disabled after terminal delivery failure"
        ),
        Ok(DigestFailureDisposition::FenceLost) => tracing::warn!(
            digest_id = %claim.subscription.id,
            attempt = claim.attempt,
            "digest dispatcher: failure settlement lost claim fence"
        ),
        Err(settle_error) => tracing::warn!(
            error = ?settle_error,
            digest_id = %claim.subscription.id,
            "digest dispatcher: failure settlement failed; lease will be reclaimed"
        ),
    }
}

async fn prepare_summary(
    ai: &dyn AiBackend,
    rooms: &aero_storage::RoomRepo,
    claim: &DigestDeliveryClaim,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<String, (String, bool)> {
    if let Some(summary) = claim.prepared_summary.as_ref() {
        return Ok(summary.clone());
    }
    let workspace = match claim.subscription.target {
        DigestTarget::Room(room) => rooms
            .room_workspace(room)
            .await
            .map_err(|error| (error.to_string(), true))?
            .map(|value| value.to_uuid()),
        DigestTarget::Workspace(workspace) => Some(workspace.to_uuid()),
    };
    let target_fingerprint = match claim.subscription.target {
        DigestTarget::Room(room) => format!("room:{room}"),
        DigestTarget::Workspace(workspace) => format!("workspace:{workspace}"),
    };
    let fingerprint = format!(
        "digest:{}:{}:{target_fingerprint}",
        claim.subscription.id, claim.delivery_key
    );
    let usage_context = aero_ai::usage::UsageContext::for_request(
        &claim.delivery_key.to_string(),
        claim.subscription.participant_id.to_uuid(),
        &fingerprint,
        workspace,
    );
    let generate = async {
        match claim.subscription.target {
            DigestTarget::Room(room) => {
                ai.summarize_room_with_usage_context(room, DIGEST_WINDOW, usage_context)
                    .await
            }
            DigestTarget::Workspace(workspace) => {
                ai.summarize_workspace_with_usage_context(
                    claim.subscription.participant_id,
                    workspace,
                    DIGEST_WINDOW,
                    usage_context,
                )
                .await
            }
        }
    };
    let summary = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(("dispatcher shutdown".into(), true)),
        result = tokio::time::timeout(DELIVERY_TIMEOUT, generate) => match result {
            Ok(Ok(summary)) => summary,
            Ok(Err(error)) => return Err((error, true)),
            Err(_) => return Err((
                format!(
                    "summary generation timed out after {}s",
                    DELIVERY_TIMEOUT.as_secs()
                ),
                true,
            )),
        }
    };
    if summary.trim().is_empty() {
        return Err(("AI returned an empty digest summary".into(), true));
    }
    Ok(summary)
}

async fn deliver_claim(
    im: &Arc<ImService>,
    ai: &dyn AiBackend,
    rooms: &aero_storage::RoomRepo,
    repo: &DigestSubscriptionRepo,
    claim: DigestDeliveryClaim,
    cancel: &tokio_util::sync::CancellationToken,
) {
    let summary = match prepare_summary(ai, rooms, &claim, cancel).await {
        Ok(summary) => summary,
        Err((error, retryable)) => {
            settle_failure(repo, &claim, &error, retryable).await;
            return;
        }
    };
    if claim.prepared_summary.is_none() {
        match repo
            .save_prepared_summary(
                claim.subscription.id,
                claim.claim_token,
                claim.delivery_key,
                claim.attempt,
                &summary,
            )
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(
                    digest_id = %claim.subscription.id,
                    "digest dispatcher: prepared-summary write lost claim fence"
                );
                return;
            }
            Err(error) => {
                // The write may have committed before the connection failed.
                // Retain the lease and let an idempotent reclaim inspect it.
                tracing::warn!(
                    ?error,
                    digest_id = %claim.subscription.id,
                    "digest dispatcher: prepared-summary write failed; lease will be reclaimed"
                );
                return;
            }
        }
    }

    let delivery = match claim.subscription.target {
        DigestTarget::Room(room) => {
            let blocks = vec![Block::text(format!(
                "📰 定时摘要 ({}):\n{summary}",
                claim.subscription.frequency
            ))];
            let hash = match digest_message_hash(room, &blocks) {
                Ok(hash) => hash,
                Err(error) => {
                    settle_failure(repo, &claim, &error.to_string(), false).await;
                    return;
                }
            };
            let send = im.send_message_idempotent(
                claim.subscription.participant_id,
                room,
                blocks,
                None,
                None,
                claim.delivery_key,
                hash,
            );
            tokio::select! {
                biased;
                () = cancel.cancelled() => Err(("dispatcher shutdown".to_owned(), true)),
                result = tokio::time::timeout(DELIVERY_TIMEOUT, send) => match result {
                    Ok(Ok(_)) => Ok(()),
                    Ok(Err(error)) => {
                        let retryable = send_error_retryable(&error);
                        Err((error.to_string(), retryable))
                    }
                    Err(_) => Err((
                        format!(
                            "room delivery timed out after {}s",
                            DELIVERY_TIMEOUT.as_secs()
                        ),
                        true,
                    )),
                }
            }
        }
        DigestTarget::Workspace(workspace) => {
            let insert = repo.insert_workspace_delivery(
                claim.subscription.participant_id,
                workspace,
                claim.delivery_key,
                &summary,
            );
            tokio::select! {
                biased;
                () = cancel.cancelled() => Err(("dispatcher shutdown".to_owned(), true)),
                result = tokio::time::timeout(DELIVERY_TIMEOUT, insert) => match result {
                    Ok(Ok(
                        WorkspaceDigestDelivery::Inserted
                        | WorkspaceDigestDelivery::AlreadyDelivered
                    )) => Ok(()),
                    Ok(Ok(WorkspaceDigestDelivery::AccessRevoked)) => Err((
                        "workspace digest access revoked before final delivery".to_owned(),
                        false,
                    )),
                    Ok(Err(error)) => Err((error.to_string(), true)),
                    Err(_) => Err((
                        format!(
                            "workspace delivery timed out after {}s",
                            DELIVERY_TIMEOUT.as_secs()
                        ),
                        true,
                    )),
                }
            }
        }
    };
    if let Err((error, retryable)) = delivery {
        settle_failure(repo, &claim, &error, retryable).await;
        return;
    }

    let sent_at = time::OffsetDateTime::now_utc();
    let Some(next) = digest_next_run_at(&claim.subscription.frequency, sent_at) else {
        settle_failure(
            repo,
            &claim,
            &format!("unknown frequency {}", claim.subscription.frequency),
            false,
        )
        .await;
        return;
    };
    match repo
        .confirm_sent(
            claim.subscription.id,
            claim.claim_token,
            claim.delivery_key,
            claim.attempt,
            next,
            sent_at,
        )
        .await
    {
        Ok(true) => {}
        Ok(false) => tracing::warn!(
            digest_id = %claim.subscription.id,
            attempt = claim.attempt,
            "digest dispatcher: success confirmation lost claim fence"
        ),
        Err(error) => tracing::warn!(
            ?error,
            digest_id = %claim.subscription.id,
            "digest dispatcher: success confirmation failed; lease will be reclaimed"
        ),
    }
}

/// Leased digest worker. Failures re-park the same occurrence without advancing
/// `next_run_at`; tracked cancellation stops new claims and settles interrupted
/// work for retry.
pub async fn run_digest_dispatcher(state: AppState, cancel: tokio_util::sync::CancellationToken) {
    let repo = DigestSubscriptionRepo::new(state.pg.clone());
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tracing::info!("digest dispatcher started");

    // No AI backend → nothing to summarize; the dispatcher idles (still honoring
    // shutdown) rather than busy-looping over rows it can't process.
    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                tracing::info!("digest dispatcher shutting down");
                return;
            }
            _ = tick.tick() => {}
        }

        let Some(ai) = state.ai.as_ref() else {
            continue;
        };

        let now = time::OffsetDateTime::now_utc();
        let due = match repo
            .claim_due_with_policy(now, DELIVERY_LEASE, MAX_ATTEMPTS, CLAIM_BATCH)
            .await
        {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = ?e, "digest dispatcher: claim_due failed");
                continue;
            }
        };
        if due.is_empty() {
            continue;
        }
        tracing::debug!(
            count = due.len(),
            "digest dispatcher: firing due subscriptions"
        );

        stream::iter(due)
            .for_each_concurrent(DELIVERY_CONCURRENCY, |claim| {
                deliver_claim(&state.im, ai.as_ref(), &state.rooms, &repo, claim, &cancel)
            })
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_delivery_hash_is_stable_and_summary_bound() {
        let room = RoomId::new();
        let blocks = vec![Block::text("summary")];
        assert_eq!(
            digest_message_hash(room, &blocks).unwrap(),
            digest_message_hash(room, &blocks).unwrap()
        );
        assert_ne!(
            digest_message_hash(room, &blocks).unwrap(),
            digest_message_hash(room, &[Block::text("different")]).unwrap()
        );
    }

    #[test]
    fn digest_retry_backoff_is_exponential_and_capped() {
        assert_eq!(retry_backoff(1), time::Duration::seconds(30));
        assert_eq!(retry_backoff(2), time::Duration::seconds(60));
        assert_eq!(retry_backoff(3), time::Duration::seconds(120));
        assert_eq!(retry_backoff(99), time::Duration::seconds(3600));
    }

    #[test]
    fn permanent_access_errors_do_not_retry_forever() {
        assert!(!send_error_retryable(&AeroError::Forbidden(
            "membership revoked".into()
        )));
        assert!(!send_error_retryable(&AeroError::NotFound(
            "room removed".into()
        )));
        assert!(send_error_retryable(&AeroError::RateLimited));
        assert!(send_error_retryable(&AeroError::Upstream("down".into())));
    }
}
