//! Relay for the transactional `stream.live` producer outbox.
//!
//! `PostgreSQL` is the correctness source: every process polls the same queue and
//! `SKIP LOCKED` leases divide work.  A claim independently advances two durable
//! stages:
//!
//! 1. publish the lifecycle event to `JetStream` with the stable `event_id`;
//! 2. materialize every matching outgoing hook into `webhook_delivery_log`
//!    without making an HTTP request.
//!
//! Only then is the outbox row complete.  Partial work is safe to repeat because
//! NATS uses the event id as `Nats-Msg-Id` and webhook rows are unique on
//! `(webhook_id, event_id)`.

use aero_common::{StreamEvent, StreamStatus};
use aero_storage::{
    build_delivery, StreamGoLiveOutboxRepo, StreamGoLiveOutboxRow, WebhookDeliveryRepo, WebhookRepo,
};
use anyhow::{anyhow, Context as _};
use time::{Duration, OffsetDateTime};
use tracing::{debug, warn};

use crate::{live::LiveService, state::AppState};

const CLAIM_LEASE: Duration = Duration::minutes(5);

/// Claim and settle one bounded batch. Individual row failures are re-parked
/// and do not prevent independent rows from progressing.
pub async fn dispatch_batch(state: &AppState, limit: i64) -> Result<usize, sqlx::Error> {
    let repo = StreamGoLiveOutboxRepo::new(state.pg.clone());
    let rows = repo
        .claim_due(OffsetDateTime::now_utc(), CLAIM_LEASE, limit)
        .await?;
    let mut completed = 0;
    for row in rows {
        if finish_claim(state, &repo, &row).await {
            completed += 1;
        }
    }
    Ok(completed)
}

async fn finish_claim(
    state: &AppState,
    repo: &StreamGoLiveOutboxRepo,
    row: &StreamGoLiveOutboxRow,
) -> bool {
    match process_claim(state, repo, row).await {
        Ok(()) => {
            debug!(
                outbox = %row.id,
                event = %row.event_id,
                stream = %row.stream_id,
                attempts = row.attempts,
                "stream.live outbox completed"
            );
            true
        }
        Err(error) => {
            let now = OffsetDateTime::now_utc();
            match repo
                .mark_failed(
                    row.id,
                    row.claim_token,
                    row.attempts,
                    now,
                    &error.to_string(),
                )
                .await
            {
                Ok(true) => warn!(
                    %error,
                    outbox = %row.id,
                    event = %row.event_id,
                    stream = %row.stream_id,
                    attempts = row.attempts,
                    "stream.live outbox failed; retry scheduled"
                ),
                Ok(false) => debug!(
                    %error,
                    outbox = %row.id,
                    event = %row.event_id,
                    "stream.live outbox failure belongs to a superseded claim"
                ),
                Err(mark_error) => warn!(
                    ?mark_error,
                    %error,
                    outbox = %row.id,
                    event = %row.event_id,
                    "stream.live outbox failed and could not be re-parked"
                ),
            }
            false
        }
    }
}

async fn process_claim(
    state: &AppState,
    repo: &StreamGoLiveOutboxRepo,
    row: &StreamGoLiveOutboxRow,
) -> anyhow::Result<()> {
    process_claim_with(&state.live, &state.pg, repo, row).await
}

async fn process_claim_with(
    live: &LiveService,
    pool: &sqlx::PgPool,
    repo: &StreamGoLiveOutboxRepo,
    row: &StreamGoLiveOutboxRow,
) -> anyhow::Result<()> {
    let mut failures = Vec::new();

    if row.nats_published_at.is_none() {
        match live.publish_outboxed_go_live(repo, row).await {
            Ok(()) => {
                let marked = repo
                    .mark_nats_published(row.id, row.claim_token, OffsetDateTime::now_utc())
                    .await
                    .context("record NATS stage")?;
                if !marked {
                    return Err(anyhow!("claim fence lost after NATS publish"));
                }
            }
            Err(error) => failures.push(format!("NATS: {error:#}")),
        }
    }

    // Materialization is independent of NATS availability.  An integration
    // endpoint can therefore enter the unified retry queue even during a broker
    // outage, while the outbox itself remains incomplete until both stages land.
    if row.webhooks_materialized_at.is_none() {
        match materialize_stream_live_webhooks(pool, row).await {
            Ok(()) => {
                let marked = repo
                    .mark_webhooks_materialized(row.id, row.claim_token, OffsetDateTime::now_utc())
                    .await
                    .context("record webhook stage")?;
                if !marked {
                    return Err(anyhow!("claim fence lost after webhook materialization"));
                }
            }
            Err(error) => failures.push(format!("webhooks: {error}")),
        }
    }

    if !failures.is_empty() {
        return Err(anyhow!(failures.join("; ")));
    }
    let completed = repo
        .mark_completed(row.id, row.claim_token, OffsetDateTime::now_utc())
        .await
        .context("complete stream.live outbox")?;
    if !completed {
        return Err(anyhow!(
            "claim fence or stage acknowledgement lost before completion"
        ));
    }
    Ok(())
}

