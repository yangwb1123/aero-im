//! Observability wiring for the gateway (ROADMAP 方向四 follow-up).
//!
//! The metrics *foundation* — the registry, the well-known metric-name
//! constants, and the Prometheus text exposition — lives in
//! [`aero_common::metrics`]. This module is the **gateway-side glue** that the
//! design doc deliberately left for `aero-server`:
//!
//! * [`metrics_handler`] — the `GET /metrics` endpoint returning the global
//!   registry's Prometheus exposition.
//! * [`http_metrics_layer`] — an Axum middleware recording the RED signals
//!   (request count by method/status + request-duration histogram by route) for
//!   every request that flows through the tower stack.
//! * [`MetricsConfig`] — whether `/metrics` is exposed and (optionally) gated
//!   behind a bearer token, read from the `AERO_*` environment.
//! * [`record_rate_limit_rejection`] — bumps a dedicated counter when the rate
//!   limiter returns 429 (the limiter calls this).
//!
//! ## Cardinality
//!
//! HTTP series stay *bounded*: requests are labeled by `method` + `status`
//! (small, fixed sets) and the duration histogram by the **matched route
//! template** (e.g. `/api/rooms/:id/messages`, not the concrete id). Using
//! [`axum::extract::MatchedPath`] is what keeps an attacker from blowing up the
//! series count by hitting `/api/rooms/<random>/...` forever.
//!
//! ## Exposure
//!
//! `/metrics` is left **unauthenticated by default** (same posture as
//! `/health`), because the typical deployment scrapes it from inside the
//! cluster / behind network policy. Set `AERO_METRICS_TOKEN` to require
//! `Authorization: Bearer <token>` for any environment where the endpoint is
//! reachable from untrusted networks, or `AERO_METRICS_ENABLED=0` to disable the
//! route entirely.

use std::sync::Arc;
use std::time::Instant;

