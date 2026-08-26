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
        aero_eng::audit_provision::Q4_SQL.replace("{table}", "audit_governance_outbox",),
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

    let url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must point at a throwaway Postgres");
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
        VerdictProbe {
            enqueued: 1,
            claimed: 0,
            has_dead: false
        },
        "Tier-1 probe sees the pending row only"
    );
    let (buckets, oldest_pending, oldest_claimed, table_bytes) = sample_audit_outbox_full(&pool)
        .await
        .expect("Tier-2 full sample");
    assert_eq!(
        buckets,
        StatusBuckets {
            enqueued: 1,
            claimed: 0,
            delivered: 1,
            dead: 0
        },
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
    let mut statuses: Vec<i32> =
        sqlx::query_scalar("SELECT status FROM audit_governance_outbox ORDER BY event_id")
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
    let probe = sample_audit_outbox(&pool).await.expect("Tier-1 probe");
    assert!(
        probe.has_dead,
        "Tier-1's unbounded EXISTS flag must see the old dead row"
    );
    let (buckets, _, _, _) = sample_audit_outbox_full(&pool)
        .await
        .expect("Tier-2 full sample");
    assert_eq!(
        buckets.dead, 0,
        "Tier-2's recency-bounded buckets exclude rows older than the 7-day window"
    );
    sqlx::query("TRUNCATE audit_governance_outbox")
        .execute(&pool)
        .await
        .expect("reset the governance outbox");
}
