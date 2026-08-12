//! DB-integration drill helpers for the [`AiWorker`] producer chain — the
//! drills themselves live in [`moderation_finalize_drill_tests`].
//!
//! These tests require a running Postgres with the workspace migrations applied
//! and are `#[ignore]`d so the hermetic suite stays green; run explicitly with:
//!
//! ```bash
//! DATABASE_URL=postgres://…/aero_drill_<suffix> \
//!   cargo test -p aero-ai --lib -- --ignored --test-threads=1 moderation_finalize
//! ```
//!
//! **`DATABASE_URL` is REQUIRED and must name a THROWAWAY database** — the
//! drills TRUNCATE `audit_governance_outbox` and flip the GLOBAL
//! `snaplink_commercial_runtime.enabled` singleton, so the shared dev DB
//! (`postgres://aero:aero_dev_pw@localhost:5432/aero`) must never be reachable.
//! Mirrors `aero-audit-connector/src/pg.rs`'s fail-loud discipline (no default
//! URL — a fallback would TRUNCATE dev data).

#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use aero_common::{Block, MessageId, ParticipantId, RoomKind, WorkspaceId};
use aero_storage::{
    db::PgPool, AiJob, AiJobRepo, MessageRepo, NewMessage, ParticipantRepo, RoomRepo,
    WorkspaceRepo,
};
use sqlx::postgres::PgPoolOptions;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::anthropic::AnthropicClient;
use crate::embed::default_embedder;
use crate::error::{AiError, Result};
use crate::service::AiService;
use crate::transcribe::default_transcriber;
use crate::usage::{
    UsageEvent, UsageOutcome, UsagePersistOutcome, UsageReservation, UsageReserveOutcome,
    UsageSink,
};
use crate::worker::{AiWorker, JobProcessor};

mod moderation_finalize_drill_tests;

// ---------------------------------------------------------------------------
// Pool + fixture plumbing
// ---------------------------------------------------------------------------

/// Fail-loud throwaway-DB pool. **No default URL** — the stock dev DB is
/// `postgres://aero:aero_dev_pw@localhost:5432/aero` (docker-compose), and a
/// silent fallback would TRUNCATE the dev governance outbox and flip the
/// global enforcement singleton.
fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must point at a THROWAWAY Postgres (never the shared dev DB)");
    PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy(&url)
        .expect("connect_lazy never fails on well-formed URL")
}

fn unique_email(prefix: &str) -> String {
    // IDs give us ULIDs through aero-common — no uuid import needed.
    format!("{prefix}-{}@test.local", ParticipantId::new())
}

fn unique_slug(prefix: &str) -> String {
    format!("{prefix}-{}", WorkspaceId::new())
}

/// One drill's scoped fixture chain: participant → workspace → room → message
/// (all inserted with enforcement OFF — the canonical body order).
struct DrillFixture {
    ws: WorkspaceId,
    msg: MessageId,
}

/// Self-isolating start (shared throwaway DB, mirroring im-core
/// `governance_drill_tests::self_isolate`):
/// 1. TRUNCATE the drill-owned outbox (re-runs after a failed test must never
///    corrupt count/parity assertions);
/// 2. orphan guard — the 0241 reconciler runs ahead of EVERY claim batch and
///    would otherwise backfill foreign orphaned `message.moderated` audit rows
///    into the drill's claim set;
/// 3. stale-queued sweep — a panic between `enqueue` and `claim` in a crashed
///    drill run leaves a `queued` job that would poison the next run's
///    `claim(1)` identity assert.
async fn self_isolate(pool: &PgPool) {
    sqlx::query("TRUNCATE audit_governance_outbox")
        .execute(pool)
        .await
        .expect("reset governance outbox");
    sqlx::query(
        "DELETE FROM audit_events
          WHERE action = 'message.moderated'
            AND NOT EXISTS (
                SELECT 1 FROM audit_governance_outbox o WHERE o.event_id = audit_events.id)",
    )
    .execute(pool)
    .await
    .expect("orphaned moderation audit rows removed");
    sqlx::query(
        "DELETE FROM ai_jobs
          WHERE status = 'queued'
            AND scheduled_at < clock_timestamp() - interval '1 minute'",
    )
    .execute(pool)
    .await
    .expect("stale queued ai_jobs swept");
}