use aero_common::metrics::{self, names};
use aero_common::WorkspaceId;
use aero_audit_connector::outbox::OutboxRepo as _;
use axum::{
    extract::{MatchedPath, Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

/// Prometheus text exposition content type (version pinned per the format spec).
const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4";

/// Counter (server-local name): API requests rejected by the rate limiter (429).
///
/// Not one of the cross-crate [`names`] constants because rate limiting is a
/// gateway concern; it lives here next to its single emit site.
pub const RATE_LIMIT_REJECTIONS_TOTAL: &str = "aero_rate_limit_rejections_total";

/// Counter (server-local name): participant-profile cache lookups, labeled by
/// `result` = `hit` | `miss`. A fresh cache hit avoids a `ParticipantRepo::get`
/// DB round-trip; a miss falls through to Postgres and backfills the cache
/// (ROADMAP6 方向四 多级缓存). Hit-ratio = hit / (hit + miss).
///
/// Server-local (not a cross-crate [`names`] constant) because the participant
/// cache is purely a gateway concern. Emitted from
/// [`crate::participant_cache::ParticipantCache`]'s call sites via
/// [`record_participant_cache`].
pub const PARTICIPANT_CACHE_LOOKUPS_TOTAL: &str = "aero_participant_cache_lookups_total";

/// Records one participant-cache lookup outcome. `hit` ⇒ served from the local
/// TTL cache (no DB); `false` ⇒ a miss that fell through to the repo.
pub fn record_participant_cache(hit: bool) {
    let result = if hit { "hit" } else { "miss" };
    metrics::inc_counter_labeled(PARTICIPANT_CACHE_LOOKUPS_TOTAL, 1, &[("result", result)]);
}

/// Gauge (server-local name): on-disk size in bytes of a tracked index, labeled
/// by `index` = the index relation name. The companion observability for the
/// mig-0136 partial-index slimming: it lets an operator watch the GIN
/// (`search_tsv`, `searchable_text` trigram), HNSW (`embedding`), and
/// `messages_room_created_idx` indexes grow over time and spot bloat before a
/// `REINDEX`/`VACUUM` is overdue.
///
/// Cardinality is **bounded**: the label only takes values from
/// [`tracked_indexes`] (a fixed, code-defined list plus an optional bounded set
/// from `AERO_METRICS_EXTRA_INDEXES`), never anything attacker-controlled.
///
/// Server-local (not a cross-crate [`names`] constant) because index-bloat
/// sampling is a gateway operational concern; emitted from the periodic sampler
/// in `bin/boot/metrics_tasks.rs` via [`sample_index_sizes`].
pub const INDEX_SIZE_BYTES: &str = "aero_index_size_bytes";

/// Gauge: estimated dead-tuple ratio for each public application table.
/// Label cardinality is bounded by the deployed schema (`table`).
pub const PG_TABLE_DEAD_TUPLE_RATIO: &str = "aero_pg_table_dead_tuple_ratio";
/// Gauge: cumulative sequential scans reported by `PostgreSQL` for each public
/// application table. It is sampled as a gauge because `PostgreSQL` may reset its
/// statistics after restart.
pub const PG_TABLE_SEQ_SCANS_TOTAL: &str = "aero_pg_table_seq_scans_total";
/// Gauge: cumulative index scans reported by `PostgreSQL` for each public index.
/// Label cardinality is bounded by the deployed schema (`index`).
pub const PG_INDEX_SCANS_TOTAL: &str = "aero_pg_index_scans_total";
/// Gauge: sessions currently idle while holding an open transaction.
pub const PG_IDLE_IN_TRANSACTION_COUNT: &str = "aero_pg_idle_in_transaction_count";
/// Gauge: age in seconds of the oldest idle-in-transaction session.
pub const PG_IDLE_IN_TRANSACTION_MAX_SECONDS: &str = "aero_pg_idle_in_transaction_max_seconds";

// ---------------------------------------------------------------------------
// B5-4 audit-outbox sampler gauges (fail-closed operational loop).
// ---------------------------------------------------------------------------
//
// The B5-4 loop's observability face: the relay is presence-gated with no
// runtime health signal, so a misconfigured/absent relay silently strands
// `audit_governance_outbox` rows in status 0/1. These series make the stall
// visible and fail closed (sampler_up 0 + errors_total + retained values,
// never a fabricated zero snapshot).
//
// Multi-instance aggregation contract (HELP text): consumers must use `max()`
// across instances — fail-safe stale-high (a dead instance's old values are
// retained, so `max()` keeps the worst case visible); `sum()`/`avg()` are
// wrong. The error counter is a per-instance COUNTER and aggregates with
// `sum()`.

/// Gauge (server-local name): `audit_governance_outbox` status-bucket counts,
/// labeled `status` ∈ {enqueued, claimed, delivered, dead}. Tier-1 (30s)
/// writes {enqueued, claimed} (exact counts); Tier-2 (slow) writes all four
/// labels (exact counts). `{status="dead"}` is written ONLY by Tier-2 — the
/// Tier-1 dead signal lives on the separate [`AUDIT_OUTBOX_DEAD_ROWS`] series
/// so no label set mixes a 0/1 boolean with an exact count (single value
/// domain per series, `max()` aggregation stays exact).
pub const AUDIT_OUTBOX_STATUS: &str = "aero_audit_outbox_status";
/// Gauge (server-local name): age in seconds of the oldest status-0 (pending)
/// outbox row — exact `Q4_SQL` mirror (`min(available_at)`, status = 0) for
/// CLI/sampler oracle parity. Series absent when no pending row (CLI "n/a"
/// semantics).
pub const AUDIT_OUTBOX_OLDEST_PENDING_SECS: &str = "aero_audit_outbox_oldest_pending_secs";
/// Gauge (server-local name): age in seconds of the oldest status-1 (claimed)
/// row — the FM-Q stall-claim visibility mirror (the same expression over
/// status = 1). Settle-rejected rows live in status 1 (lease reclaim, never
/// requeue), so a quiet-period stall is visible HERE, not in the pending
/// series. Series absent when no claimed row.
pub const AUDIT_OUTBOX_OLDEST_CLAIMED_SECS: &str = "aero_audit_outbox_oldest_claimed_secs";
/// Gauge (server-local name): 1 = the last Tier-1 sample succeeded; 0 = it
/// failed (prior values retained — never a fabricated zero snapshot).
pub const AUDIT_OUTBOX_SAMPLER_UP: &str = "aero_audit_outbox_sampler_up";
/// Gauge (server-local name): count of dead (status 3) rows. OWN series — the
/// Tier-1 dead signal is a 0/1 flag (FM4 ≤ 1 tick visibility); the exact
/// count also lands on `{status="dead"}` via Tier-2. Keeping the bool off the
/// labeled status series preserves its `max()` aggregation contract.
pub const AUDIT_OUTBOX_DEAD_ROWS: &str = "aero_audit_outbox_dead_rows";
/// Counter (server-local name, per-instance): failed audit-outbox samples
/// (Tier-1 + Tier-2). Aggregates with `sum()` (a counter, never `max()`).
pub const AUDIT_OUTBOX_SAMPLE_ERRORS_TOTAL: &str = "aero_audit_outbox_sample_errors_total";
/// Gauge (server-local name): on-disk size in bytes of `audit_governance_outbox`
/// (the P-2 growth-surfacing companion to the bounded Q3 scan: growth is
/// visible before it approaches `statement_timeout`).
pub const AUDIT_OUTBOX_TABLE_SIZE_BYTES: &str = "aero_audit_outbox_table_size_bytes";

/// The default set of indexes the size gauge samples — the hot, bloat-prone
/// `messages` indexes touched by migration 0136 (partial GIN/HNSW slimming) plus
/// the room/created lookup index. Kept small and fixed so the `index` label
/// stays bounded.
const DEFAULT_TRACKED_INDEXES: &[&str] = &[
    "messages_search_tsv_gin",   // full-text GIN(search_tsv)
    "messages_searchable_trgm",  // trigram GIN(searchable_text)
    "messages_embedding_hnsw",   // vector HNSW(embedding)
    "messages_room_created_idx", // (room_id, created_at DESC)
];

/// Returns the bounded list of index names whose on-disk size the periodic
/// sampler reports as [`INDEX_SIZE_BYTES`].
///
/// This is [`DEFAULT_TRACKED_INDEXES`] plus, optionally, a small operator-defined
/// set from the comma-separated `AERO_METRICS_EXTRA_INDEXES` env var. The extra
/// set is **capped** (at 8) and de-duplicated so the gauge's `index` label can
/// never explode the series count, and each entry is trimmed/non-empty.
#[must_use]
pub fn tracked_indexes() -> Vec<String> {
    let mut out: Vec<String> = DEFAULT_TRACKED_INDEXES
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    if let Ok(extra) = std::env::var("AERO_METRICS_EXTRA_INDEXES") {
        for name in extra.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if out.len() >= 12 {
                break; // hard cardinality cap (4 defaults + up to 8 extra)
            }
            let name = name.to_owned();
            if !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out
}

/// Samples the on-disk size of each [`tracked_indexes`] entry and publishes it to
/// the [`INDEX_SIZE_BYTES`] gauge (label `index=<name>`).
///
/// Resolution uses `to_regclass(<name>)` so a missing index (e.g. a slimmed-down
/// deploy that never built the HNSW index) resolves to `NULL` and is simply
/// **skipped**, never erroring. Per-index query failures only `warn!` —
/// sampling is fail-open and must never crash the sampler loop. Returns the
/// number of indexes that were resolved and reported.
pub async fn sample_index_sizes(pool: &sqlx::PgPool) -> usize {
    let mut reported = 0usize;
    for index in tracked_indexes() {
        // `pg_relation_size(to_regclass($1))` → bigint, or NULL when the index
        // does not exist. `query_scalar::<_, Option<i64>>` yields:
        //   Ok(Some(Some(bytes)))  → index exists, report it
        //   Ok(Some(None)) / Ok(None) → index absent, skip
        //   Err(_)                 → query failed, warn and skip (fail-open)
        let row: Result<Option<i64>, _> =
            sqlx::query_scalar::<_, Option<i64>>("SELECT pg_relation_size(to_regclass($1))")
                .bind(&index)
                .fetch_one(pool)
                .await;
        match row {
            Ok(Some(bytes)) => {
                #[allow(clippy::cast_precision_loss)]
                metrics::set_gauge_labeled(
                    INDEX_SIZE_BYTES,
                    bytes as f64,
                    &[("index", index.as_str())],
                );
                reported += 1;
            }
            Ok(None) => {
                tracing::debug!(%index, "index size sample skipped: index does not exist");
            }
            Err(e) => {
                tracing::warn!(error = %e, %index, "index size query failed");
            }
        }
    }
    reported
}

/// Counts successfully sampled `PostgreSQL` health series. Query failures leave
/// the previous gauge values untouched so a transient monitoring failure never
/// fabricates a healthy zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PgHealthSample {
    pub tables: usize,
    pub indexes: usize,
    pub idle_activity: bool,
}

/// Sample `PostgreSQL` maintenance and query-efficiency signals from the
/// statistics views. Every query is read-only and restricted to the `public`
/// schema; metric labels therefore come only from deployed relation names, not
/// request data.
///
/// The three query groups fail independently. A failure logs a warning and
/// preserves the last exported values for that group.
pub async fn sample_pg_health(pool: &sqlx::PgPool) -> PgHealthSample {
    let mut sample = PgHealthSample::default();
    let tables = sqlx::query_as::<_, (String, i64, i64, i64)>(
        r"SELECT relname,
                  n_live_tup::bigint,
                  n_dead_tup::bigint,
                  seq_scan::bigint
             FROM pg_stat_user_tables
            WHERE schemaname = 'public'",
    )
    .fetch_all(pool)
    .await;
    match tables {
        Ok(rows) => {
            for (table, live, dead, sequential_scans) in rows {
                let total = live.saturating_add(dead);
                #[allow(clippy::cast_precision_loss)]
                let ratio = if total <= 0 {
                    0.0
                } else {
                    dead.max(0) as f64 / total as f64
                };
                metrics::set_gauge_labeled(
                    PG_TABLE_DEAD_TUPLE_RATIO,
                    ratio,
                    &[("table", table.as_str())],
                );
                #[allow(clippy::cast_precision_loss)]
                metrics::set_gauge_labeled(
                    PG_TABLE_SEQ_SCANS_TOTAL,
                    sequential_scans.max(0) as f64,
                    &[("table", table.as_str())],
                );
                sample.tables += 1;
            }
        }
        Err(error) => tracing::warn!(%error, "PostgreSQL table-stat sample failed"),
    }

    let indexes = sqlx::query_as::<_, (String, i64)>(
        r"SELECT indexrelname, idx_scan::bigint
             FROM pg_stat_all_indexes
            WHERE schemaname = 'public'",
    )
    .fetch_all(pool)
    .await;
    match indexes {
        Ok(rows) => {
            for (index, scans) in rows {
                #[allow(clippy::cast_precision_loss)]
                metrics::set_gauge_labeled(
                    PG_INDEX_SCANS_TOTAL,
                    scans.max(0) as f64,
                    &[("index", index.as_str())],
                );
                sample.indexes += 1;
            }
        }
        Err(error) => tracing::warn!(%error, "PostgreSQL index-stat sample failed"),
    }

    let idle = sqlx::query_as::<_, (i64, f64)>(
        r"SELECT COUNT(*)::bigint,
                  COALESCE(
                    EXTRACT(EPOCH FROM (clock_timestamp() - MIN(state_change))),
                    0
                  )::double precision
             FROM pg_stat_activity
            WHERE state = 'idle in transaction'",
    )
    .fetch_one(pool)
    .await;
    match idle {
        Ok((count, oldest_seconds)) => {
            #[allow(clippy::cast_precision_loss)]
            metrics::set_gauge(PG_IDLE_IN_TRANSACTION_COUNT, count.max(0) as f64);
            metrics::set_gauge(PG_IDLE_IN_TRANSACTION_MAX_SECONDS, oldest_seconds.max(0.0));
            sample.idle_activity = true;
        }
        Err(error) => tracing::warn!(%error, "PostgreSQL idle-transaction sample failed"),
    }
    sample
}

/// B5-4 Tier-1 (30s) read-only probe of the audit governance outbox. Any
/// query failure fails the WHOLE sample (never a partial/all-zero assembly):
/// the caller retains prior gauge values and flips `sampler_up` to 0 (F2).
pub async fn sample_audit_outbox(
    pool: &sqlx::PgPool,
) -> Result<aero_audit_connector::outbox::VerdictProbe, aero_audit_connector::outbox::Error> {
    let repo = aero_audit_connector::pg::PgOutboxRepo::new(pool.clone());
    repo.verdict_probe().await
}

/// Recency window for the sampler's Q3 status-bucket scan (P-2): the gauge's
/// purpose is the LIVE backlog + recent terminal state, not 1-year
/// archaeology — bounded per-instance cost even as the retention-bounded
/// table grows. This is a DELIBERATE sampler/CLI divergence: the connector
/// trait's `status_buckets()` and the aero-eng `audit-provision-check` CLI
/// stay the exact unbounded `Q3_SQL` mirror (oracle parity); only the
/// periodic sampler windows its scan. `{status="dead"}` therefore counts
/// dead rows created in the last 7 days; the unbounded Tier-1
/// `aero_audit_outbox_dead_rows` flag (O(1) EXISTS via the
/// `audit_governance_status3_idx` partial index) remains the "any dead row
/// exists (ever)" alert signal.
const AUDIT_OUTBOX_Q3_RECENCY: &str = "interval '7 days'";

/// Exact text mirror of `aero_eng::audit_provision::Q4_SQL` (with `{table}`
/// resolved): the sampler's oldest-pending age must agree with the
/// `audit-provision-check` CLI's — parity pinned by the unit test below.
const AUDIT_OUTBOX_OLDEST_PENDING_Q4_MIRROR: &str =
    "SELECT extract(epoch FROM (clock_timestamp() - min(available_at)))::bigint \
     FROM audit_governance_outbox WHERE status = 0";

/// B5-4 Tier-2 (slow) full sample: the Q3 status-bucket aggregation
/// (RECENCY-BOUNDED — see [`AUDIT_OUTBOX_Q3_RECENCY`]; the connector trait's
/// `status_buckets()` stays the exact unbounded mirror for CLI parity) + the
/// oldest-pending `Q4_SQL` mirror + the oldest-claimed (status 1) stall
/// mirror + the table-size gauge input. `oldest_pending_secs`/
/// `oldest_claimed_secs` are `None` when their status has no rows (series
/// absent — CLI "n/a" semantics). Pure reads; any query failure fails the
/// whole sample (the caller retains old values; Tier-1 is unaffected — the
/// two tiers fail independently, FM10).
pub async fn sample_audit_outbox_full(
    pool: &sqlx::PgPool,
) -> Result<
    (
        aero_audit_connector::outbox::StatusBuckets,
        Option<i64>,
        Option<i64>,
        i64,
    ),
    aero_audit_connector::outbox::Error,
> {
    // Q3 mirror, recency-bounded (P-2): `created_at >= now() - 7 days` over
    // all four statuses — the 30s-hot Tier-1 stays untouched (cost ∝ due
    // set); this Tier-2 scan is the one that must not grow with history.
    // Missing table → `Err` (never a fabricated zero snapshot).
    let rows = sqlx::query_as::<_, (i32, i64)>(&format!(
        "SELECT status, count(*)::bigint FROM audit_governance_outbox
          WHERE created_at >= clock_timestamp() - {AUDIT_OUTBOX_Q3_RECENCY}
          GROUP BY status ORDER BY status"
    ))
    .fetch_all(pool)
    .await?;
    let mut buckets = aero_audit_connector::outbox::StatusBuckets::default();
    for (status, count) in rows {
        match status {
            0 => buckets.enqueued = count,
            1 => buckets.claimed = count,
            2 => buckets.delivered = count,
            3 => buckets.dead = count,
            _ => {}
        }
    }
    // Exact Q4 mirror: `min(available_at)`, status = 0 → NULL over an empty
    // set → `None` (the aero-eng CLI's "n/a" semantics, kept for parity).
    let oldest_pending: Option<i64> =
        sqlx::query_scalar(AUDIT_OUTBOX_OLDEST_PENDING_Q4_MIRROR)
            .fetch_one(pool)
            .await?;
    // FM-Q stall visibility: the same expression over status = 1 — the rows
    // settle-rejection strands (lease reclaim, never requeue).
    let oldest_claimed: Option<i64> = sqlx::query_scalar(
        "SELECT extract(epoch FROM (clock_timestamp() - min(available_at)))::bigint
           FROM audit_governance_outbox WHERE status = 1",
    )
    .fetch_one(pool)
    .await?;
    // P-2 growth companion: O(1) `pg_total_relation_size` so table growth is
    // visible before the full GROUP BY approaches `statement_timeout`.
    let table_bytes: Option<i64> =
        sqlx::query_scalar("SELECT pg_total_relation_size('audit_governance_outbox')")
            .fetch_one(pool)
            .await?;
    Ok((buckets, oldest_pending, oldest_claimed, table_bytes.unwrap_or(0)))
}

/// Parse `AERO__SERVER__AUDIT_OUTBOX_FULL_SAMPLE_SECS` (H-3 three-state
/// table):
/// * absent / empty → `Some(300)` (default);
/// * `0` → `None` (disabled — one boot `info!`);
/// * a valid positive integer → `Some(secs)`;
/// * garbage / negative / overflow → **fail-closed disable**: `None` + one
///   boot `error!` (a typo must not quintuple the most expensive sampler).
///
/// Read via `std::env::var` directly (mirroring the `AERO_INDEX_SIZE_SAMPLE_SECS`
/// pattern): it must NOT live in the `AERO_AUDIT_*` namespace (the connector's
/// stray-scan boot error fires for any such var without `AERO_AUDIT_TOKEN_ENDPOINT`,
/// breaking the relay-absent harness boot) and must NOT go through figment's
/// `AppConfig` (aero-common is the no-touch list; figment ignores unknown keys
/// anyway).
#[must_use]
pub fn parse_audit_full_sample_secs() -> Option<u64> {
    let Some(raw) = std::env::var("AERO__SERVER__AUDIT_OUTBOX_FULL_SAMPLE_SECS").ok() else {
        return Some(300);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Some(300);
    }
    match trimmed.parse::<u64>() {
        Ok(0) => None,
        Ok(secs) => Some(secs),
        Err(_) => {
            tracing::error!(
                %trimmed,
                "AERO__SERVER__AUDIT_OUTBOX_FULL_SAMPLE_SECS is not a positive integer; \
                 the audit outbox Tier-2 full sampler is DISABLED (fail-closed)"
            );
            None
        }
    }
}

/// `/metrics` exposure policy, read from the environment.
#[derive(Debug, Clone)]
pub struct MetricsConfig {
    /// Whether the `/metrics` route is mounted at all.
    pub enabled: bool,
    /// Optional bearer token required to scrape `/metrics`. `None` ⇒ the
    /// endpoint is unauthenticated (the default, matching `/health`).
    pub bearer_token: Option<String>,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bearer_token: None,
        }
    }
}

