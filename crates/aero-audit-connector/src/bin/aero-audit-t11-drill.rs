//! T-11 fail-closed drill — the audit relay absent ⇒ outbox rows stay pending.
//!
//! Usage (after `aero-cli migrate` on a disposable database):
//!
//! ```text
//! DATABASE_URL=postgres://…/aero_t11_drill_$$ cargo run -p aero-audit-connector \
//!     --bin aero-audit-t11-drill
//! ```
//!
//! Seeds N rows directly into B5-1's `audit_governance_outbox` (the 0239
//! table; `AERO_AUDIT_DRILL_ROWS`, default 3) — N 1:1-shaped rows PLUS one
//! window-shaped row (md5 window key, class 'message', priority 10) PLUS one
//! spill-shaped row (deterministic md5(key|'|'|event) key, `spill: true`)
//! PLUS one admin-class row (class 'admin', priority 100) PLUS one room-class
//! row (class 'room', priority 10) PLUS one AUTH-token-shaped row (class
//! 'message', priority 10, conforming envelope carrying
//! `action: "auth.login.failed"` — the auth backlog lane of the
//! security-event-audit direction) — the per-class L1 aggregation shapes AC1
//! requires on the sink-absent leg — then runs
//! the relay against a deterministically CLOSED token endpoint (loopback port
//! bound and dropped — connections are refused, no wall-clock window), so no
//! token can ever be minted and the events endpoint is never reached. It then
//! asserts the *reachable* T-11 invariants (design finding D1 — a literal
//! `SUM(attempts) == 0` "zero claims" is unreachable because
//! `dispatch_batch` claims rows before the per-claim token fetch):
//!   - `COUNT(status=0) == total` — every row re-parks Ready (pending, never
//!     silently delivered);
//!   - `COUNT(status IN (1,2,3)) == 0` — zero claimed-at-rest / delivered /
//!     dead (never falsely dead);
//!   - `SUM(attempts) == total` after round 1 and `2*total` after round 2 —
//!     the relay really ran (non-vacuous evidence), and the 1.2s sleep
//!     (> backoff(1) = 1s, same-machine PG clock) proves the retry-forever
//!     posture;
//!   - `COUNT(last_error LIKE '%audit connector HTTP transport failed%') == total`
//!     — the closed token endpoint was genuinely attempted.
//!
//! The window/spill seeds carry a CONFORMING payload (`source_system` +
//! own `event_id` + own `idempotency_key`) — `validate_delivery_payload`
//! runs before any transport, so a guard-failing window row would dead
//! (permanent) and break the `terminal == 0` invariant. All invariants are
//! written in terms of the seeded count, so the extra shapes shift the
//! constants (rows → rows + 5), never the equations — a seed that silently
//! drops a shape fails the shifted totals.
//!
//! BOUND: `AERO_AUDIT_DRILL_ROWS` must be ≤ 95. `dispatch_batch` calls
//! `claim_due` exactly once per batch with the drill's fixed
//! `batch_size: 100`, and the drill asserts `claimed == total`; with the
//! five extra seeds `total = rows + 5 ≤ 100` ⇒ `rows ≤ 95` (rows=96 fails
//! fail-loud with `round 1: claimed 100 rows, expected 101`). The bound is
//! enforced at startup — an oversized env input bails immediately instead of
//! mid-run. Default 3 is unaffected (total 8 ≤ 100).
//!
//! The core T-11 property (pending rows never silently succeed, never falsely
//! die) is fully preserved. Exits 2 with a clear message when the 0239 table
//! is absent (B5-1 not yet landed); the test-integration.sh section is gated
//! on the migration file.

use std::sync::Arc;
use std::time::Duration;

