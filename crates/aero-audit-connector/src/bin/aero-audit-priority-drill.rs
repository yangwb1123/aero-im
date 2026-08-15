//! Moderation-priority drill — 500 backlog rows + 1 auth-token backlog row +
//! 1 moderation row; the moderation row must be claimed and delivered within
//! the first batch.
//!
//! Usage (after `aero-cli migrate` on a disposable database):
//!
//! ```text
//! DATABASE_URL=postgres://…/aero_priority_drill_$$ cargo run -p aero-audit-connector \
//!     --bin aero-audit-priority-drill
//! ```
//!
//! Seeds 500 backlog rows (priority = [`BACKLOG_PRIORITY`] = 10,
//! `GOVERNANCE_PRIORITY_BACKLOG` — production values verbatim, enqueued
//! FIRST / earlier `available_at`), 1 auth-token-shaped backlog row (class
//! 'message', priority 10, envelope carrying `action: "auth.login.failed"` —
//! the security-event-audit direction's backlog lane, enqueued among the
//! backlog) and 1 moderation row (`class = 'admin'`,
//! priority = [`MODERATION_PRIORITY`] = 100, `GOVERNANCE_PRIORITY_MODERATION`
//! = the 0239 trigger stamp, outbound action
//! [`MODERATION_OUTBOUND_ACTION`] (the leaf-single-sourced contract token),
//! enqueued LAST / later `available_at`), so any ordering evidence can only
//! come from priority, never FIFO. Drains with `batch_size = 100` (the first batch
//! cannot hold all 502 rows) / `concurrency = 1` (serial settle) against a
//! stub audit sink (202 + valid receipt), then asserts:
//!   - round 1: `COUNT(status=2) == 100` AND the moderation row is IN that
//!     set — batch membership (B5-3 decision D3: `UPDATE … FROM (CTE ORDER
//!     BY …) … RETURNING` emits target-table heap order, so delivery
//!     *firstness* is an executor artifact, not a contract; membership in
//!     the top-100 claimed set is the contract of the `priority DESC`
//!     ORDER BY, regardless of enqueue order);
//!   - full drain: `COUNT(status=2) == 502` and `event_id` set-parity (no
//!     orphans, no duplicates).
//!
//! Capability gate: the 0239 table AND its `priority`/`class` columns are
//! probed at runtime — absent → exit 2 SKIP (B5-1 DDL shape not landed).
//! Crucially, 0239 landing *without* B5-3's priority ordering is NOT a SKIP:
//! this drill then FAILS red — the honest G6 signal that B5-3's claim
//! ordering has not landed (design findings D3/F2/P4).
//!
//! Exits 2 with a clear message when the 0239 table is absent (B5-1 not yet
//! landed); the test-integration.sh section is gated on the migration file.
//!
//! Phase 2 (B5-3 D-CAP starvation leg): after the parity-502 drain, the
//! drill self-isolates again (same TRUNCATE gate), seeds a 600-row
//! priority-100 flood (earliest `available_at`) + 100 priority-10 backlog
//! rows (latest), and asserts the minimum-service floor — round-1 split
//! 95/5, per-round quota through round 5, then a full 700-row drain
//! (`starvation-round1-split-95-5` / `starvation-cross-batch-quota` /
//! `starvation-drain-700` PASS lines).

use std::sync::Arc;
use std::time::Duration;

use aero_audit_connector::client::AuditClient;
use aero_audit_connector::config::RelayConfig;
use aero_audit_connector::outbox::OutboxRepo;
use aero_audit_connector::pg::PgOutboxRepo;
use aero_audit_connector::relay::AuditRelay;
use aero_audit_connector::stub::StubSink;
use aero_common::model::audit::MODERATION_OUTBOUND_ACTION;
use anyhow::Context;
use reqwest::Url;
use serde_json::json;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