impl MetricsConfig {
    /// Build from the environment.
    ///
    /// * `AERO_METRICS_ENABLED` — `0`/`false`/`off` ⇒ do not mount `/metrics`
    ///   (default: enabled).
    /// * `AERO_METRICS_TOKEN` — when set (and non-empty), `/metrics` requires
    ///   `Authorization: Bearer <token>`; otherwise it is unauthenticated.
    #[must_use]
    pub fn from_env() -> Self {
        let enabled = std::env::var("AERO_METRICS_ENABLED")
            .ok()
            .map_or(true, |v| !matches!(v.trim(), "0" | "false" | "off"));
        let bearer_token = std::env::var("AERO_METRICS_TOKEN")
            .ok()
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty());
        Self {
            enabled,
            bearer_token,
        }
    }
}

/// `GET /metrics` — renders the process-global registry as Prometheus text.
///
/// Extracts only [`MetricsConfig`] (via [`axum::extract::FromRef`]) rather than
/// the full `AppState`, so it needs no DB / NATS / Redis handles — which also
/// makes the route fully unit-testable offline.
///
/// Honours [`MetricsConfig::bearer_token`]: returns `401` when a token is
/// configured and the request lacks a matching `Authorization: Bearer` header.
/// (When no token is configured every request is served, like `/health`.)
pub async fn metrics_handler(
    State(cfg): State<Arc<MetricsConfig>>,
    headers: HeaderMap,
) -> Response {
    if let Some(expected) = cfg.bearer_token.as_deref() {
        if !bearer_matches(&headers, expected) {
            return (StatusCode::UNAUTHORIZED, "metrics: unauthorized").into_response();
        }
    }
    let body = metrics::render_prometheus();
    ([(header::CONTENT_TYPE, PROMETHEUS_CONTENT_TYPE)], body).into_response()
}