use aero_audit_connector::client::AuditClient;
use aero_audit_connector::config::RelayConfig;
use aero_audit_connector::fake::StaticScopeProvisioner;
use aero_audit_connector::outbox::OutboxRepo;
use aero_audit_connector::pg::PgOutboxRepo;
use aero_audit_connector::relay::AuditRelay;
use aero_common::{
    AGGREGATED_MESSAGE_ACTION, AUDIT_SOURCE_SYSTEM, GOVERNANCE_CLASS_ADMIN,
    GOVERNANCE_CLASS_MESSAGE, GOVERNANCE_CLASS_ROOM, L1_WINDOW_SECONDS,
};
use anyhow::Context;
use reqwest::Url;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

const ROUNDS: usize = 2;
/// Sleep between round 1 and round 2: strictly longer than backoff(1) = 1s so
/// the requeued rows are claimable again on the repo clock.
const RETRY_SLEEP: Duration = Duration::from_millis(1200);

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
        .filter(|value| *value > 0)
        .unwrap_or(3);
    // BOUND (docstring above): `dispatch_batch` claims exactly once per batch
    // with batch_size 100 and the drill asserts `claimed == total`; the five
    // extra seeds shift `total = rows + 5`, so `rows ≤ 95` or the mid-run
    // claim assertion fails confusingly (rows=96 → `claimed 100 rows,
    // expected 101`). Fail loud at startup instead.
    if rows > 95 {
        anyhow::bail!(
            "AERO_AUDIT_DRILL_ROWS must be ≤ 95 (total = rows + 5 ≤ claim batch 100); \
             got {rows}"
        );
    }
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

    // Self-isolating start (db-reviewer finding 3): TRUNCATE the outbox so
    // the drill's count/parity invariants hold even when a sibling drill ran
    // against the same database first (the harness grants each drill its own
    // throwaway DB, but a shared-DB run must never corrupt the assertions).
    sqlx::query("TRUNCATE audit_governance_outbox")
        .execute(&pool)
        .await
        .context("reset the governance outbox (TRUNCATE-at-start guard)")?;
    println!("reset audit_governance_outbox (TRUNCATE-at-start guard)");

    // Deterministically closed token endpoint: bind an ephemeral loopback
    // port, take its number, drop the listener. Every connection is refused
    // with no wall-clock window — no token can ever be minted and the events
    // endpoint is never reached.
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("bind the closed token endpoint port")?;
    let port = closed
        .local_addr()
        .context("read the closed token endpoint port")?
        .port();
    drop(closed);
    let token_endpoint = Url::parse(&format!("http://127.0.0.1:{port}/token"))
        .context("closed token endpoint URL")?;
    let events_url =
        Url::parse(&format!("http://127.0.0.1:{port}/events")).context("unreachable events URL")?;

    let config = RelayConfig {
        token_endpoint,
        events_url,
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
        batch_size: 100,
        concurrency: 4,
        jwks_uri: None,
        provision_freshness: std::time::Duration::from_secs(300),
    };

    // Seed N enqueued rows (status 0, due now, 1:1 event_id with the audit
    // event ids they carry — A3 drill precedent).
    for _ in 0..rows {
        let event_id = Uuid::new_v4();
        sqlx::query(
            r"INSERT INTO audit_governance_outbox
                    (event_id, payload, available_at, attempts, status)
              VALUES ($1, $2, clock_timestamp(), 0, 0)",
        )
        .bind(event_id)
        .bind(json!({ "event_id": event_id.to_string(), "source_system": AUDIT_SOURCE_SYSTEM }))
        .execute(&pool)
        .await
        .with_context(|| format!("seed governance row {event_id}"))?;
    }
    // L1 aggregation shapes (AC1): one window-shaped row + one spill-shaped
    // row, deterministic md5 keys, conforming payload (source_system + own
    // event_id + own idempotency_key — the payload guard would otherwise
    // dead them before any transport and break `terminal == 0`).
    let (window_key, spill_key) = seed_l1_shapes(&pool).await?;
    // Per-class shapes (AC4): one admin-class row (priority 100 =
    // GOVERNANCE_PRIORITY_MODERATION) + one room-class row (priority 10 =
    // GOVERNANCE_PRIORITY_BACKLOG), full-column INSERT with the conforming
    // 1:1 envelope (event_id = own PK, source_system = AUDIT_SOURCE_SYSTEM,
    // own idempotency_key — the payload guard runs before transport; a
    // guard-failing row would dead and break `terminal == 0`).
    let (admin_key, room_key) = seed_class_rows(&pool).await?;
    // Auth-token-shaped row (the auth backlog lane): class 'message',
    // priority 10, conforming 1:1 envelope carrying `action:
    // "auth.login.failed"` — the token the security-event-audit direction's
    // login path emits (payload guard runs before transport; a non-conforming
    // row would dead and break `terminal == 0`).
    let auth_key = seed_auth_row(&pool).await?;
    let total = rows + 5; // rows 1:1 + window + spill + admin + room + auth
    println!(
        "seeded {rows} 1:1 + window {window_key} + spill {spill_key} + admin {admin_key} \
         + room {room_key} + auth {auth_key} audit governance rows (token endpoint closed)"
    );

    let repo: Arc<dyn OutboxRepo> = Arc::new(PgOutboxRepo::new(pool.clone()));
    let client = AuditClient::new(config.clone()).context("build audit client")?;
    let relay = AuditRelay::new(repo, client, config)
        .with_scope_provisioner(Arc::new(StaticScopeProvisioner::new(true)));

    for round in 1..=ROUNDS {
        let claimed = relay
            .dispatch_batch()
            .await
            .context("dispatch a relay batch")?;
        if claimed != usize::try_from(total).expect("small row count") {
            anyhow::bail!(
                "round {round}: claimed {claimed} rows, expected {total} \
                 (every seeded row must be claimable)"
            );
        }
        let pending: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM audit_governance_outbox WHERE status = 0",
        )
        .fetch_one(&pool)
        .await
        .context("count pending rows")?;
        let terminal: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM audit_governance_outbox WHERE status IN (1, 2, 3)",
        )
        .fetch_one(&pool)
        .await
        .context("count terminal rows")?;
        let attempts: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(attempts), 0)::bigint FROM audit_governance_outbox",
        )
        .fetch_one(&pool)
        .await
        .context("sum attempts")?;
        let transport_errors: i64 = sqlx::query_scalar(
            r"SELECT COUNT(*)::bigint FROM audit_governance_outbox
               WHERE last_error LIKE '%audit connector HTTP transport failed%'",
        )
        .fetch_one(&pool)
        .await
        .context("count transport-error rows")?;
        let expected_attempts = total * i64::try_from(round).expect("small round");
        if pending != total
            || terminal != 0
            || attempts != expected_attempts
            || transport_errors != total
        {
            anyhow::bail!(
                "round {round} T-11 invariant broken: pending={pending} (want {total}), \
                 terminal={terminal} (want 0), SUM(attempts)={attempts} (want {expected_attempts}), \
                 transport-errors={transport_errors} (want {total}) — rows must stay pending \
                 with zero delivered/dead and per-round claim evidence"
            );
        }
        // Per-round keyed evidence for every shape: rows stay status 0,
        // attempts == round, the COLUMN last_error records the transport
        // failure, and the class/priority COLUMNS equal the seeded values
        // (per-class evidence — a seed that silently drops a shape fails the
        // shifted totals above; this pins the shape's own row evidence).
        // `last_error` is read from the COLUMN, never from the payload:
        // `deliver(&Claim)` never mutates `claim.payload` (relay.rs
        // deliver_claim) and the requeue/mark_dead paths write `last_error`
        // on the row only — the payload never carries it.
        for (key, expected_class, expected_priority) in [
            (window_key, GOVERNANCE_CLASS_MESSAGE, 10_i16),
            (spill_key, GOVERNANCE_CLASS_MESSAGE, 10_i16),
            (admin_key, GOVERNANCE_CLASS_ADMIN, 100_i16),
            (room_key, GOVERNANCE_CLASS_ROOM, 10_i16),
            (auth_key, GOVERNANCE_CLASS_MESSAGE, 10_i16),
        ] {
            let row: (i32, i64, String, i16, Option<String>, Value) = sqlx::query_as(
                "SELECT status, attempts, class, priority, last_error, payload
                   FROM audit_governance_outbox WHERE event_id = $1",
            )
            .bind(key)
            .fetch_one(&pool)
            .await
            .with_context(|| format!("read shape row {key}"))?;
            let last_error = row.4.as_deref().unwrap_or("<no last_error>");
            let mut broken = row.0 != 0
                || row.1 != i64::try_from(round).expect("small round")
                || row.2 != expected_class
                || row.3 != expected_priority
                || !last_error.contains("transport failed");
            // The window/spill keys keep their payload count/aggregated
            // asserts; the admin/room keys assert the conforming 3-key 1:1
            // envelope instead (no L1 markers on 1:1 rows).
            if key == window_key || key == spill_key {
                broken |= row.5["count"] != json!(1) || row.5["aggregated"] != json!(true);
            } else {
                broken |= row.5["event_id"] != key.to_string()
                    || row.5["source_system"] != AUDIT_SOURCE_SYSTEM
                    || row.5["idempotency_key"] != key.to_string();
            }
            if broken {
                anyhow::bail!(
                    "round {round}: shape row {key} broken: status={} attempts={} \
                     class={:?} priority={} last_error={:?} payload={}",
                    row.0,
                    row.1,
                    row.2,
                    row.3,
                    row.4,
                    row.5
                );
            }
        }
        println!(
            "round {round}: {total}/{total} pending, 0 terminal, attempts sum {attempts}, \
             {transport_errors}/{total} recorded the transport failure"
        );
        if round < ROUNDS {
            tokio::time::sleep(RETRY_SLEEP).await;
        }
    }
    println!("drill: t11-pending: PASS");
    Ok(())
}