/// Gate 1 + Gate 2 + entitlement prerequisites for the 0239 trigger, mirroring
/// im-core `seed_governance_enforcement` (the entitlement projection is
/// mandatory: 0235 metering RAISEs on message INSERT with enforcement on).
/// Returns the binding's `source_system` — the fixture-level pin: a drift
/// between this seeding SQL and the trigger stamp fails the drills at seed
/// time.
async fn seed_governance_enforcement(pool: &PgPool, ws: WorkspaceId) -> String {
    sqlx::query(
        "UPDATE snaplink_commercial_runtime
            SET enabled = TRUE, updated_at = clock_timestamp()
          WHERE singleton",
    )
    .execute(pool)
    .await
    .expect("enable commercial enforcement");
    sqlx::query(
        "INSERT INTO snaplink_commercial_bindings
               (workspace_id, tenant_id, client_id, audit_client_id, source_system,
                revision, enabled)
         VALUES ($1, $2, $3, $4, $5, 1, TRUE)
         ON CONFLICT (workspace_id) DO NOTHING",
    )
    .bind(ws.to_uuid())
    .bind(format!("tenant-{ws}"))
    .bind(format!("client-{ws}"))
    .bind(format!("audit-client-{ws}"))
    .bind(format!("source-{ws}"))
    .execute(pool)
    .await
    .expect("seed enabled binding");
    sqlx::query(
        "INSERT INTO snaplink_entitlement_projections
               (workspace_id, tenant_id, revision, active, im_enabled,
                notifications_enabled, messages_soft, messages_hard,
                messages_unlimited, notifications_soft, notifications_hard,
                notifications_unlimited, effective_at, generated_at)
         VALUES ($1, $2, 1, TRUE, TRUE, TRUE, 0, 0, TRUE, 0, 0, TRUE,
                 clock_timestamp(), clock_timestamp())
         ON CONFLICT (workspace_id) DO NOTHING",
    )
    .bind(ws.to_uuid())
    .bind(format!("tenant-{ws}"))
    .execute(pool)
    .await
    .expect("seed active entitlement projection");
    let source: String = sqlx::query_scalar(
        "SELECT source_system FROM snaplink_commercial_bindings WHERE workspace_id = $1",
    )
    .bind(ws.to_uuid())
    .fetch_one(pool)
    .await
    .expect("read binding source_system back");
    assert_eq!(source, format!("source-{ws}"), "binding source_system pin");
    source
}

/// Restore the fresh-DB default (`enabled = FALSE`, 0235) after a drill that
/// flipped the global singleton on — a drill that leaves it ON makes every
/// later message INSERT in the shared DB raise P0001 (0235 metering).
async fn restore_enforcement_disabled(pool: &PgPool) {
    sqlx::query(
        "UPDATE snaplink_commercial_runtime
            SET enabled = FALSE, updated_at = clock_timestamp()
          WHERE singleton",
    )
    .execute(pool)
    .await
    .expect("restore enforcement disabled (fresh-DB default)");
}

/// Fresh participant/workspace/room/message chain, all inserted BEFORE
/// enforcement is enabled (canonical body order — mandatory for Drill 3,
/// whose binding-less workspace would hit the 0235 metering RAISE on INSERT).
///
/// After insert, pre-seeds the soft-delete SET-clause targets (`embedding`,
/// `searchable_text`) so Drill 1/4's pins are REAL — a fresh row would satisfy
/// `embedding IS NULL` / `searchable_text = ''` trivially.
async fn fixture(pool: &PgPool, prefix: &str) -> DrillFixture {
    let participants = ParticipantRepo::new(pool.clone());
    let participant = participants
        .create_human(aero_storage::participant::NewHuman {
            email: unique_email(prefix),
            display_name: format!("{prefix} drill user"),
            password_hash: "unused-drill-hash".into(),
        })
        .await
        .expect("create drill participant");
    let ws = WorkspaceRepo::new(pool.clone())
        .create(format!("{prefix} WS"), unique_slug(prefix), participant.id)
        .await
        .expect("create drill workspace")
        .id;
    let room = RoomRepo::new(pool.clone())
        .create_in_workspace(
            ws,
            RoomKind::Group,
            Some(format!("{prefix}-room")),
            participant.id,
        )
        .await
        .expect("create drill room")
        .id;
    let msg = MessageRepo::new(pool.clone())
        .insert(NewMessage {
            room_id: room,
            sender_id: participant.id,
            blocks: vec![Block::text("drill content")],
            reply_to: None,
            metadata: serde_json::json!({}),
            expires_at: None,
        })
        .await
        .expect("insert drill message")
        .id;
    sqlx::query(
        "UPDATE messages
            SET embedding = (SELECT array_fill(0::real, ARRAY[1024])::vector),
                searchable_text = 'pre-drill visible text'
          WHERE id = $1",
    )
    .bind(msg.to_uuid())
    .execute(pool)
    .await
    .expect("pre-seed embedding + searchable_text");
    DrillFixture { ws, msg }
}

// ---------------------------------------------------------------------------
// Local stub doubles (no new dev-dependencies — tokio TcpListener, mirroring
// aero-audit-connector/src/stub.rs)
// ---------------------------------------------------------------------------

