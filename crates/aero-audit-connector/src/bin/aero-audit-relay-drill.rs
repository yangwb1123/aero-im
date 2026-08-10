//! A3 relay drill — throwaway-DB end-to-end run of the audit connector.
//!
//! Usage (after `aero-cli migrate` on a disposable database):
//!
//! ```text
//! DATABASE_URL=postgres://…/aero_audit_connector_$$ cargo run -p aero-audit-connector \
//!     --bin aero-audit-relay-drill
//! ```
//!
//! Seeds N rows directly into B5-1's `audit_governance_outbox` (the 0239
//! table) — the **mixed-shape delivery leg** (receipt-contract resolution
//! §4.1): `rows/3` 1:1-shaped rows + `rows/3` window-shaped rows (md5 window
//! key, full arbitrated envelope, count 5) + the remainder spill-shaped rows
//! (own deterministic key, `spill: true`, count 1), plus **2 negative
//! controls** seeded with `attempts = 1` (their first claim is attempt 2 →
//! dead in-batch, no sleep): ① `payload.event_id` = a different uuid (correct
//! `source_system`) → dies at `ReceiptMismatch`; ② `payload.source_system` =
//! `"wrong"` (correct `event_id`) → dies at `PayloadGuard` **before any POST**
//! (`validate_delivery_payload` runs first in `client.rs::deliver`). It then
//! runs the relay against a stub audit sink (202 + valid receipt echoing
//! `payload.event_id`) and asserts:
//!   - `COUNT(*) WHERE status = 2` (delivered) == N — every conforming shape
//!     settles end-to-end through the receipt echo (status 2 is reachable
//!     only via POST → echo of `payload.event_id` → value-level match vs the
//!     row PK → `settle`; any missing/mismatched `event_id`/`source_system`
//!     on any shape deads it and the parity goes red);
//!   - `event_id` set-parity: the delivered set equals the seeded conforming
//!     set (no duplicates, no orphans);
//!   - `COUNT(status = 3) == 2`, `COUNT(status IN (0,1)) == 0` — both
//!     negative controls died at the pinned permanent classes;
//!   - `stub.posts() == N + 1` — every conforming row posted once, control ①
//!     posted once (receipt mismatch), control 2 never posted (payload
//!     guard).
//!
//! Exits 2 with a clear message when the 0239 table is absent (B5-1 not yet
//! landed); the test-integration.sh section is gated on the migration file.

use std::sync::Arc;
use std::time::Duration;

use aero_audit_connector::client::AuditClient;
use aero_audit_connector::config::RelayConfig;
use aero_audit_connector::outbox::OutboxRepo;
use aero_audit_connector::pg::PgOutboxRepo;
use aero_audit_connector::relay::AuditRelay;
use aero_audit_connector::stub::StubSink;
use aero_common::{
    AuditId, AGGREGATED_MESSAGE_ACTION, AUDIT_SOURCE_SYSTEM, GOVERNANCE_CLASS_MESSAGE,
    L1_WINDOW_SECONDS,
};
use anyhow::Context;
use reqwest::Url;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