/// Constant-time-ish bearer comparison for the metrics token.
fn bearer_matches(headers: &HeaderMap, expected: &str) -> bool {
    let Some(raw) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some(token) = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
    else {
        return false;
    };
    let token = token.trim();
    // Length-prefixed equality; avoids leaking length via early return where it
    // is cheap to avoid. (The token is a deployment secret, not a password DB.)
    token.len() == expected.len() && token.bytes().zip(expected.bytes()).all(|(a, b)| a == b)
}

/// Response-extension marker carrying the tenant a request was attributed to, so
/// the [`http_metrics_layer`] can attach a per-tenant `workspace` label to the
/// HTTP RED signals **without** any of the architecturally-expensive
/// alternatives (a `workspace` JWT claim that would force re-issuing every token,
/// or a DB lookup on the request hot path).
///
/// ## How it works (the "response-extension pass-through" path)
///
/// A handler that has *already* cheaply resolved the owning workspace (e.g. it
/// parsed `workspace_id` off the request) stamps the response with this marker
/// via [`attach_workspace_label`]. The metrics layer, which sees every response,
/// reads it back with `response.extensions().get::<WorkspaceLabel>()`. Nothing in
/// the hot path changes for handlers that don't (or can't cheaply) know their
/// tenant — they simply emit the original, un-`workspace`-labeled series.
///
/// ## Cardinality / safety
///
/// The `workspace` label is added **only** when (a) the request carried this
/// marker and (b) [`per_tenant_http_metrics_enabled`] (the `AERO_PER_TENANT_METRICS`
/// opt-in, default OFF) is on. The label value is a [`WorkspaceId`] rendered as
/// its ULID — a bounded set (one series per active tenant), never anything
/// attacker-controlled. With the flag off, or no marker present, the series are
/// byte-for-byte the same as before this mechanism existed.
#[derive(Debug, Clone, Copy)]
pub struct WorkspaceLabel(pub WorkspaceId);

