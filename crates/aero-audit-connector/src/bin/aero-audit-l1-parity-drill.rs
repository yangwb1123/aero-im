//! L1 parity drill (AC4 arbiter) — the non-vacuous trigger-presence +
//! double-count check for migration 0242's `aero_enqueue_l1_aggregate_audit`.
//!
//! Usage (after `aero-cli migrate` on a disposable database):
//!
//! ```text
//! DATABASE_URL=postgres://…/aero_l1_parity_$$ cargo run -p aero-audit-connector \
//!     --bin aero-audit-l1-parity-drill
//! ```
//!
//! Unlike the T-11/relay drills (which seed the outbox directly and test row
//! *shapes*), this drill seeds its input THROUGH THE TRIGGER: it inserts N ≥ 1
//! `message.create` rows into `audit_events` (fixed `created_at`, captured
//! once) and lets 0242 fire per row. Non-vacuity: N ≥ 1 ⇒ `COUNT(mapped) ≥ 1`,
//! so a dropped trigger yields `SUM = 0 ≠ COUNT = N` → red — the vacuous
//! `0 == 0` pass is impossible.
//!
//! Steps:
//!   1. Self-isolating start (T-11 precedent): TRUNCATE the outbox; delete the
//!      drill's own audit rows; fixture workspace + participant inserts.
//!   2. Self-seed N `message.create` rows (`AERO_AUDIT_DRILL_ROWS`, default 5)
//!      with one fixed `created_at` → exactly 1 window row, count = N.
//!   3. Parity query — ALL message-class rows (window + spill): the SUM side
//!      selects `class = 'message' AND (payload->>'aggregated' = 'true' OR
//!      payload->>'spill' = 'true')` — the `spill` marker is load-bearing (a
//!      marker-less spill is invisible to the SUM side → red). The COUNT side
//!      selects `audit_events` rows whose action is in the allowlist. Both
//!      sides are window-start-scoped to the retention cutoff
//!      (`floor(epoch(created_at)/60)*60 >= cutoff`, cutoff = now −
//!      `AERO__SERVER__AUDIT_RETENTION_DAYS`, default 365 — matching
//!      `boot/retention.rs`; the outbox is never swept, so un-scoped outbox
//!      rows would false-red against swept audit rows; window-start on BOTH
//!      sides keeps straddling windows consistent).
//!   4. Spill leg: force the window row `status = 2` (delivered), insert one
//!      more `message.create` row WITH THE SAME fixed `created_at` (same
//!      window — a different window would create a new window row, not a
//!      spill) → exactly 1 spill row (`spill = 'true'`, `count = 1`, own
//!      deterministic key, `payload.event_id` = own PK). Re-run the parity
//!      query → `SUM = N+1 == COUNT = N+1`.
//!
//! Exit 0 PASS / non-zero FAIL naming the diverging lane + window / exit 2
//! SKIP when `audit_governance_outbox` or `aero_enqueue_l1_aggregate_audit`
//! is absent (drill precedent; the harness leg is 0242-file-gated anyway).

use aero_common::{
    AGGREGATED_MESSAGE_ACTION, AUDIT_SOURCE_SYSTEM, GOVERNANCE_CLASS_MESSAGE,
    LOCAL_ACTION_MESSAGE_CREATE, LOCAL_ACTION_MESSAGE_EDIT,
};
use anyhow::Context;
use sqlx::PgPool;
use uuid::Uuid;

fn main() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build drill runtime")?;
    runtime.block_on(run())
}