const MAX_ROUNDS: usize = 10;

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

    // Stub sink: token endpoint mints claims matching the drill config;
    // events endpoint answers 202 with a durable receipt echoing event_id.
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
        batch_size: 100,
        concurrency: 4,
        jwks_uri: None,
    };

    // Seed N conforming rows (status 0, due now): rows/3 1:1-shaped + rows/3
    // window-shaped + remainder spill-shaped — the mixed-shape delivery leg.
    // Every conforming payload carries `event_id` == own row PK and
    // `source_system` == config value (the receipt echo reads payload.event_id;
    // the payload guard runs before any POST).
    let third = rows / 3;
    if third == 0 {
        anyhow::bail!(
            "AERO_AUDIT_DRILL_ROWS must be >= 3 for the mixed-shape leg (got {rows})"
        );
    }
    let one_one_n = third;
    let window_n = third;
    let spill_n = rows - 2 * third;
    let mut seeded = Vec::new();
    let ws = Uuid::new_v4();
    let window_epoch: i64 =
        sqlx::query_scalar("SELECT floor(extract(epoch FROM now()) / $1)::bigint")
            .bind(L1_WINDOW_SECONDS)
            .fetch_one(&pool)
            .await
            .context("window epoch")?;
    let started = time::OffsetDateTime::now_utc();
    let rfc3339 = |ts: time::OffsetDateTime| -> String {
        ts.format(&time::format_description::well_known::Rfc3339)
            .expect("rfc3339")
    };
    let event_at = rfc3339(started);
    let window_start = rfc3339(started);
    let window_end = rfc3339(started + time::Duration::seconds(L1_WINDOW_SECONDS));

    for _ in 0..one_one_n {
        let event_id = Uuid::new_v4();
        seed_conforming(&pool, event_id, json!({ "event_id": event_id.to_string(), "source_system": AUDIT_SOURCE_SYSTEM }))
            .await?;
        seeded.push(event_id);
    }
    for _ in 0..window_n {
        let key: Uuid = sqlx::query_scalar("SELECT md5($1)::uuid")
            .bind(format!(
                "{ws}|{GOVERNANCE_CLASS_MESSAGE}|{window_epoch}"
            ))
            .fetch_one(&pool)
            .await
            .context("window key")?;
        seed_conforming(
            &pool,
            key,
            l1_envelope(ws, key, &event_at, &window_start, &window_end, 5, false),
        )
        .await?;
        seeded.push(key);
    }
    for _ in 0..spill_n {
        // Distinct deterministic spill key per row: the window key's v_key
        // preimage + a fresh event uuid (the 0242 SQL shape).
        let event = Uuid::new_v4();
        let key: Uuid = sqlx::query_scalar("SELECT md5($1)::uuid")
            .bind(format!(
                "{ws}|{GOVERNANCE_CLASS_MESSAGE}|{window_epoch}|{event}"
            ))
            .fetch_one(&pool)
            .await
            .context("spill key")?;
        seed_conforming(
            &pool,
            key,
            l1_envelope(ws, key, &event_at, &window_start, &window_end, 1, true),
        )
        .await?;
        seeded.push(key);
    }
    // Negative controls (attempts = 1 ⇒ first claim is attempt 2 ⇒ dead
    // in-batch): ① mismatched payload.event_id → ReceiptMismatch; ② wrong
    // payload.source_system → PayloadGuard (never POSTed).
    let control_mismatch = Uuid::new_v4();
    seed_control(
        &pool,
        control_mismatch,
        json!({ "event_id": Uuid::new_v4().to_string(), "source_system": AUDIT_SOURCE_SYSTEM }),
    )
    .await?;
    let control_guard = Uuid::new_v4();
    seed_control(
        &pool,
        control_guard,
        json!({ "event_id": control_guard.to_string(), "source_system": "wrong" }),
    )
    .await?;
    println!(
        "seeded {rows} conforming rows ({one_one_n} 1:1 + {window_n} window + {spill_n} spill) \
         + 2 negative controls ({control_mismatch}, {control_guard})"
    );

    let repo: Arc<dyn OutboxRepo> = Arc::new(PgOutboxRepo::new(pool.clone()));
    let client = AuditClient::new(config.clone()).context("build audit client")?;
    let relay = AuditRelay::new(repo, client, config);
    for round in 1..=MAX_ROUNDS {
        let claimed = relay
            .dispatch_batch()
            .await
            .context("dispatch a relay batch")?;
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
    let delivered_ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT event_id FROM audit_governance_outbox WHERE status = 2 ORDER BY event_id",
    )
    .fetch_all(&pool)
    .await
    .context("list delivered event ids")?;
    let mut expected = seeded.clone();
    expected.sort_unstable();
    let stuck: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM audit_governance_outbox WHERE status IN (0, 1)",
    )
    .fetch_one(&pool)
    .await
    .context("count stuck rows")?;
    let dead: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM audit_governance_outbox WHERE status = 3",
    )
    .fetch_one(&pool)
    .await
    .context("count dead rows")?;

    if delivered != rows {
        anyhow::bail!("delivered {delivered} != seeded conforming {rows} (stuck rows: {stuck})");
    }
    if delivered_ids != expected {
        anyhow::bail!(
            "event_id parity mismatch: delivered set differs from the seeded conforming set \
             (delivered {}, seeded {})",
            delivered_ids.len(),
            expected.len()
        );
    }
    if dead != 2 {
        anyhow::bail!("dead {dead} != 2 (both negative controls must die in-batch)");
    }
    if stuck != 0 {
        anyhow::bail!(
            "{stuck} rows stuck in status 0/1 — every seeded row must reach a terminal \
             (delivered or dead)"
        );
    }
    // Control ① POSTs once (receipt mismatch), control ② never POSTs (the
    // payload guard runs before any transport — client.rs `deliver`). The
    // design sketch's "rows + 2" miscounts this second gate.
    let posts = stub.posts();
    if posts != usize::try_from(rows).expect("small rows") + 1 {
        anyhow::bail!(
            "stub POSTs {posts} != {rows} + 1 (every conforming row posted once, control ① once, \
             control ② zero — payload guard)"
        );
    }
    // Header-spelling pin: every conforming delivered id appears as its
    // Idempotency-Key header (AuditId Display = ULID base32 — including the
    // md5 window/spill PKs).
    let seen = stub.seen_idempotency_keys().await;
    let missing: Vec<String> = seeded
        .iter()
        .map(|id| AuditId::from_uuid(*id).to_string())
        .filter(|key| !seen.contains(key))
        .collect();
    if !missing.is_empty() {
        anyhow::bail!(
            "Idempotency-Key header spelling drift: {} seeded ids never observed (e.g. {})",
            missing.len(),
            missing[0]
        );
    }
    println!(
        "PASS: {delivered}/{rows} delivered (status 2), event_id set-parity exact, \
         {dead} dead (negative controls), {stuck} stuck, stub POSTs {posts}"
    );
    stub.shutdown();
    Ok(())
}