/// Stamp the response with the [`WorkspaceLabel`] for tenant `ws`, so the
/// [`http_metrics_layer`] can attribute this request's RED metrics to that
/// workspace. A no-op for metrics when `AERO_PER_TENANT_METRICS` is off (the
/// layer just ignores the marker), so handlers can call this unconditionally.
///
/// Returns the (now-stamped) response, for ergonomic use in a handler tail:
/// `Ok(attach_workspace_label(Json(v).into_response(), ws))`.
#[must_use]
pub fn attach_workspace_label(mut response: Response, ws: WorkspaceId) -> Response {
    response.extensions_mut().insert(WorkspaceLabel(ws));
    response
}

/// Whether per-tenant HTTP metrics are enabled, read ONCE from
/// `AERO_PER_TENANT_METRICS` and cached for the process lifetime.
///
/// OPT-IN (default OFF) because a `workspace` label multiplies the HTTP RED
/// series' cardinality by the active-tenant count — a deliberate operator
/// choice. Shares the **same** env switch as the message-throughput per-tenant
/// metric (`aero_im_core`), so one flag governs all per-tenant breakdowns.
/// Resolved at most once so the request hot path never re-reads env.
#[must_use]
pub fn per_tenant_http_metrics_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("AERO_PER_TENANT_METRICS")
            .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
    })
}

/// Axum middleware recording HTTP RED metrics for every request.
///
/// Emits, on completion:
/// * [`names::HTTP_REQUESTS_TOTAL`] labeled `method` + `status`, and
/// * [`names::HTTP_REQUEST_DURATION_SECONDS`] (a histogram) labeled by the
///   matched `route` template.
///
/// When [`per_tenant_http_metrics_enabled`] is on AND the response carries a
/// [`WorkspaceLabel`] (stamped by a handler that cheaply knew its tenant), both
/// signals additionally carry a bounded `workspace` label. Otherwise the series
/// are emitted exactly as before — no marker, no extra dimension.
///
/// Placed in the tower stack so it observes *every* response — including ones
/// short-circuited by inner layers (rate-limit 429s, timeouts) — as long as it
/// sits outside them.
pub async fn http_metrics_layer(request: Request, next: Next) -> Response {
    // Capture the matched route template (bounded cardinality) before consuming
    // the request. Fall back to the raw path only when no route matched (404s);
    // that set is unbounded in theory but such requests don't reach a handler.
    let route = request.extensions().get::<MatchedPath>().map_or_else(
        || request.uri().path().to_owned(),
        |m| m.as_str().to_owned(),
    );
    let method = request.method().clone();

    let start = Instant::now();
    let response = next.run(request).await;
    let elapsed = start.elapsed().as_secs_f64();

    let status_str = response.status().as_u16().to_string();

    // Per-tenant breakdown: only when the operator opted in AND a handler stamped
    // the response with the resolved tenant. Bounded by active-workspace count;
    // absent ⇒ the original, un-`workspace`-labeled series (no unbounded label).
    let workspace = if per_tenant_http_metrics_enabled() {
        response
            .extensions()
            .get::<WorkspaceLabel>()
            .map(|w| w.0.to_string())
    } else {
        None
    };

    if let Some(ws) = workspace.as_deref() {
        metrics::inc_counter_labeled(
            names::HTTP_REQUESTS_TOTAL,
            1,
            &[
                ("method", method.as_str()),
                ("status", &status_str),
                ("workspace", ws),
            ],
        );
        metrics::observe_histogram_labeled(
            names::HTTP_REQUEST_DURATION_SECONDS,
            elapsed,
            &[("route", &route), ("workspace", ws)],
        );
    } else {
        metrics::inc_counter_labeled(
            names::HTTP_REQUESTS_TOTAL,
            1,
            &[("method", method.as_str()), ("status", &status_str)],
        );
        metrics::observe_histogram_labeled(
            names::HTTP_REQUEST_DURATION_SECONDS,
            elapsed,
            &[("route", &route)],
        );
    }
    response
}