/// Materialize immutable requests for all hooks that existed when this event is
/// relayed. No HTTP is performed here; the unified webhook retry worker owns all
/// endpoint permits, breaker decisions, attempt charging, and remote I/O.
async fn materialize_stream_live_webhooks(
    pool: &sqlx::PgPool,
    row: &StreamGoLiveOutboxRow,
) -> Result<(), sqlx::Error> {
    let Some(room_id) = row.room_id else {
        return Ok(());
    };
    let hooks = WebhookRepo::new(pool.clone())
        .list_outgoing_for_room_event(room_id, "stream.live")
        .await?;
    let deliveries = WebhookDeliveryRepo::new(pool.clone());
    let payload = webhook_payload(row);
    let event_id = row.event_id.to_string();
    for hook in hooks {
        let mut delivery = build_delivery(
            &hook.url,
            &hook.secret,
            &payload,
            row.created_at.unix_timestamp(),
        );
        if let Some(traceparent) = row.traceparent.as_deref() {
            delivery
                .headers
                .push(("traceparent".to_owned(), traceparent.to_owned()));
        }
        deliveries
            .enqueue(
                hook.id,
                Some(&event_id),
                &delivery.body,
                &delivery.retry_headers(),
            )
            .await?;
    }
    Ok(())
}

fn webhook_payload(row: &StreamGoLiveOutboxRow) -> serde_json::Value {
    serde_json::json!({
        "kind": "stream.live",
        "event_id": row.event_id,
        "stream_id": row.stream_id.to_string(),
        "room_id": row.room_id,
        "owner_id": row.owner_id,
        "title": row.title,
    })
}