const MAX_ROUNDS: usize = 10;
/// Backlog lane priority = `aero_ai::governance::GOVERNANCE_PRIORITY_BACKLOG`
/// (governance.rs) = the 0239 column DEFAULT — production values verbatim.
/// Under B5-3's `ORDER BY priority DESC` (higher = more urgent) this is the
/// low lane.
const BACKLOG_PRIORITY: i64 = 10;
/// Moderation lane priority = `GOVERNANCE_PRIORITY_MODERATION` (governance.rs)
/// = the 0239 trigger stamp for `message.moderated` — strictly above the
/// backlog lane so it sorts first under `priority DESC`. Production values,
/// not [PROPOSED].
const MODERATION_PRIORITY: i64 = 100;
/// Outbound action carried by the moderation row — the single locked
/// contract token, imported from the leaf
/// (`aero_common::model::audit::MODERATION_OUTBOUND_ACTION`); one constant,
/// not a runtime choice.
const MODERATION_ACTION: &str = MODERATION_OUTBOUND_ACTION;

/// Contract item 3 outbound vocabulary (proposal:10, implementation-
/// gate.md:65; the leaf locks ONE constant — either spelling is contract-
/// legal). Hardcoded, NOT derived from the leaf: a leaf flip must not
/// auto-follow and silently kill the pin.
const MODERATION_OUTBOUND_ACTIONS: [&str; 2] = ["admin.content.flag", "admin.moderation.action"];

const BACKLOG_ROWS: i64 = 500;
/// Auth-token-shaped backlog rows (class 'message', priority 10, envelope
/// carrying `action: "auth.login.failed"`) — the security-event-audit
/// direction's backlog lane. N ≤ 499 (B3 pin): `MAX_ROUNDS=10` × batch 100 =
/// 1000 caps the drain, so 501+N ≤ 1000.
const AUTH_ROWS: i64 = 1;
const MODERATION_ROWS: i64 = 1;
const TOTAL_ROWS: i64 = BACKLOG_ROWS + AUTH_ROWS + MODERATION_ROWS;
const BATCH_SIZE: i64 = 100;

/// Phase 2 (D-CAP): admin flood size — exceeds `MAX_CLAIM = 500` so even
/// the maximum claim clamp cannot drain it in one round.
const STARVATION_ADMIN_ROWS: i64 = 600;
/// Phase 2: backlog lane size (the direction's "100 backlog rows").
const STARVATION_BACKLOG_ROWS: i64 = 100;
/// Phase 2: the per-tick minimum service floor at the drill's batch size —
/// `min_service_floor(BATCH_SIZE)` = 5 at batch 100. Asserted by the
/// `starvation_floor_matches_production_batch` unit test, never hardcoded
/// blindly (D8′ style).
const STARVATION_K: i64 = 5;

/// D8′ destructive-gate predicate shared by both phases: a non-empty
/// outbox requires the explicit opt-in `AERO_PRIORITY_DRILL_ALLOW_TRUNCATE
/// == "1"` (fail-closed — "true"/"yes"/absent refuse). Pure, so the gate
/// is unit-tested without a DB.
#[must_use]
fn truncate_gate_allows(n: i64, allow_truncate: Option<&str>) -> bool {
    n == 0 || allow_truncate == Some("1")
}

fn main() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build drill runtime")?;
    runtime.block_on(run())
}