/// Records one rate-limit rejection (HTTP 429). Called from the rate-limit
/// middleware when a client is over budget.
pub fn record_rate_limit_rejection() {
    metrics::inc_counter(RATE_LIMIT_REJECTIONS_TOTAL, 1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request as HttpRequest, routing::get, Router};
    use tower::ServiceExt as _; // for `oneshot`

    #[test]
    fn metrics_config_env_defaults_to_enabled_unauthenticated() {
        // Don't touch process env here (parallel tests); assert the default shape.
        let c = MetricsConfig::default();
        assert!(c.enabled);
        assert!(c.bearer_token.is_none());
    }

    #[test]
    fn bearer_matches_requires_exact_token() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, "Bearer s3cret".parse().unwrap());
        assert!(bearer_matches(&h, "s3cret"));
        assert!(!bearer_matches(&h, "other"));
        assert!(!bearer_matches(&h, "s3cre")); // length differs
    }

    #[test]
    fn bearer_matches_accepts_lowercase_scheme_and_trims() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, "bearer  padded ".parse().unwrap());
        assert!(bearer_matches(&h, "padded"));
    }

    #[test]
    fn bearer_matches_rejects_missing_or_malformed_header() {
        let empty = HeaderMap::new();
        assert!(!bearer_matches(&empty, "t"));
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, "Token abc".parse().unwrap());
        assert!(!bearer_matches(&h, "abc"));
    }

    #[test]
    fn tracked_indexes_includes_the_mig0136_message_indexes() {
        let idx = tracked_indexes();
        for expected in [
            "messages_search_tsv_gin",
            "messages_searchable_trgm",
            "messages_embedding_hnsw",
            "messages_room_created_idx",
        ] {
            assert!(
                idx.iter().any(|i| i == expected),
                "missing default index {expected}"
            );
        }
        // Default list is exactly the four hot message indexes (no env extras here).
        assert_eq!(
            idx.len(),
            4,
            "unexpected default tracked-index set: {idx:?}"
        );
    }

    #[test]
    fn index_size_gauge_uses_server_local_name() {
        assert_eq!(INDEX_SIZE_BYTES, "aero_index_size_bytes");
        // The gauge is auto-created on first `set`; assert it renders with the
        // bounded `index` label and shows up in the global exposition.
        metrics::set_gauge_labeled(
            INDEX_SIZE_BYTES,
            4096.0,
            &[("index", "messages_search_tsv_gin")],
        );
        let body = metrics::render_prometheus();
        assert!(
            body.contains(INDEX_SIZE_BYTES),
            "index gauge missing:\n{body}"
        );
        assert!(
            body.contains("index=\"messages_search_tsv_gin\""),
            "index label missing"
        );
    }

    #[test]
    fn pg_health_gauges_use_bounded_schema_labels() {
        metrics::set_gauge_labeled(PG_TABLE_DEAD_TUPLE_RATIO, 0.25, &[("table", "messages")]);
        metrics::set_gauge_labeled(PG_TABLE_SEQ_SCANS_TOTAL, 12.0, &[("table", "messages")]);
        metrics::set_gauge_labeled(
            PG_INDEX_SCANS_TOTAL,
            42.0,
            &[("index", "messages_room_created_idx")],
        );
        metrics::set_gauge(PG_IDLE_IN_TRANSACTION_COUNT, 2.0);
        metrics::set_gauge(PG_IDLE_IN_TRANSACTION_MAX_SECONDS, 90.0);
        let body = metrics::render_prometheus();
        assert!(body.contains("aero_pg_table_dead_tuple_ratio{table=\"messages\"} 0.25"));
        assert!(body.contains("aero_pg_table_seq_scans_total{table=\"messages\"} 12"));
        assert!(body.contains("aero_pg_index_scans_total{index=\"messages_room_created_idx\"} 42"));
        assert!(body.contains("aero_pg_idle_in_transaction_count 2"));
        assert!(body.contains("aero_pg_idle_in_transaction_max_seconds 90"));
    }

    #[test]
    fn rate_limit_rejection_counter_uses_server_local_name() {
        // The emit helper bumps the dedicated, namespaced counter. Assert on the
        // name constant rather than absolute values (global registry, parallel).
        assert_eq!(
            RATE_LIMIT_REJECTIONS_TOTAL,
            "aero_rate_limit_rejections_total"
        );
        record_rate_limit_rejection();
        assert!(metrics::render_prometheus().contains(RATE_LIMIT_REJECTIONS_TOTAL));
    }

    // ---- Router-level tests ----
    //
    // `metrics_handler` extracts only `Arc<MetricsConfig>`, so we can mount it on
    // a tiny `Router<Arc<MetricsConfig>>` and drive it through the *real* axum
    // stack with no DB / NATS / Redis. `tower::ServiceExt::oneshot` +
    // `axum::body::to_bytes` are already in the dependency tree (no new deps).

    fn metrics_router(cfg: MetricsConfig) -> Router {
        Router::new()
            .route("/metrics", get(metrics_handler))
            .with_state(Arc::new(cfg))
    }

    #[tokio::test]
    async fn metrics_route_returns_200_with_prometheus_exposition() {
        // Seed the global registry so the well-known names are present.
        metrics::register_known_metrics(metrics::global());
        let app = metrics_router(MetricsConfig::default());

        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        assert_eq!(ct, PROMETHEUS_CONTENT_TYPE);

        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        // Exposition advertises the key ROADMAP 方向四 signals.
        assert!(
            body.contains(names::WS_CONNECTIONS),
            "ws gauge missing:\n{body}"
        );
        assert!(
            body.contains(names::MESSAGES_SENT_TOTAL),
            "msg counter missing"
        );
        assert!(
            body.contains(names::HTTP_REQUESTS_TOTAL),
            "http counter missing"
        );
        assert!(body.contains("# TYPE aero_ws_connections gauge"));
    }

    #[tokio::test]
    async fn metrics_route_requires_token_when_configured() {
        let cfg = MetricsConfig {
            enabled: true,
            bearer_token: Some("scrape-me".into()),
        };
        let app = metrics_router(cfg);

        // No Authorization header → 401.
        let unauth = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

        // Correct bearer → 200.
        let ok = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/metrics")
                    .header(header::AUTHORIZATION, "Bearer scrape-me")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
    }

    #[test]
    fn attach_workspace_label_stamps_a_bounded_marker() {
        // The marker carries the tenant id; the layer renders it as the ULID
        // (a bounded label value, not free-text).
        let ws = WorkspaceId(ulid::Ulid(0));
        let resp = attach_workspace_label("ok".into_response(), ws);
        let got = resp
            .extensions()
            .get::<WorkspaceLabel>()
            .expect("marker present");
        assert_eq!(got.0, ws);
        assert_eq!(got.0.to_string(), ws.to_string());
    }

    #[test]
    fn unstamped_response_carries_no_workspace_marker() {
        // A handler that didn't stamp the marker ⇒ no `workspace` dimension; the
        // layer falls back to the original series (this is the "no extension ⇒ no
        // label" / unbounded-safe branch).
        let resp = "ok".into_response();
        assert!(resp.extensions().get::<WorkspaceLabel>().is_none());
    }

    #[test]
    fn per_tenant_flag_reflects_env() {
        // The cached reader mirrors the same `AERO_PER_TENANT_METRICS` switch the
        // message-throughput metric uses. Default (unset in CI) is OFF; assert the
        // cached value matches whatever the env says at first read.
        let expected = std::env::var("AERO_PER_TENANT_METRICS")
            .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
        assert_eq!(per_tenant_http_metrics_enabled(), expected);
    }

    #[tokio::test]
    async fn http_metrics_layer_passes_stamped_response_through_unchanged() {
        async fn stamped() -> Response {
            super::attach_workspace_label("ok".into_response(), WorkspaceId(ulid::Ulid(7)))
        }
        // A handler stamps the marker; the layer must still return the response
        // intact (status + marker) regardless of the env flag's cached value.
        let ws = WorkspaceId(ulid::Ulid(7));
        let app: Router = Router::new()
            .route("/probe-ws", get(stamped))
            .layer(axum::middleware::from_fn(http_metrics_layer));
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/probe-ws")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // The marker survives the layer (it only reads it; never strips it).
        assert_eq!(
            resp.extensions().get::<WorkspaceLabel>().map(|w| w.0),
            Some(ws)
        );
        assert!(metrics::render_prometheus().contains(names::HTTP_REQUESTS_TOTAL));
    }

    #[tokio::test]
    async fn http_metrics_layer_records_request() {
        // Mount the layer over a trivial handler and confirm a request flows
        // through (status 200) and the HTTP counter series appears in the global
        // exposition. Assert presence/200, not absolute counts (global registry).
        async fn ok_handler() -> &'static str {
            "ok"
        }
        let app: Router = Router::new()
            .route("/probe", get(ok_handler))
            .layer(axum::middleware::from_fn(http_metrics_layer));

        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/probe")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(metrics::render_prometheus().contains(names::HTTP_REQUESTS_TOTAL));
    }

    // -----------------------------------------------------------------------
    // B5-4 audit-outbox sampler (gauge-name pins + env parse table).
    // -----------------------------------------------------------------------

    /// F-10 — CLI/sampler oracle parity: the sampler's oldest-pending query
    /// is a TEXT mirror of the aero-eng CLI's `Q4_SQL` (table resolved). If
    /// either side drifts (e.g. a recency filter sneaks in, or the CLI
    /// changes its age expression), this reds — the two surfaces must keep
    /// answering the same question.
    #[test]
    fn oldest_pending_q4_mirror_parity_with_the_cli() {
        assert_eq!(
            AUDIT_OUTBOX_OLDEST_PENDING_Q4_MIRROR,
            aero_eng::audit_provision::Q4_SQL.replace(
                "{table}",
                "audit_governance_outbox",
            ),
            "sampler oldest-pending must stay a verbatim Q4_SQL mirror"
        );
    }

    #[test]
    fn audit_outbox_gauge_names_are_pinned() {
        assert_eq!(AUDIT_OUTBOX_STATUS, "aero_audit_outbox_status");
        assert_eq!(
            AUDIT_OUTBOX_OLDEST_PENDING_SECS,
            "aero_audit_outbox_oldest_pending_secs"
        );
        assert_eq!(
            AUDIT_OUTBOX_OLDEST_CLAIMED_SECS,
            "aero_audit_outbox_oldest_claimed_secs"
        );
        assert_eq!(AUDIT_OUTBOX_SAMPLER_UP, "aero_audit_outbox_sampler_up");
        assert_eq!(AUDIT_OUTBOX_DEAD_ROWS, "aero_audit_outbox_dead_rows");
        assert_eq!(
            AUDIT_OUTBOX_SAMPLE_ERRORS_TOTAL,
            "aero_audit_outbox_sample_errors_total"
        );
        assert_eq!(
            AUDIT_OUTBOX_TABLE_SIZE_BYTES,
            "aero_audit_outbox_table_size_bytes"
        );
    }

    /// H-3 env parse table (no PG): absent → default 300; "0" → disable
    /// (None); valid positive → Some; garbage / negative / overflow →
    /// fail-closed disable (None). Env is snapshotted/restored around each
    /// case.
    #[test]
    fn parse_audit_full_sample_secs_pins_the_three_state_table() {
        const KEY: &str = "AERO__SERVER__AUDIT_OUTBOX_FULL_SAMPLE_SECS";
        let saved = std::env::var(KEY).ok();
        let run = |value: Option<&str>| {
            match value {
                Some(v) => std::env::set_var(KEY, v),
                None => std::env::remove_var(KEY),
            }
            let parsed = parse_audit_full_sample_secs();
            match value {
                Some(v) => std::env::set_var(KEY, v),
                None => std::env::remove_var(KEY),
            }
            parsed
        };
        // Absent → default 300.
        assert_eq!(run(None), Some(300));
        // Empty string → default 300 (figment-style tolerance).
        assert_eq!(run(Some("")), Some(300));
        // 0 → disabled.
        assert_eq!(run(Some("0")), None);
        // Valid positive → interval.
        assert_eq!(run(Some("60")), Some(60));
        assert_eq!(run(Some("300")), Some(300));
        // Garbage / negative / overflow → fail-closed disable (never a
        // default that quintuples the most expensive sampler).
        assert_eq!(run(Some("not-a-number")), None);
        assert_eq!(run(Some("-5")), None);
        assert_eq!(run(Some("99999999999999999999999999")), None);
        // Restore.
        match saved {
            Some(v) => std::env::set_var(KEY, v),
            None => std::env::remove_var(KEY),
        }
    }

    /// R2.4 — PG-gated full sampler: seed {1 status=0 backdated 600s, 1
    /// status=2} → `sample_audit_outbox` = {1, 0, false}; full = buckets
    /// {1, 0, 1, 0} + `oldest_pending ≥ 600` (FM11 probe determinism) +
    /// `oldest_claimed None` + a positive table-size byte count. Read-only:
    /// both probes must not mutate any row.
    #[tokio::test]
    #[ignore = "requires live Postgres (DATABASE_URL)"]
    async fn audit_outbox_full_sampler_pins_backdated_pending() {
        use aero_audit_connector::outbox::{StatusBuckets, VerdictProbe};

        let url = std::env::var("DATABASE_URL")
            .expect("DATABASE_URL must point at a throwaway Postgres");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("well-formed DATABASE_URL");
        sqlx::query("TRUNCATE audit_governance_outbox")
            .execute(&pool)
            .await
            .expect("reset the governance outbox");
        let pending_id = uuid::Uuid::new_v4();
        sqlx::query(
            r"INSERT INTO audit_governance_outbox
                    (event_id, payload, status, attempts, priority,
                     available_at, created_at)
              VALUES ($1, $2, 0, 0, 10,
                      clock_timestamp() - make_interval(secs => 600),
                      clock_timestamp() - make_interval(secs => 600))",
        )
        .bind(pending_id)
        .bind(serde_json::json!({
            "event_id": pending_id.to_string(),
            "source_system": "aero-im.source",
            "action": "admin.content.flag",
        }))
        .execute(&pool)
        .await
        .expect("seed backdated pending row");
        let delivered_id = uuid::Uuid::new_v4();
        sqlx::query(
            r"INSERT INTO audit_governance_outbox
                    (event_id, payload, status, attempts)
              VALUES ($1, $2, 2, 0)",
        )
        .bind(delivered_id)
        .bind(serde_json::json!({
            "event_id": delivered_id.to_string(),
            "source_system": "aero-im.source",
            "action": "admin.content.flag",
        }))
        .execute(&pool)
        .await
        .expect("seed delivered row");

        let probe = sample_audit_outbox(&pool)
            .await
            .expect("Tier-1 probe must not fabricate a zero");
        assert_eq!(
            probe,
            VerdictProbe { enqueued: 1, claimed: 0, has_dead: false },
            "Tier-1 probe sees the pending row only"
        );
        let (buckets, oldest_pending, oldest_claimed, table_bytes) =
            sample_audit_outbox_full(&pool).await.expect("Tier-2 full sample");
        assert_eq!(
            buckets,
            StatusBuckets { enqueued: 1, claimed: 0, delivered: 1, dead: 0 },
            "Tier-2 buckets mirror the seeded distribution"
        );
        let pending = oldest_pending.expect("a status-0 row exists");
        assert!(
            pending >= 600,
            "backdated pending row must report age >= 600s (got {pending})"
        );
        assert_eq!(
            oldest_claimed, None,
            "no status-1 row → the oldest-claimed series is absent"
        );
        assert!(table_bytes > 0, "table-size gauge carries a real size");

        // Read-only pin: the rows are untouched by both probes.
        let mut statuses: Vec<i32> = sqlx::query_scalar(
            "SELECT status FROM audit_governance_outbox ORDER BY event_id",
        )
        .fetch_all(&pool)
        .await
        .expect("row statuses");
        statuses.sort_unstable();
        assert_eq!(statuses, vec![0, 2], "probes must never mutate state");

        // P-2 recency-window pin: an OLD dead row (30 days, outside the 7-day
        // sampler window) is visible to Tier-1's unbounded O(1) EXISTS flag
        // (the alert signal) but EXCLUDED from the Tier-2 windowed buckets —
        // the sampler's GROUP BY stays bounded by history growth.
        let old_dead_id = uuid::Uuid::new_v4();
        sqlx::query(
            r"INSERT INTO audit_governance_outbox
                    (event_id, payload, status, attempts,
                     created_at)
              VALUES ($1, $2, 3, 0,
                      clock_timestamp() - make_interval(days => 30))",
        )
        .bind(old_dead_id)
        .bind(serde_json::json!({
            "event_id": old_dead_id.to_string(),
            "source_system": "aero-im.source",
            "action": "admin.content.flag",
        }))
        .execute(&pool)
        .await
        .expect("seed old dead row");
        let probe = sample_audit_outbox(&pool)
            .await
            .expect("Tier-1 probe");
        assert!(
            probe.has_dead,
            "Tier-1's unbounded EXISTS flag must see the old dead row"
        );
        let (buckets, _, _, _) = sample_audit_outbox_full(&pool).await.expect("Tier-2 full sample");
        assert_eq!(
            buckets.dead, 0,
            "Tier-2's recency-bounded buckets exclude rows older than the 7-day window"
        );
        sqlx::query("TRUNCATE audit_governance_outbox")
            .execute(&pool)
            .await
            .expect("reset the governance outbox");
    }
}
