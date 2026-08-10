//! Per-tenant AI usage route plus crash-recoverable outbox relay.
//!
//! Paid paths await a fenced `PostgreSQL` reservation before contacting a provider.
//! Success finalizes the actual charge, ordinary failure cancels it, and expired
//! ambiguity is conservatively charged. A leased background relay moves ready
//! events to the query-compatible ledger without a lossy in-memory channel.

use std::str::FromStr;
use std::time::Duration;

use aero_ai::usage::{
    UsageEvent, UsageOutcome, UsagePersistOutcome, UsageReservation, UsageReserveOutcome, UsageSink,
};
use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, WorkspaceId, WorkspaceRole};
use aero_storage::{
    AiUsageRepo, QueryConsistency, UsageCharge, UsageFinalizeOutcome,
    UsageOutcome as StoredUsageOutcome, UsageReservation as StoredUsageReservation,
    UsageReserveOutcome as StoredReserveOutcome,
};
use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::error::ApiResult;
use crate::state::AppState;

/// AI-usage route, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/workspaces/:id/ai-usage", get(ai_usage))
}

pub const AI_USAGE_OUTBOX_BACKLOG: &str = "aero_ai_usage_outbox_backlog";
pub const AI_USAGE_RESERVATIONS_RECOVERED_TOTAL: &str =
    "aero_ai_usage_reservations_recovered_total";

/// Build a stable accounting root when the caller supplies `Idempotency-Key`.
/// Without that explicit contract, each HTTP request is a distinct billable
/// operation. The fingerprint must include endpoint, resource, and paid-input
/// fields; it is hashed into a UUID and never persisted as plaintext. Completed
/// provider operations replay their minimal durable result, so losing the HTTP
/// response does not trigger another paid call or an `AlreadyFinalized` dead end.
#[must_use]
pub fn request_usage_context(
    headers: &HeaderMap,
    actor: ParticipantId,
    workspace: Option<uuid::Uuid>,
    fingerprint: &str,
) -> aero_ai::usage::UsageContext {
    headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|key| !key.is_empty() && key.len() <= 256)
        .map_or_else(
            || aero_ai::usage::UsageContext::new(workspace),
            |key| {
                aero_ai::usage::UsageContext::for_request(
                    key,
                    actor.to_uuid(),
                    fingerprint,
                    workspace,
                )
            },
        )
}

pub async fn room_request_usage_context(
    state: &AppState,
    headers: &HeaderMap,
    actor: ParticipantId,
    room: aero_common::RoomId,
    fingerprint: &str,
) -> Result<aero_ai::usage::UsageContext, AeroError> {
    let workspace = state
        .rooms
        .room_workspace(room)
        .await
        .map_err(AeroError::from)?
        .map(|value| value.to_uuid());
    Ok(request_usage_context(
        headers,
        actor,
        workspace,
        fingerprint,
    ))
}

#[derive(Clone)]
pub struct PgUsageSink {
    repo: AiUsageRepo,
    reservation_lease: time::Duration,
}

impl PgUsageSink {
    #[must_use]
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self {
            repo: AiUsageRepo::new(pool),
            reservation_lease: env_seconds("AERO__SERVER__AI_USAGE_RESERVATION_SECS")
                .map_or(time::Duration::minutes(5), time::Duration::seconds),
        }
    }
}