async fn run() -> anyhow::Result<()> {
    let url = std::env::var("DATABASE_URL").context("DATABASE_URL is required")?;
    let rows = std::env::var("AERO_AUDIT_DRILL_ROWS")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value >= 1)
        .unwrap_or(5);
    let retention_days = std::env::var("AERO__SERVER__AUDIT_RETENTION_DAYS")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(365);
    let pool = PgPool::connect(&url)
        .await
        .context("connect to the drill database")?;

    let table: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('audit_governance_outbox')::text")
            .fetch_one(&pool)
            .await
            .context("probe for the 0239 governance outbox")?;
    if table.is_none() {
        eprintln!(
            "SKIP: audit_governance_outbox (B5-1 0239) is not migrated; \
             the drill cannot run until it lands"
        );
        std::process::exit(2);
    }
    let probe: Option<String> =
        sqlx::query_scalar("SELECT to_regprocedure('aero_enqueue_l1_aggregate_audit()')::text")
            .fetch_one(&pool)
            .await
            .context("probe for the 0242 function")?;
    if probe.is_none() {
        eprintln!(
            "SKIP: 0242 not migrated (aero_enqueue_l1_aggregate_audit missing) — \
             the L1 trigger cannot be exercised"
        );
        std::process::exit(2);
    }

    // Self-isolating start (T-11 TRUNCATE-at-start precedent): the harness
    // grants each drill its own throwaway DB, but a shared-DB run must never
    // corrupt the count/parity invariants.
    sqlx::query("TRUNCATE audit_governance_outbox")
        .execute(&pool)
        .await
        .context("reset the governance outbox (TRUNCATE-at-start guard)")?;
    let ws = Uuid::new_v4();
    let actor = Uuid::new_v4();
    sqlx::query("DELETE FROM audit_events WHERE workspace_id = $1")
        .bind(ws)
        .execute(&pool)
        .await
        .context("delete the drill's own audit rows")?;
    println!("reset audit_governance_outbox + drill audit rows (self-isolating start)");

    // Fixture: workspace + participant (audit_events FK targets).
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(actor)
        .bind(format!("l1-parity-{ws}"))
        .execute(&pool)
        .await
        .context("insert drill participant")?;
    sqlx::query(
        "INSERT INTO workspaces (id, name, slug, created_by, created_at)
         VALUES ($1, $2, $3, $4, now())",
    )
    .bind(ws)
    .bind("L1 Parity Drill WS")
    .bind(format!("l1-parity-{ws}"))
    .bind(actor)
    .execute(&pool)
    .await
    .context("insert drill workspace")?;

    // Fixed created_at captured ONCE: every seeded row lands in the SAME
    // window (single clock domain, server-stamped semantics).
    let fixed_ts: time::OffsetDateTime =
        sqlx::query_scalar("SELECT now()").fetch_one(&pool).await.context("capture fixed ts")?;

    // --- Self-seed through the trigger (non-vacuity) ---
    for _ in 0..rows {
        sqlx::query(
            "INSERT INTO audit_events (id, workspace_id, actor_id, action, detail, created_at)
             VALUES ($1, $2, $3, $4, '{}'::jsonb, $5)",
        )
        .bind(Uuid::new_v4())
        .bind(ws)
        .bind(actor)
        .bind(LOCAL_ACTION_MESSAGE_CREATE)
        .bind(fixed_ts)
        .execute(&pool)
        .await
        .context("self-seed message.create through the 0242 trigger")?;
    }
    println!("self-seeded {rows} message.create rows through the trigger (fixed created_at)");

    let cutoff_epoch = retention_cutoff_epoch(&pool, retention_days).await?;
    let (sum, count) = parity(&pool, &ws, cutoff_epoch).await?;
    if sum != count {
        anyhow::bail!(
            "parity broken after self-seed: SUM(count) = {sum} != COUNT(mapped audit) = {count} \
             — a dropped/mis-allowlisted 0242 trigger or a drifted envelope marker"
        );
    }
    if sum != rows {
        anyhow::bail!(
            "expected exactly one window row with count = {rows}; parity sum is {sum} \
             (the trigger must aggregate N rows into one window row)"
        );
    }
    println!("parity after self-seed: SUM(count) = {sum} == COUNT(mapped) = {count} == {rows}");

    // --- Spill leg: force the window row delivered (status 2), then a late
    // same-window row → exactly one spill row (own key, own markers). ---
    let window_row: (Uuid, i64, i64) = sqlx::query_as(
        "SELECT event_id, (payload->>'count')::bigint,
                floor(extract(epoch FROM (payload->>'window_start')::timestamptz) / 60)::bigint
           FROM audit_governance_outbox
          WHERE class = 'message' AND (payload->>'aggregated') = 'true'",
    )
    .fetch_one(&pool)
    .await
    .context("read the window row")?;
    let (window_key, window_count, window_epoch) = window_row;
    if window_count != rows {
        anyhow::bail!(
            "window row count {window_count} != seeded {rows} — the merge arithmetic drifted",
        );
    }
    sqlx::query("UPDATE audit_governance_outbox SET status = 2 WHERE event_id = $1")
        .bind(window_key)
        .execute(&pool)
        .await
        .context("force the window row delivered (status 2)")?;

    let late_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO audit_events (id, workspace_id, actor_id, action, detail, created_at)
         VALUES ($1, $2, $3, $4, '{}'::jsonb, $5)",
    )
    .bind(late_id)
    .bind(ws)
    .bind(actor)
    .bind(LOCAL_ACTION_MESSAGE_CREATE)
    .bind(fixed_ts) // SAME window — a different window would create a new window row, not a spill
    .execute(&pool)
    .await
    .context("insert the late same-window row (must spill)")?;

    let spills: Vec<(Uuid, serde_json::Value)> = sqlx::query_as(
        "SELECT event_id, payload FROM audit_governance_outbox
          WHERE (payload->>'spill') = 'true'",
    )
    .fetch_all(&pool)
    .await
    .context("read spill rows")?;
    if spills.len() != 1 {
        anyhow::bail!(
            "expected exactly 1 spill row after the late same-window event, got {}",
            spills.len()
        );
    }
    let (spill_key, spill_payload) = &spills[0];
    // Spill fields: own deterministic key recomputed from the trigger's
    // preimage shape; own event_id/idempotency_key (settle contract); the
    // `spill: true` marker the parity SUM side discriminates on.
    let expected_spill: Uuid = sqlx::query_scalar("SELECT md5($1)::uuid")
        .bind(format!(
            "{ws}|{GOVERNANCE_CLASS_MESSAGE}|{window_epoch}|{late_id}"
        ))
        .fetch_one(&pool)
        .await
        .context("recompute spill key")?;
    if *spill_key != expected_spill {
        anyhow::bail!(
            "spill key {spill_key} != recomputed {expected_spill} (deterministic md5(v_key|'|'|event) drifted)"
        );
    }
    if spill_payload["count"] != serde_json::json!(1)
        || spill_payload["aggregated"] != serde_json::json!(true)
        || spill_payload["spill"] != serde_json::json!(true)
        || spill_payload["event_id"] != spill_key.to_string()
        || spill_payload["idempotency_key"] != spill_key.to_string()
        || spill_payload["source_system"] != AUDIT_SOURCE_SYSTEM
        || spill_payload["action"] != AGGREGATED_MESSAGE_ACTION
    {
        anyhow::bail!(
            "spill row payload drifted: {spill_payload:?}"
        );
    }

    let (sum, count) = parity(&pool, &ws, cutoff_epoch).await?;
    if sum != count {
        anyhow::bail!(
            "parity broken after spill leg: SUM(count) = {sum} != COUNT(mapped audit) = {count}"
        );
    }
    if sum != rows + 1 {
        anyhow::bail!(
            "spill leg must re-balance to SUM = COUNT = {rows} + 1; got {sum} \
             (the late event was either dropped or double-counted)"
        );
    }
    println!(
        "parity after spill leg: SUM(count) = {sum} == COUNT(mapped) = {count} \
         (window {window_key} delivered at count {rows} + spill {spill_key} count 1)"
    );
    println!("drill: l1-aggregation-parity: PASS");
    Ok(())
}