async fn seed_conforming(pool: &PgPool, event_id: Uuid, payload: Value) -> anyhow::Result<()> {
    sqlx::query(
        r"INSERT INTO audit_governance_outbox
                (event_id, payload, available_at, attempts, status)
          VALUES ($1, $2, clock_timestamp(), 0, 0)",
    )
    .bind(event_id)
    .bind(payload)
    .execute(pool)
    .await
    .with_context(|| format!("seed conforming row {event_id}"))?;
    Ok(())
}

async fn seed_control(pool: &PgPool, event_id: Uuid, payload: Value) -> anyhow::Result<()> {
    sqlx::query(
        r"INSERT INTO audit_governance_outbox
                (event_id, payload, available_at, attempts, status)
          VALUES ($1, $2, clock_timestamp(), 1, 0)",
    )
    .bind(event_id)
    .bind(payload)
    .execute(pool)
    .await
    .with_context(|| format!("seed control row {event_id}"))?;
    Ok(())
}

/// The arbitrated 0242 envelope (receipt-contract resolution §3): own
/// `event_id`, own `idempotency_key`, `source_system` = `AUDIT_SOURCE_SYSTEM`,
/// timestamps-only window fields (no member-id refs), `aggregated: true` on
/// both shapes, `spill: true` on spill rows only.
fn l1_envelope(
    ws: Uuid,
    key: Uuid,
    event_at: &str,
    window_start: &str,
    window_end: &str,
    count: i64,
    spill: bool,
) -> Value {
    let mut payload = json!({
        "event_id": key.to_string(),
        "source_system": AUDIT_SOURCE_SYSTEM,
        "event_type": "aero.im.security",
        "schema_id": "aero.im.security",
        "schema_version": 1,
        "occurred_at": if spill { event_at } else { window_start },
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
}