pub(crate) fn bus_payload(
    row: &StreamGoLiveOutboxRow,
    seq: Option<u64>,
) -> serde_json::Result<Vec<u8>> {
    let event = StreamEvent::Status {
        stream_id: row.stream_id,
        status: StreamStatus::Live,
    };
    let mut payload = serde_json::to_value(event)?;
    if let serde_json::Value::Object(object) = &mut payload {
        object.insert(
            "event_id".to_owned(),
            serde_json::Value::String(row.event_id.to_string()),
        );
        object.insert("owner_id".to_owned(), serde_json::to_value(row.owner_id)?);
        object.insert(
            "title".to_owned(),
            serde_json::Value::String(row.title.clone()),
        );
        if let Some(room_id) = row.room_id {
            object.insert("room_id".to_owned(), serde_json::to_value(room_id)?);
        }
    }
    aero_bus::stamp_seq(&mut payload, seq);
    aero_bus::stamp_traceparent(&mut payload, row.traceparent.as_deref());
    serde_json::to_vec(&payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use aero_bus::traits::BusResult;
    use aero_bus::{EventBus, Subscription};
    use aero_common::{ParticipantId, RoomId, StreamProtocol, WorkspaceId};
    use aero_storage::{
        generate_secret, LiveRepo, NewStream, ParticipantRepo, StreamRepo, WebhookRepo,
    };
    use futures::stream::BoxStream;
    use uuid::Uuid;

    #[derive(Debug)]
    struct Published {
        subject: String,
        payload: bytes::Bytes,
        message_id: String,
    }

    #[derive(Default)]
    struct FakeBus {
        published: Mutex<Vec<Published>>,
    }

    #[async_trait::async_trait]
    impl EventBus for FakeBus {
        async fn publish(&self, subject: &str, payload: bytes::Bytes) -> BusResult<()> {
            self.published
                .lock()
                .expect("publish lock")
                .push(Published {
                    subject: subject.to_owned(),
                    payload,
                    message_id: String::new(),
                });
            Ok(())
        }

        async fn publish_idempotent(
            &self,
            subject: &str,
            payload: bytes::Bytes,
            message_id: &str,
        ) -> BusResult<()> {
            self.published
                .lock()
                .expect("publish lock")
                .push(Published {
                    subject: subject.to_owned(),
                    payload,
                    message_id: message_id.to_owned(),
                });
            Ok(())
        }

        async fn subscribe(
            &self,
            _subject: &str,
            _durable: Option<&str>,
        ) -> BusResult<BoxStream<'static, Box<dyn Subscription + Send>>> {
            Ok(Box::pin(futures::stream::empty()))
        }
    }

    fn row(room_id: Option<RoomId>) -> StreamGoLiveOutboxRow {
        StreamGoLiveOutboxRow {
            id: Uuid::new_v4(),
            event_id: Uuid::new_v4(),
            stream_id: ulid::Ulid::new(),
            room_id,
            owner_id: ParticipantId::new(),
            title: "Crash-safe launch".into(),
            subject: "live.stream.test".into(),
            traceparent: Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into()),
            seq: None,
            attempts: 1,
            claim_token: Uuid::new_v4(),
            available_at: OffsetDateTime::UNIX_EPOCH,
            claimed_at: Some(OffsetDateTime::UNIX_EPOCH),
            nats_published_at: None,
            webhooks_materialized_at: None,
            completed_at: None,
            last_error: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn bus_payload_has_stable_id_snapshot_and_typed_status() {
        let row = row(Some(RoomId::new()));
        let bytes = bus_payload(&row, Some(77)).expect("serialize");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(value["event_id"], row.event_id.to_string());
        assert_eq!(value["owner_id"], row.owner_id.to_string());
        assert_eq!(value["title"], row.title);
        assert_eq!(value["seq"], 77);
        assert_eq!(value["traceparent"], row.traceparent.as_deref().unwrap());
        assert!(matches!(
            serde_json::from_value::<StreamEvent>(value).expect("typed event"),
            StreamEvent::Status {
                stream_id,
                status: StreamStatus::Live
            } if stream_id == row.stream_id
        ));
    }

    #[test]
    fn every_production_ingest_uses_only_atomic_mark_live() {
        let rtmp = include_str!("../../aero-live-rtmp/src/lib.rs");
        let srt = include_str!("../../aero-live-srt/src/lib.rs");
        let whip = include_str!("routes/handlers/whip.rs");
        for (name, source) in [("rtmp", rtmp), ("srt", srt), ("whip", whip)] {
            assert!(
                source.contains(".mark_live("),
                "{name} must reserve through StreamRepo::mark_live"
            );
            assert!(
                !source.contains("notify_go_live")
                    && !source.contains("publish_go_live")
                    && !source.contains("spawn_stream_live"),
                "{name} must not bypass the transactional outbox"
            );
        }
    }

    fn pool() -> sqlx::PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("valid DATABASE_URL")
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn worker_publishes_and_materializes_durably_with_stable_id() {
        let pool = pool();
        let actor = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(actor.to_uuid())
            .bind(format!("stream-outbox-{actor}"))
            .execute(&pool)
            .await
            .expect("participant");
        let workspace = WorkspaceId::new();
        let mut tx = pool.begin().await.expect("begin workspace fixture");
        sqlx::query(
            "INSERT INTO workspaces (id,name,slug,created_by,created_at)
             VALUES ($1,$2,$3,$4,now())",
        )
        .bind(workspace.to_uuid())
        .bind("Stream outbox")
        .bind(format!("stream-outbox-{workspace}"))
        .bind(actor.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("workspace");
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id,participant_id,role)
             VALUES ($1,$2,'owner')",
        )
        .bind(workspace.to_uuid())
        .bind(actor.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("workspace membership");
        let room = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id,kind,created_by,workspace_id,created_at)
             VALUES ($1,'group',$2,$3,now())",
        )
        .bind(room.to_uuid())
        .bind(actor.to_uuid())
        .bind(workspace.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("room");
        sqlx::query(
            "INSERT INTO room_members (room_id,participant_id,role)
             VALUES ($1,$2,'owner')",
        )
        .bind(room.to_uuid())
        .bind(actor.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("room membership");
        tx.commit().await.expect("commit workspace fixture");
        let hooks = WebhookRepo::new(pool.clone());
        let all = hooks
            .create_outgoing(
                room,
                "https://all.example.test/hook",
                &generate_secret(),
                &[],
                Some("all"),
                actor,
            )
            .await
            .expect("all hook");
        let live = hooks
            .create_outgoing(
                room,
                "https://live.example.test/hook",
                &generate_secret(),
                &["stream.live".to_owned()],
                Some("live"),
                actor,
            )
            .await
            .expect("live hook");
        hooks
            .create_outgoing(
                room,
                "https://message.example.test/hook",
                &generate_secret(),
                &["message".to_owned()],
                Some("message"),
                actor,
            )
            .await
            .expect("filtered hook");

        let streams = StreamRepo::new(pool.clone());
        let stream = streams
            .create(NewStream {
                owner_id: actor,
                room_id: Some(room),
                title: "Crash-safe launch".into(),
                protocol: StreamProtocol::Whip,
                stream_key: None,
            })
            .await
            .expect("stream");
        let aero_storage::MarkLiveOutcome::Started(transition) = streams
            .mark_live(stream.id, "/hls/test/index.m3u8")
            .await
            .expect("mark live")
        else {
            panic!("stream must transition");
        };
        let outbox = StreamGoLiveOutboxRepo::new(pool.clone());
        let claimed = outbox
            .claim_by_id(transition.outbox_id, OffsetDateTime::now_utc(), CLAIM_LEASE)
            .await
            .expect("claim")
            .expect("claimed");
        let bus = Arc::new(FakeBus::default());
        let live_service = LiveService::new(
            streams.clone(),
            LiveRepo::new(pool.clone()),
            ParticipantRepo::new(pool.clone()),
            bus.clone(),
        );
        process_claim_with(&live_service, &pool, &outbox, &claimed)
            .await
            .expect("dispatch outbox");

        let completed = outbox
            .get(transition.outbox_id)
            .await
            .expect("read outbox")
            .expect("outbox present");
        assert!(completed.completed_at.is_some());
        assert!(completed.nats_published_at.is_some());
        assert!(completed.webhooks_materialized_at.is_some());
        assert_eq!(completed.seq, Some(1));
        {
            let published = bus.published.lock().expect("publish lock");
            assert_eq!(published.len(), 1);
            assert_eq!(published[0].subject, format!("live.stream.{}", stream.id));
            assert_eq!(published[0].message_id, transition.event_id.to_string());
            let bus_value: serde_json::Value =
                serde_json::from_slice(&published[0].payload).expect("bus JSON");
            assert_eq!(bus_value["event_id"], transition.event_id.to_string());
            assert_eq!(bus_value["seq"], 1);
        }

        // A replay of the webhook materialization stage sees the same stable
        // event id and cannot append duplicate delivery rows.
        materialize_stream_live_webhooks(&pool, &completed)
            .await
            .expect("deduplicated materialization replay");
        let rows: Vec<(Uuid, String, i32, Vec<u8>)> = sqlx::query_as(
            "SELECT webhook_id, status, attempts, request_body
               FROM webhook_delivery_log
              WHERE event_id = $1
              ORDER BY webhook_id",
        )
        .bind(transition.event_id.to_string())
        .fetch_all(&pool)
        .await
        .expect("delivery rows");
        assert_eq!(rows.len(), 2);
        assert!(rows
            .iter()
            .all(|(_, status, attempts, _)| { status == "failed" && *attempts == 0 }));
        assert_eq!(
            rows.iter()
                .map(|(id, ..)| *id)
                .collect::<std::collections::HashSet<_>>(),
            [all.to_uuid(), live.to_uuid()].into_iter().collect()
        );
        for (_, _, _, body) in rows {
            let payload: serde_json::Value = serde_json::from_slice(&body).expect("body JSON");
            assert_eq!(payload["event_id"], transition.event_id.to_string());
            assert_eq!(payload["kind"], "stream.live");
        }

        sqlx::query("DELETE FROM streams WHERE id = $1")
            .bind(uuid::Uuid::from_u128(stream.id.0))
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM stream_go_live_outbox WHERE id = $1")
            .bind(transition.outbox_id)
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(workspace.to_uuid())
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(actor.to_uuid())
            .execute(&pool)
            .await
            .ok();
    }
}