/// Retention cutoff as a window-start epoch (`floor(epoch/60)*60`), both
/// sides of the parity query scope to it (outbox is never swept — v1-parity
/// durable cursor; `boot/retention.rs` sweeps `audit_events` at
/// `created_at < now - AERO__SERVER__AUDIT_RETENTION_DAYS`, default 365).
async fn retention_cutoff_epoch(pool: &PgPool, retention_days: i64) -> anyhow::Result<i64> {
    let epoch: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM (now() - make_interval(days => $1)))",
    )
    .bind(retention_days)
    .fetch_one(pool)
    .await
    .context("retention cutoff")?;
    // window-start epoch (floor to the minute, both parity sides scope to
    // this grid) — the truncation is the intended semantic, not a bug.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let floored = ((epoch / 60.0).floor() as i64) * 60;
    Ok(floored)
}

/// The parity query — ALL message-class outbox rows (window `aggregated:
/// true` + spill `spill: true`; the spill marker is load-bearing) vs the
/// allowlisted audit rows, window-start-scoped on BOTH sides. Returns
/// (SUM(count), COUNT(audit)).
async fn parity(pool: &PgPool, ws: &Uuid, cutoff_epoch: i64) -> anyhow::Result<(i64, i64)> {
    let sum: i64 = sqlx::query_scalar(
        r"SELECT COALESCE(SUM((payload->>'count')::bigint), 0)::bigint
            FROM audit_governance_outbox
           WHERE class = 'message'
             AND ((payload->>'aggregated') = 'true' OR (payload->>'spill') = 'true')
             AND floor(extract(epoch FROM (payload->>'window_start')::timestamptz) / 60) * 60
                 >= $2",
    )
    .bind(ws.to_string())
    .bind(cutoff_epoch)
    .fetch_one(pool)
    .await
    .context("parity SUM")?;
    let count: i64 = sqlx::query_scalar(
        r"SELECT COUNT(*)::bigint
            FROM audit_events
           WHERE workspace_id = $1
             AND action IN ($2, $3)
             AND floor(extract(epoch FROM created_at) / 60) * 60 >= $4",
    )
    .bind(ws.to_string())
    .bind(LOCAL_ACTION_MESSAGE_CREATE)
    .bind(LOCAL_ACTION_MESSAGE_EDIT)
    .bind(cutoff_epoch)
    .fetch_one(pool)
    .await
    .context("parity COUNT")?;
    Ok((sum, count))
}