/// Seed the L1 window- and spill-shaped rows (deterministic md5 keys,
/// conforming arbitrated-envelope payload). Returns the two event ids.
async fn seed_l1_shapes(pool: &PgPool) -> anyhow::Result<(Uuid, Uuid)> {
    let ws = Uuid::new_v4();
    let window_epoch: i64 =
        sqlx::query_scalar("SELECT floor(extract(epoch FROM now()) / $1)::bigint")
            .bind(L1_WINDOW_SECONDS)
            .fetch_one(pool)
            .await
            .context("window epoch")?;
    let window_key: Uuid = sqlx::query_scalar("SELECT md5($1)::uuid")
        .bind(format!("{ws}|{GOVERNANCE_CLASS_MESSAGE}|{window_epoch}"))
        .fetch_one(pool)
        .await
        .context("window key")?;
    let event = Uuid::new_v4();
    let spill_key: Uuid = sqlx::query_scalar("SELECT md5($1)::uuid")
        .bind(format!(
            "{ws}|{GOVERNANCE_CLASS_MESSAGE}|{window_epoch}|{event}"
        ))
        .fetch_one(pool)
        .await
        .context("spill key")?;
    let started = time::OffsetDateTime::now_utc();
    let rfc3339 = |ts: time::OffsetDateTime| -> String {
        ts.format(&time::format_description::well_known::Rfc3339)
            .expect("rfc3339")
    };
    let window_start = rfc3339(started);
    let window_end = rfc3339(started + time::Duration::seconds(L1_WINDOW_SECONDS));
    let event_at = rfc3339(started);
    let envelope = |key: Uuid, count: i64, spill: bool| -> Value {
        let mut payload = json!({
            "event_id": key.to_string(),
            "source_system": AUDIT_SOURCE_SYSTEM,
            "event_type": "aero.im.security",
            "schema_id": "aero.im.security",
            "schema_version": 1,
            "occurred_at": event_at,
            "aggregate_type": "workspace",
            "aggregate_id": ws.to_string(),
            "action": AGGREGATED_MESSAGE_ACTION,
            "outcome": "success",
            "data_classification": "confidential",
            "retention_class": "security",
            "idempotency_key": key.to_string(),
            "count": count,
            "aggregated": true,
            "window_start": window_start,
            "window_end": window_end,
            "first_event_at": event_at,
            "last_event_at": event_at,
        });
        if spill {
            payload["spill"] = json!(true);
        }
        payload
    };
    for (key, count, spill) in [(window_key, 1, false), (spill_key, 1, true)] {
        sqlx::query(
            r"INSERT INTO audit_governance_outbox
                    (event_id, payload, available_at, attempts, status, class, priority)
              VALUES ($1, $2, clock_timestamp(), 0, 0, 'message', 10)",
        )
        .bind(key)
        .bind(envelope(key, count, spill))
        .execute(pool)
        .await
        .with_context(|| format!("seed L1 shape {key}"))?;
    }
    Ok((window_key, spill_key))
}