async fn run() -> anyhow::Result<()> {
    // D8′ vocabulary pin (design: priority-drill-destructive-gate): bail red
    // BEFORE any destructive work if the leaf drifted outside the contract
    // pair — the pin must not auto-follow a leaf flip.
    if !MODERATION_OUTBOUND_ACTIONS.contains(&MODERATION_ACTION) {
        anyhow::bail!(
            "leaf MODERATION_OUTBOUND_ACTION {MODERATION_ACTION} is outside the contract \
             vocabulary {MODERATION_OUTBOUND_ACTIONS:?}"
        );
    }
    let url = std::env::var("DATABASE_URL").context("DATABASE_URL is required")?;
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
    // Self-isolating start (db-reviewer finding 3) + D8′ destructive gate
    // (design: priority-drill-destructive-gate; authoritative — the usage
    // header documents direct invocation, so the wrapper's psql COUNT is
    // fast-fail UX only and THIS lock+count+TRUNCATE is the gate that
    // cannot be skipped). ACCESS EXCLUSIVE blocks concurrent writers (incl.
    // the 0239 enqueue trigger's INSERT, which blocks inside its own
    // audit_events tx) so the count and the TRUNCATE are atomic; TRUNCATE is
    // transactional in PG — a refused run rolls back and the rows survive.
    let mut tx = pool.begin().await.context("begin drill gate tx")?;
    sqlx::query("LOCK TABLE audit_governance_outbox IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await
        .context("lock outbox for the drill gate")?;
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*)::bigint FROM audit_governance_outbox")
        .fetch_one(&mut *tx)
        .await
        .context("count outbox rows in the drill gate")?;
    if n > 0 && std::env::var("AERO_PRIORITY_DRILL_ALLOW_TRUNCATE").as_deref() != Ok("1") {
        tx.rollback().await?;
        eprintln!(
            "aero-audit-priority-drill: REFUSED — audit_governance_outbox has {n} row(s); \
             the drill TRUNCATEs the table. Re-run with \
             AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=1 (throwaway DB only)"
        );
        std::process::exit(1);
    }
    sqlx::query("TRUNCATE audit_governance_outbox")
        .execute(&mut *tx)
        .await
        .context("reset the governance outbox (TRUNCATE-at-start guard)")?;
    tx.commit().await?;
    println!("reset audit_governance_outbox (TRUNCATE-at-start guard)");
    // Runtime capability gate (design D3): the priority/class columns are
    // B5-1 DDL shape; the priority *ordering* is B5-3's claim query. Column
    // absence = SKIP (0239 shape not landed); ordering absence = FAIL below.
    for column in ["priority", "class"] {
        let present: bool = sqlx::query_scalar(
            r"SELECT EXISTS (
                 SELECT 1 FROM information_schema.columns
                  WHERE table_name = 'audit_governance_outbox' AND column_name = $1
             )",
        )
        .bind(column)
        .fetch_one(&pool)
        .await
        .context("probe for the priority/class columns")?;
        if !present {
            eprintln!(
                "SKIP: audit_governance_outbox is missing the '{column}' column; \
                 B5-1's 0239 DDL has not landed in its promised shape"
            );
            std::process::exit(2);
        }
    }

    let stub = StubSink::start().await.context("start stub sink")?;
    let config = RelayConfig {
        token_endpoint: Url::parse(&stub.token_url()).context("stub token URL")?,
        events_url: Url::parse(&stub.events_url()).context("stub events URL")?,
        resource: "audit-governance".into(),
        client_id: "drill-client".into(),
        client_secret: "0123456789abcdef0123456789abcdef".into(),
        expected_iss: "https://idp.example.test".into(),
        expected_aud: "audit-governance".into(),
        expected_scope: "audit:event:write".into(),
        expected_sub: "aero-im.source".into(),
        source_system: "aero-im.source".into(),
        request_timeout: Duration::from_secs(5),
        delivery_lease: Duration::from_secs(30),
        poll_interval: Duration::from_secs(1),
        shutdown_drain: Duration::from_secs(2),
        batch_size: BATCH_SIZE,
        concurrency: 1,
        jwks_uri: None,
    };

    // Seed backlog FIRST (earlier available_at), the auth-token backlog row
    // among them, moderation LAST (later available_at) — any ordering
    // evidence must come from priority.
    let mut seeded = Vec::new();
    for i in 0..BACKLOG_ROWS {
        let event_id = Uuid::new_v4();
        sqlx::query(
            r"INSERT INTO audit_governance_outbox
                    (event_id, payload, available_at, attempts, status, priority, class)
              VALUES ($1, $2, clock_timestamp(), 0, 0, $3, 'message')",
        )
        .bind(event_id)
        .bind(json!({
            "event_id": event_id.to_string(),
            "source_system": "aero-im.source",
        }))
        .bind(BACKLOG_PRIORITY)
        .execute(&pool)
        .await
        .with_context(|| format!("seed backlog row {i}"))?;
        seeded.push(event_id);
    }
    // Auth-token-shaped backlog rows (priority 10, class 'message', envelope
    // carrying `action: "auth.login.failed"`), enqueued strictly BEFORE the
    // moderation row — the auth backlog lane must never displace the admin
    // row in round 1 (R7).
    for i in 0..AUTH_ROWS {
        let event_id = Uuid::new_v4();
        sqlx::query(
            r"INSERT INTO audit_governance_outbox
                    (event_id, payload, available_at, attempts, status, priority, class)
              VALUES ($1, $2, clock_timestamp(), 0, 0, $3, 'message')",
        )
        .bind(event_id)
        .bind(json!({
            "event_id": event_id.to_string(),
            "source_system": "aero-im.source",
            "idempotency_key": event_id.to_string(),
            "action": "auth.login.failed",
        }))
        .bind(BACKLOG_PRIORITY)
        .execute(&pool)
        .await
        .with_context(|| format!("seed auth backlog row {i}"))?;
        seeded.push(event_id);
    }
    let moderation_id = Uuid::new_v4();
    sqlx::query(
        r"INSERT INTO audit_governance_outbox
                (event_id, payload, available_at, attempts, status, priority, class)
          VALUES ($1, $2, clock_timestamp(), 0, 0, $3, 'admin')",
    )
    .bind(moderation_id)
    .bind(json!({
        "event_id": moderation_id.to_string(),
        "source_system": "aero-im.source",
        "action": MODERATION_ACTION,
    }))
    .bind(MODERATION_PRIORITY)
    .execute(&pool)
    .await
    .context("seed the moderation row")?;
    seeded.push(moderation_id);
    println!(
        "seeded {BACKLOG_ROWS} backlog rows + {AUTH_ROWS} auth-token backlog row(s) \
         (priority {BACKLOG_PRIORITY}) then 1 moderation row (class admin, \
         priority {MODERATION_PRIORITY}, action {MODERATION_ACTION})"
    );

    let repo: Arc<dyn OutboxRepo> = Arc::new(PgOutboxRepo::new(pool.clone()));
    let client = AuditClient::new(config.clone()).context("build audit client")?;
    let relay = AuditRelay::new(repo, client, config);

    // Round 1: batch_size 100 < 501 rows, so the first claimed set cannot
    // contain everything — its composition must come from priority, not FIFO.
    let claimed = relay.dispatch_batch().await.context("dispatch round 1")?;
    if claimed != usize::try_from(BATCH_SIZE).expect("small batch size") {
        anyhow::bail!("round 1 claimed {claimed}, expected {BATCH_SIZE} (batch_size)");
    }
    let delivered_first: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::bigint FROM audit_governance_outbox WHERE status = 2")
            .fetch_one(&pool)
            .await
            .context("count round-1 delivered rows")?;
    if delivered_first != BATCH_SIZE {
        anyhow::bail!(
            "round 1 delivered {delivered_first} != {BATCH_SIZE} \
             (stub must settle every claim; claim order must be priority-first)"
        );
    }
    let moderation_delivered: Option<OffsetDateTime> =
        sqlx::query_scalar("SELECT delivered_at FROM audit_governance_outbox WHERE event_id = $1")
            .bind(moderation_id)
            .fetch_one(&pool)
            .await
            .context("read the moderation row delivered_at")?;
    if moderation_delivered.is_none() {
        anyhow::bail!(
            "round 1: the moderation row was NOT claimed+delivered within the first batch — \
             claim order must be priority DESC, not FIFO/enqueue order (B5-3 not landed?)"
        );
    }
    // D3: membership in the top-100 claimed set is the contract — NOT
    // `delivered_at == MIN(delivered_at)`. `UPDATE … FROM (CTE ORDER BY …)
    // … RETURNING` emits target-table heap order (the moderation row is
    // inserted last, so it settles last); delivery *firstness* is an
    // executor artifact, never an assertion.
    println!("drill: moderation-in-first-batch: PASS");

    // D8′ vocabulary read-back: the delivered moderation row's payload
    // `action` must be inside the contract pair. This is a seeded-row re-
    // read (the relay forwards claim.payload verbatim and never writes the
    // payload back), not wire-level delivery evidence — its value is the
    // greppable acceptance line + defense-in-depth against future payload
    // rewriting. NULL/missing action → red FAIL, never a no-op pass.
    let delivered_action: Option<String> = sqlx::query_scalar(
        "SELECT payload->>'action' FROM audit_governance_outbox WHERE event_id = $1",
    )
    .bind(moderation_id)
    .fetch_one(&pool)
    .await
    .context("read the delivered moderation row action")?;
    match delivered_action.as_deref() {
        Some(action) if MODERATION_OUTBOUND_ACTIONS.contains(&action) => {}
        other => anyhow::bail!(
            "delivered moderation row action {other:?} not in contract vocabulary \
             {MODERATION_OUTBOUND_ACTIONS:?}"
        ),
    }
    println!("drill: moderation-action-vocabulary: PASS");

    // Drain the remaining 401 rows (6 more rounds at batch_size 100).
    for round in 2..=MAX_ROUNDS {
        let claimed = relay
            .dispatch_batch()
            .await
            .context("dispatch a drain round")?;
        if claimed == 0 {
            break;
        }
        if round == MAX_ROUNDS {
            anyhow::bail!("relay did not drain within {MAX_ROUNDS} rounds");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let delivered: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::bigint FROM audit_governance_outbox WHERE status = 2")
            .fetch_one(&pool)
            .await
            .context("count delivered rows")?;
    if delivered != TOTAL_ROWS {
        let stuck: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM audit_governance_outbox WHERE status IN (0, 1, 3)",
        )
        .fetch_one(&pool)
        .await
        .context("count stuck rows")?;
        anyhow::bail!(
            "delivered {delivered} != {TOTAL_ROWS} (stuck rows: {stuck}) — \
             every seeded row must settle against the stub"
        );
    }
    println!("drill: drain-502: PASS");

    let delivered_ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT event_id FROM audit_governance_outbox WHERE status = 2 ORDER BY event_id",
    )
    .fetch_all(&pool)
    .await
    .context("list delivered event ids")?;
    let mut expected = seeded.clone();
    expected.sort_unstable();
    if delivered_ids != expected {
        anyhow::bail!(
            "event_id parity mismatch: delivered set differs from the seeded set \
             (delivered {}, seeded {})",
            delivered_ids.len(),
            expected.len()
        );
    }
    println!("drill: parity-502: PASS");

    // Phase 2: B5-3 D-CAP starvation leg — a sustained priority-100 flood
    // must never starve the priority-10 backlog lane (self-isolating
    // TRUNCATE + seed + per-round floor asserts, see phase2_starvation).
    phase2_starvation(&pool, &relay).await?;

    // Round-1 batch membership (above) is the ordering oracle — the
    // full-drain strict-first assert was removed per D3 (heap-order RETURNING
    // makes delivery *firstness* executor-dependent; parity-501 already
    // proves the moderation row was delivered exactly once).
    stub.shutdown();
    Ok(())
}