#[async_trait::async_trait]
impl UsageSink for PgUsageSink {
    async fn reserve(&self, event: UsageEvent) -> Result<UsageReserveOutcome, String> {
        let cost_micros = i64::try_from(event.micros)
            .map_err(|_| format!("AI cost exceeds BIGINT for usage {}", event.usage_id))?;
        let outcome = self
            .repo
            .reserve(
                &UsageCharge {
                    usage_id: event.usage_id,
                    workspace_id: event.workspace,
                    kind: event.kind,
                    cost_micros,
                    outcome_kind: event.outcome_kind,
                },
                time::OffsetDateTime::now_utc(),
                self.reservation_lease,
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(match outcome {
            StoredReserveOutcome::Reserved(reservation) => {
                UsageReserveOutcome::Acquired(UsageReservation {
                    usage_id: reservation.usage_id,
                    token: reservation.reservation_token,
                })
            }
            StoredReserveOutcome::InFlight => UsageReserveOutcome::InFlight,
            StoredReserveOutcome::Finalized(outcome) => {
                UsageReserveOutcome::AlreadyFinalized(outcome.map(|outcome| UsageOutcome {
                    kind: outcome.kind,
                    payload: outcome.payload,
                }))
            }
            StoredReserveOutcome::Recovered => {
                aero_common::metrics::global()
                    .inc_counter(AI_USAGE_RESERVATIONS_RECOVERED_TOTAL, 1);
                warn!(
                    usage_id = %event.usage_id,
                    "expired AI provider reservation conservatively finalized during retry"
                );
                UsageReserveOutcome::AlreadyFinalized(None)
            }
        })
    }

    async fn finalize(
        &self,
        reservation: UsageReservation,
        actual_micros: u64,
        outcome: Option<UsageOutcome>,
    ) -> Result<UsagePersistOutcome, String> {
        let cost_micros = i64::try_from(actual_micros)
            .map_err(|_| format!("AI cost exceeds BIGINT for usage {}", reservation.usage_id))?;
        let stored_outcome = outcome.map(|outcome| StoredUsageOutcome {
            kind: outcome.kind,
            payload: outcome.payload,
        });
        let outcome = self
            .repo
            .finalize(
                StoredUsageReservation {
                    usage_id: reservation.usage_id,
                    reservation_token: reservation.token,
                },
                cost_micros,
                stored_outcome.as_ref(),
                time::OffsetDateTime::now_utc(),
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(match outcome {
            UsageFinalizeOutcome::Finalized => UsagePersistOutcome::Inserted,
            UsageFinalizeOutcome::Duplicate => UsagePersistOutcome::Duplicate,
        })
    }

    async fn cancel(&self, reservation: UsageReservation) -> Result<bool, String> {
        self.repo
            .cancel(
                StoredUsageReservation {
                    usage_id: reservation.usage_id,
                    reservation_token: reservation.token,
                },
                time::OffsetDateTime::now_utc(),
            )
            .await
            .map_err(|error| error.to_string())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct UsageWorkerConfig {
    pub poll_interval: Duration,
    pub lease: time::Duration,
    pub batch_size: i64,
    pub shutdown_drain: Duration,
}

impl Default for UsageWorkerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(250),
            lease: time::Duration::seconds(30),
            batch_size: 256,
            shutdown_drain: Duration::from_secs(10),
        }
    }
}

impl UsageWorkerConfig {
    #[must_use]
    pub fn from_env() -> Self {
        let default = Self::default();
        Self {
            poll_interval: env_millis("AERO__SERVER__AI_USAGE_OUTBOX_POLL_MS")
                .map_or(default.poll_interval, Duration::from_millis),
            lease: env_seconds("AERO__SERVER__AI_USAGE_OUTBOX_LEASE_SECS")
                .map_or(default.lease, time::Duration::seconds),
            batch_size: std::env::var("AERO__SERVER__AI_USAGE_OUTBOX_BATCH")
                .ok()
                .and_then(|value| value.parse::<i64>().ok())
                .filter(|value| *value > 0)
                .unwrap_or(default.batch_size)
                .clamp(1, 500),
            shutdown_drain: env_seconds("AERO__SERVER__AI_USAGE_SHUTDOWN_DRAIN_SECS")
                .map_or(default.shutdown_drain, |seconds| {
                    Duration::from_secs(u64::try_from(seconds).unwrap_or(10))
                }),
        }
    }
}

fn env_millis(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()?
        .parse::<u64>()
        .ok()
        .filter(|value| *value >= 10)
}

fn env_seconds(name: &str) -> Option<i64> {
    std::env::var(name)
        .ok()?
        .parse::<i64>()
        .ok()
        .filter(|value| *value > 0)
}

/// Run the durable outbox-to-ledger relay until cancellation. On shutdown it
/// drains all currently-due rows within a bounded grace window; anything left is
/// still durable and reclaimable by the next process.
pub async fn run_usage_ledger_worker(
    repo: AiUsageRepo,
    config: UsageWorkerConfig,
    cancel: CancellationToken,
) {
    let registry = aero_common::metrics::global();
    registry.register_help(
        AI_USAGE_OUTBOX_BACKLOG,
        aero_common::metrics::MetricKind::Gauge,
        "Paid AI usage events durably queued but not yet in the usage ledger.",
    );
    registry.register_help(
        AI_USAGE_RESERVATIONS_RECOVERED_TOTAL,
        aero_common::metrics::MetricKind::Counter,
        "Expired AI provider reservations conservatively finalized after ambiguous outcomes.",
    );
    let mut tick = tokio::time::interval(config.poll_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                let drained = tokio::time::timeout(
                    config.shutdown_drain,
                    drain_due(&repo, config),
                ).await;
                match drained {
                    Ok(Ok(count)) => info!(count, "AI usage outbox shutdown drain complete"),
                    Ok(Err(error)) => warn!(?error, "AI usage outbox shutdown drain failed; rows remain durable"),
                    Err(_) => warn!("AI usage outbox shutdown drain timed out; rows remain durable"),
                }
                sample_backlog(&repo).await;
                return;
            }
            _ = tick.tick() => {
                if let Err(error) = dispatch_batch(&repo, config).await {
                    warn!(?error, "AI usage outbox relay failed; leased rows remain reclaimable");
                }
                sample_backlog(&repo).await;
            }
        }
    }
}

async fn drain_due(repo: &AiUsageRepo, config: UsageWorkerConfig) -> Result<usize, sqlx::Error> {
    let mut total = 0;
    loop {
        let count = dispatch_batch(repo, config).await?;
        total += count;
        if count == 0 {
            return Ok(total);
        }
    }
}