/// Seed one admin-class row (priority 100 = `GOVERNANCE_PRIORITY_MODERATION`)
/// and one room-class row (priority 10 = `GOVERNANCE_PRIORITY_BACKLOG`) with
/// the conforming 1:1 envelope (`event_id` = own PK, `source_system` =
/// `AUDIT_SOURCE_SYSTEM`, own `idempotency_key` — the payload guard runs before
/// transport, so a non-conforming payload would dead and break
/// `terminal == 0`). Returns the two event ids.
async fn seed_class_rows(pool: &PgPool) -> anyhow::Result<(Uuid, Uuid)> {
    let admin_key = Uuid::new_v4();
    let room_key = Uuid::new_v4();
    for (key, class, priority) in [
        (admin_key, GOVERNANCE_CLASS_ADMIN, 100_i16),
        (room_key, GOVERNANCE_CLASS_ROOM, 10_i16),
    ] {
        sqlx::query(
            r"INSERT INTO audit_governance_outbox
                    (event_id, status, class, priority, payload, available_at, attempts)
              VALUES ($1, 0, $2, $3, $4, clock_timestamp(), 0)",
        )
        .bind(key)
        .bind(class)
        .bind(priority)
        .bind(json!({
            "event_id": key.to_string(),
            "source_system": AUDIT_SOURCE_SYSTEM,
            "idempotency_key": key.to_string(),
        }))
        .execute(pool)
        .await
        .with_context(|| format!("seed class row {key} ({class}/{priority})"))?;
    }
    Ok((admin_key, room_key))
}

/// Seed one auth-token-shaped row: the auth backlog lane (class 'message',
/// priority 10 = `GOVERNANCE_PRIORITY_BACKLOG`) carrying a conforming 1:1
/// envelope with `action: "auth.login.failed"` (the token the
/// security-event-audit direction's login path emits verbatim). Returns the
/// event id.
async fn seed_auth_row(pool: &PgPool) -> anyhow::Result<Uuid> {
    let auth_key = Uuid::new_v4();
    sqlx::query(
        r"INSERT INTO audit_governance_outbox
                (event_id, status, class, priority, payload, available_at, attempts)
          VALUES ($1, 0, 'message', 10, $2, clock_timestamp(), 0)",
    )
    .bind(auth_key)
    .bind(json!({
        "event_id": auth_key.to_string(),
        "source_system": AUDIT_SOURCE_SYSTEM,
        "idempotency_key": auth_key.to_string(),
        "action": "auth.login.failed",
    }))
    .execute(pool)
    .await
    .with_context(|| format!("seed auth row {auth_key}"))?;
    Ok(auth_key)
}
