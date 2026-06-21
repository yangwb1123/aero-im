//! Per-tenant AI usage: the `/api/workspaces/:id/ai-usage` summary endpoint + the
//! boot drain task that persists the ledger (ROADMAP 第六版 · 方向一·2).
//!
//! `aero_ai::metrics::charge_cost` fans every paid AI charge (worker queue + the
//! real-time moderation bot — its single convergence point) into a bounded channel;
//! [`run_usage_ledger_drain`] coalesces those events into batched inserts off the
//! hot path, and [`routes`] exposes an admin-gated per-kind cost rollup per tenant.

use std::str::FromStr;
use std::time::Duration;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, WorkspaceId, WorkspaceRole};
use aero_storage::{AiUsageRepo, UsageRow};
use axum::{
    extract::{Path, Query, State},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::error::ApiResult;
use crate::state::AppState;

/// AI-usage route, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/workspaces/:id/ai-usage", get(ai_usage))
}

/// Flush when this many events are queued (one multi-row INSERT), or…
const FLUSH_MAX: usize = 256;
/// …every this many seconds, whichever comes first.
const FLUSH_INTERVAL_SECS: u64 = 5;

/// Drain the usage-ledger channel into batched inserts. Coalesces a burst into one
/// `INSERT` (flush at [`FLUSH_MAX`] or every [`FLUSH_INTERVAL_SECS`]) so the AI hot
/// path never blocks on a per-charge round trip; flushes the remainder on shutdown.
/// Best-effort: an insert error drops that batch (the aggregate Prometheus counter
/// stays the alerting source of truth — the ledger is the historical billing view).
pub async fn run_usage_ledger_drain(
    repo: AiUsageRepo,
    mut rx: tokio::sync::mpsc::Receiver<aero_ai::metrics::UsageEvent>,
    cancel: CancellationToken,
) {
    let mut buf: Vec<UsageRow> = Vec::with_capacity(FLUSH_MAX);
    let mut tick = tokio::time::interval(Duration::from_secs(FLUSH_INTERVAL_SECS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                while let Ok(ev) = rx.try_recv() {
                    buf.push(to_row(ev));
                }
                flush(&repo, &mut buf).await;
                break;
            }
            _ = tick.tick() => flush(&repo, &mut buf).await,
            recv = rx.recv() => match recv {
                Some(ev) => {
                    buf.push(to_row(ev));
                    if buf.len() >= FLUSH_MAX {
                        flush(&repo, &mut buf).await;
                    }
                }
                None => {
                    // Sink dropped (boot teardown) — flush remainder + exit.
                    flush(&repo, &mut buf).await;
                    break;
                }
            },
        }
    }
}

fn to_row(ev: aero_ai::metrics::UsageEvent) -> UsageRow {
    UsageRow {
        workspace_id: ev.workspace,
        kind: ev.kind.to_owned(),
        cost_micros: i64::try_from(ev.micros).unwrap_or(i64::MAX),
    }
}

async fn flush(repo: &AiUsageRepo, buf: &mut Vec<UsageRow>) {
    if buf.is_empty() {
        return;
    }
    if let Err(e) = repo.insert_batch(buf).await {
        warn!(error = ?e, n = buf.len(), "ai-usage ledger: batch insert failed (batch dropped)");
    }
    buf.clear();
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

    let since_secs = q.since_secs.unwrap_or(30 * 24 * 3600).clamp(1, 366 * 24 * 3600);
    let since = time::OffsetDateTime::now_utc() - time::Duration::seconds(since_secs);
    let by_kind = AiUsageRepo::new(s.pg.clone())
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
        .member_role(ws, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))?;
    if role.can_administer() {
        Ok(())
    } else {
        Err(AeroError::Forbidden("workspace admin required".into()))
    }
}