/// Settle one claimed batch. A database error never removes a row: the current
/// token is re-parked with backoff when possible, otherwise lease expiry makes it
/// reclaimable.
pub async fn dispatch_batch(
    repo: &AiUsageRepo,
    config: UsageWorkerConfig,
) -> Result<usize, sqlx::Error> {
    let now = time::OffsetDateTime::now_utc();
    let recovered = repo
        .recover_expired_reservations(now, config.batch_size)
        .await?;
    for charge in &recovered {
        aero_common::metrics::global().inc_counter(AI_USAGE_RESERVATIONS_RECOVERED_TOTAL, 1);
        warn!(
            usage_id = %charge.usage_id,
            workspace_id = ?charge.workspace_id,
            kind = %charge.kind,
            estimated_cost_micros = charge.cost_micros,
            "expired AI provider reservation conservatively finalized"
        );
    }
    let claims = repo.claim_due(now, config.lease, config.batch_size).await?;
    let count = recovered.len() + claims.len();
    for claim in claims {
        match repo.settle(claim.usage_id, claim.claim_token).await {
            Ok(true) => {}
            Ok(false) => {
                warn!(
                    usage_id = %claim.usage_id,
                    "AI usage settlement lost its lease fence; successor owns the row"
                );
            }
            Err(error) => {
                let parked = repo
                    .mark_failed(
                        claim.usage_id,
                        claim.claim_token,
                        claim.attempts,
                        time::OffsetDateTime::now_utc(),
                        &error.to_string(),
                    )
                    .await;
                if let Err(park_error) = parked {
                    warn!(
                        usage_id = %claim.usage_id,
                        ?error,
                        ?park_error,
                        "AI usage settlement and re-park both failed; lease expiry will reclaim"
                    );
                }
            }
        }
    }
    Ok(count)
}

async fn sample_backlog(repo: &AiUsageRepo) {
    match repo.pending_count().await {
        Ok(count) => {
            #[allow(clippy::cast_precision_loss)]
            aero_common::metrics::global().set_gauge(AI_USAGE_OUTBOX_BACKLOG, count as f64);
        }
        Err(error) => warn!(?error, "AI usage outbox backlog sample failed"),
    }
}

#[derive(Deserialize)]
struct UsageQuery {
    /// Look-back window in seconds (default 30 days, clamped to [1s, 366d]).
    #[serde(default)]
    since_secs: Option<i64>,
}

/// `GET /api/workspaces/:id/ai-usage` — per-kind AI cost rollup for the tenant over
/// a look-back window. Workspace-admin gated (chargeback / spend visibility).
async fn ai_usage(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = WorkspaceId::from_str(ws_str.trim())
        .map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))?;
    assert_admin(&s, ws, auth.participant_id).await?;

    let since_secs = q
        .since_secs
        .unwrap_or(30 * 24 * 3600)
        .clamp(1, 366 * 24 * 3600);
    let since = time::OffsetDateTime::now_utc() - time::Duration::seconds(since_secs);
    // The ledger aggregate is workspace-wide and feeds chargeback decisions.
    // Express its strong contract through QueryRouter; only already-authorized
    // single-room historical reads may use the replica.
    let by_kind = AiUsageRepo::new(s.query_router.repo_pool(QueryConsistency::Strong))
        .summary_since(ws.to_uuid(), since)
        .await
        .map_err(AeroError::from)?;
    let total_micros: i64 = by_kind.iter().map(|k| k.cost_micros).sum();
    Ok(Json(serde_json::json!({
        "workspace_id": ws,
        "since_secs": since_secs,
        "total_micros": total_micros,
        "by_kind": by_kind,
    })))
}

/// Resolve the caller's workspace role and require admin (Owner/Admin), mirroring
/// `ip_allowlist::assert_admin`.
async fn assert_admin(
    s: &AppState,
    ws: WorkspaceId,
    caller: ParticipantId,
) -> Result<(), AeroError> {
    let role: WorkspaceRole = s
        .workspaces
        .effective_member_role(ws, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))?;
    if role.can_administer() {
        Ok(())
    } else {
        Err(AeroError::Forbidden("workspace admin required".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_idempotency_key_stabilizes_only_matching_request_fingerprint() {
        let actor = ParticipantId::new();
        let workspace = Some(uuid::Uuid::new_v4());
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "retry-42".parse().unwrap());
        let first = request_usage_context(&headers, actor, workspace, "ask:room:question");
        assert_eq!(
            first,
            request_usage_context(&headers, actor, workspace, "ask:room:question")
        );
        assert_ne!(
            first,
            request_usage_context(&headers, actor, workspace, "ask:room:changed")
        );
    }

    #[test]
    fn requests_without_idempotency_key_are_distinct_billable_operations() {
        let headers = HeaderMap::new();
        let actor = ParticipantId::new();
        let first = request_usage_context(&headers, actor, None, "rewrite:text");
        let second = request_usage_context(&headers, actor, None, "rewrite:text");
        assert_ne!(first.root_id, second.root_id);
    }
}