/// Hand-rolled Anthropic Messages API stub on an ephemeral loopback port.
/// Answers every `POST /v1/messages` with `verdict_text` as a text content
/// block plus a fixed usage object — parses via `ResponseBody`/`ContentBlock`
/// (`crates/aero-ai/src/anthropic.rs`), so `parse_moderation_verdict` yields
/// `Some(reason)` for `BLOCK:` text and `None` (ALLOW) for anything else.
///
/// Lifecycle: per-`#[tokio::test]` runtime — the accept task is aborted on
/// drop; per-connection head-read blocks are bounded by the client's 60s
/// timeout.
struct StubAnthropicServer {
    base_url: String,
    task: tokio::task::JoinHandle<()>,
}

impl StubAnthropicServer {
    async fn start(verdict_text: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub listener");
        let addr = listener.local_addr().expect("stub local addr");
        let body = serde_json::json!({
            "content": [{"type": "text", "text": verdict_text}],
            "usage": {"input_tokens": 10, "output_tokens": 4},
        })
        .to_string();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _peer)) = listener.accept().await else {
                    break;
                };
                let body = body.clone();
                tokio::spawn(async move {
                    // Read head to the terminator, then drain the body per
                    // Content-Length so the client sees a complete exchange.
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 2048];
                    let mut head_end = None;
                    loop {
                        match socket.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                buf.extend_from_slice(&tmp[..n]);
                                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                    head_end = Some(pos + 4);
                                    break;
                                }
                            }
                        }
                    }
                    let Some(head_end) = head_end else {
                        let _ = socket.shutdown().await;
                        return;
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]);
                    let content_length: usize = head
                        .lines()
                        .find_map(|line| {
                            let line = line.to_ascii_lowercase();
                            line.strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse().ok())
                        })
                        .unwrap_or(0);
                    while buf.len() < head_end + content_length {
                        match socket.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                    }
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self {
            base_url: format!("http://{addr}"),
            task,
        }
    }
}

impl Drop for StubAnthropicServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// In-memory [`UsageSink`] mirror of `Capture` (private to
/// `crate::usage::tests` — unreachable from here): reserve → `Acquired`,
/// finalize → `Inserted`, cancel → `true`. The event list lets drills pin the
/// reserve→finalize round-trip (chain-level accounting).
#[derive(Default)]
struct InMemoryUsageSink(Mutex<Vec<UsageEvent>>);

#[async_trait::async_trait]
impl UsageSink for InMemoryUsageSink {
    async fn reserve(&self, event: UsageEvent) -> Result<UsageReserveOutcome, String> {
        let usage_id = event.usage_id;
        self.0.lock().expect("sink lock").push(event);
        Ok(UsageReserveOutcome::Acquired(UsageReservation {
            usage_id,
            token: uuid::Uuid::new_v4(),
        }))
    }

    async fn finalize(
        &self,
        _reservation: UsageReservation,
        _actual_micros: u64,
        _outcome: Option<UsageOutcome>,
    ) -> Result<UsagePersistOutcome, String> {
        Ok(UsagePersistOutcome::Inserted)
    }

    async fn cancel(&self, _reservation: UsageReservation) -> Result<bool, String> {
        Ok(true)
    }
}

/// `AiService` with the stub Anthropic client + in-memory usage sink, repos on
/// the drill pool. Returns the sink too so drills can pin chain-level
/// accounting (reserve + finalize per process).
fn drill_service(pool: &PgPool, stub_url: &str) -> (Arc<AiService>, Arc<InMemoryUsageSink>) {
    let sink = Arc::new(InMemoryUsageSink::default());
    let service = Arc::new(
        AiService::new(
            Some(Arc::new(
                AnthropicClient::new("drill-key", "drill-model").with_base_url(stub_url),
            )),
            default_embedder(),
            default_transcriber(),
            AiJobRepo::new(pool.clone()),
            MessageRepo::new(pool.clone()),
            RoomRepo::new(pool.clone()),
            None,
        )
        .with_usage_sink(sink.clone()),
    );
    (service, sink)
}

/// Timeout-wrapped `JobProcessor::process`. The Anthropic client's own worst
/// case is ~183s (60s × 3 attempts + 1s/2s backoff), which is bounded but far
/// too slow for a drill — surface a stall as an explicit failure instead.
/// Never blind-retry: a timeout is a drill failure, not a second `process`.
async fn run_processed(worker: &AiWorker, job: AiJob) -> Result<serde_json::Value> {
    match tokio::time::timeout(std::time::Duration::from_secs(30), worker.process(job)).await {
        Ok(inner) => inner,
        Err(_) => Err(AiError::Storage(
            "stub/anthropic stall: process exceeded 30s (client worst case is ~183s: \
             60s x 3 attempts + 1s/2s backoff)"
                .into(),
        )),
    }
}