/// B5-3 D-CAP starvation leg (phase 2): after the moderation-precedence
/// phase, self-isolate with the same destructive gate, seed a 600-row admin
/// flood (priority 100, earliest `available_at`) plus 100 backlog rows
/// (priority 10, latest), and assert the floor: exactly
/// `STARVATION_K` = `min_service_floor(BATCH_SIZE)` backlog rows per round
/// while the flood persists, then a full 700-row drain within
/// [`MAX_ROUNDS`].
async fn phase2_starvation(pool: &PgPool, relay: &AuditRelay) -> anyhow::Result<()> {
    // Self-isolating destructive gate (drill precedent): after phase 1 the
    // table holds 502 rows, so the env check is meaningful even on a
    // previously-empty DB — a direct invocation without the opt-in REFUSEs
    // here (exit 1) instead of silently truncating phase-1 rows.
    let mut tx = pool.begin().await.context("begin phase-2 gate tx")?;
    sqlx::query("LOCK TABLE audit_governance_outbox IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await
        .context("lock outbox for the phase-2 gate")?;
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*)::bigint FROM audit_governance_outbox")
        .fetch_one(&mut *tx)
        .await
        .context("count outbox rows in the phase-2 gate")?;
    if !truncate_gate_allows(
        n,
        std::env::var("AERO_PRIORITY_DRILL_ALLOW_TRUNCATE").ok().as_deref(),
    ) {
        tx.rollback().await?;
        eprintln!(
            "aero-audit-priority-drill: REFUSED (phase 2) — audit_governance_outbox has {n} \
             row(s); the phase-2 starvation leg TRUNCATEs the table. Re-run with \
             AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=1 (throwaway DB only)"
        );
        std::process::exit(1);
    }
    sqlx::query("TRUNCATE audit_governance_outbox")
        .execute(&mut *tx)
        .await
        .context("reset the governance outbox (phase-2 TRUNCATE)")?;
    tx.commit().await?;

    // Seed the admin flood FIRST (earliest `available_at`) and the backlog
    // LAST (latest) — FIFO can never explain low-lane progress.
    let mut seeded = Vec::new();
    for i in 0..STARVATION_ADMIN_ROWS {
        let event_id = Uuid::new_v4();
        sqlx::query(
            r"INSERT INTO audit_governance_outbox
                    (event_id, payload, available_at, attempts, status, priority, class)
              VALUES ($1, $2, clock_timestamp(), 0, 0, $3, 'admin')",
        )
        .bind(event_id)
        .bind(json!({
            "event_id": event_id.to_string(),
            "source_system": "aero-im.source",
        }))
        .bind(MODERATION_PRIORITY)
        .execute(pool)
        .await
        .with_context(|| format!("seed starvation admin row {i}"))?;
        seeded.push(event_id);
    }
    for i in 0..STARVATION_BACKLOG_ROWS {
        let event_id = Uuid::new_v4();
        sqlx::query(
            r"INSERT INTO audit_governance_outbox
                    (event_id, payload, available_at, attempts, status, priority, class)
              VALUES ($1, $2, clock_timestamp(), 0, 0, $3, 'message')",
        )
        .bind(event_id)
        .bind(json!({
            "event_id": event_id.to_string(),
            "source_system": "aero-im.source",
        }))
        .bind(BACKLOG_PRIORITY)
        .execute(pool)
        .await
        .with_context(|| format!("seed starvation backlog row {i}"))?;
        seeded.push(event_id);
    }
    println!(
        "seeded {STARVATION_ADMIN_ROWS} admin flood rows (priority {MODERATION_PRIORITY}) \
         then {STARVATION_BACKLOG_ROWS} backlog rows (priority {BACKLOG_PRIORITY})"
    );

    // Round 1: claimed == 100, split exactly 95 admin / 5 backlog — the
    // pinned D-CAP batch split (arm A `BATCH_SIZE − STARVATION_K` / arm B
    // `STARVATION_K`).
    let claimed = relay
        .dispatch_batch()
        .await
        .context("dispatch starvation round 1")?;
    if claimed != usize::try_from(BATCH_SIZE).expect("small batch size") {
        anyhow::bail!("starvation round 1 claimed {claimed}, expected {BATCH_SIZE}");
    }
    let admin_delivered: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM audit_governance_outbox WHERE status = 2 AND priority = 100",
    )
    .fetch_one(pool)
    .await
    .context("count round-1 delivered admin rows")?;
    let backlog_delivered: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM audit_governance_outbox WHERE status = 2 AND priority = 10",
    )
    .fetch_one(pool)
    .await
    .context("count round-1 delivered backlog rows")?;
    if admin_delivered != BATCH_SIZE - STARVATION_K || backlog_delivered != STARVATION_K {
        anyhow::bail!(
            "starvation round 1 split {admin_delivered} admin / {backlog_delivered} backlog, \
             expected {} / {STARVATION_K}",
            BATCH_SIZE - STARVATION_K
        );
    }
    println!("drill: starvation-round1-split-95-5: PASS");

    // Rounds 2-5: the quota is per-round, not a round-1 artifact — after
    // round 5 exactly 25 backlog rows are delivered while 125 admin rows
    // are still pending (the flood outlives the low lane's progress).
    for round in 2..=5 {
        let claimed = relay
            .dispatch_batch()
            .await
            .context("dispatch a starvation quota round")?;
        if claimed != usize::try_from(BATCH_SIZE).expect("small batch size") {
            anyhow::bail!("starvation round {round} claimed {claimed}, expected {BATCH_SIZE}");
        }
    }
    let backlog_at_5: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM audit_governance_outbox WHERE status = 2 AND priority = 10",
    )
    .fetch_one(pool)
    .await
    .context("count backlog delivered after round 5")?;
    let admin_pending_at_5: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM audit_governance_outbox \
         WHERE status IN (0, 1) AND priority = 100",
    )
    .fetch_one(pool)
    .await
    .context("count admin pending after round 5")?;
    if backlog_at_5 != STARVATION_K * 5 {
        anyhow::bail!(
            "after round 5: backlog delivered {backlog_at_5}, expected {}",
            STARVATION_K * 5
        );
    }
    let expected_admin_pending = STARVATION_ADMIN_ROWS - 5 * (BATCH_SIZE - STARVATION_K);
    if admin_pending_at_5 != expected_admin_pending || admin_pending_at_5 <= 0 {
        anyhow::bail!(
            "after round 5: admin pending {admin_pending_at_5}, expected {expected_admin_pending} > 0"
        );
    }
    println!("drill: starvation-cross-batch-quota: PASS");

    // Drain: rounds 6..=MAX_ROUNDS (precomputed: rounds 1-6 @ 95/5, round 7
    // @ 30 admin + 70 backlog, round 8 empty → 7 non-empty ≤ MAX_ROUNDS).
    for round in 6..=MAX_ROUNDS {
        let claimed = relay
            .dispatch_batch()
            .await
            .context("dispatch a starvation drain round")?;
        if claimed == 0 {
            break;
        }
        if round == MAX_ROUNDS {
            anyhow::bail!("starvation leg did not drain within {MAX_ROUNDS} rounds");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let delivered: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM audit_governance_outbox WHERE status = 2",
    )
    .fetch_one(pool)
    .await
    .context("count delivered starvation rows")?;
    if delivered != STARVATION_ADMIN_ROWS + STARVATION_BACKLOG_ROWS {
        anyhow::bail!(
            "starvation drain delivered {delivered}, expected {}",
            STARVATION_ADMIN_ROWS + STARVATION_BACKLOG_ROWS
        );
    }
    let delivered_ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT event_id FROM audit_governance_outbox WHERE status = 2 ORDER BY event_id",
    )
    .fetch_all(pool)
    .await
    .context("list delivered starvation event ids")?;
    let mut expected = seeded.clone();
    expected.sort_unstable();
    if delivered_ids != expected {
        anyhow::bail!(
            "starvation event_id parity mismatch: delivered {} vs seeded {}",
            delivered_ids.len(),
            expected.len()
        );
    }
    println!("drill: starvation-drain-700: PASS");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vocabulary pin is the contract pair verbatim (mirror of the leaf
    /// `vocabulary_consts_are_pinned` shape) — a leaf flip must not silently
    /// drag this pin along.
    #[test]
    fn outbound_vocabulary_is_the_contract_pair() {
        assert_eq!(
            MODERATION_OUTBOUND_ACTIONS,
            ["admin.content.flag", "admin.moderation.action"]
        );
    }

    /// The leaf's single locked constant must be inside the pair — either
    /// contract spelling is legal (a flip between the two stays green).
    #[test]
    fn leaf_action_is_inside_the_contract_vocabulary() {
        assert!(MODERATION_OUTBOUND_ACTIONS.contains(&MODERATION_ACTION));
    }

    /// B5-3 D-CAP: the pinned starvation floor must equal the production
    /// floor at the drill's batch size — `STARVATION_K` is asserted, never
    /// hardcoded blindly (D8′ style).
    #[test]
    fn starvation_floor_matches_production_batch() {
        assert_eq!(
            STARVATION_K,
            aero_audit_connector::relay::min_service_floor(BATCH_SIZE)
        );
    }

    /// The destructive gate is fail-closed: only the exact opt-in string
    /// "1" allows a TRUNCATE over a non-empty outbox (shared by both
    /// phases; "true"/"yes"/absent refuse).
    #[test]
    fn truncate_gate_requires_the_exact_opt_in() {
        assert!(truncate_gate_allows(0, None), "empty table needs no opt-in");
        assert!(truncate_gate_allows(0, Some("0")));
        assert!(
            !truncate_gate_allows(502, None),
            "non-empty outbox refuses without the opt-in"
        );
        assert!(
            !truncate_gate_allows(502, Some("true")),
            "only the exact string '1' allows"
        );
        assert!(truncate_gate_allows(502, Some("1")));
    }
}
