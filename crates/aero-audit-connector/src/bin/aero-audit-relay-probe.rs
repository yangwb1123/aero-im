//! DB-free black-box probe for the leased audit relay.
//!
//! The CLI/network seam is already wired to spawn this binary.  Keep this
//! probe deliberately independent from the database-backed drill: each
//! scenario gets a fresh [`FakeOutbox`] and [`StubSink`], while the real
//! [`AuditClient`] and [`AuditRelay`] exercise the loopback HTTP and fenced
//! state-machine paths together.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use aero_audit_connector::client::AuditClient;
use aero_audit_connector::config::{check_lease_invariant, RelayConfig};
use aero_audit_connector::fake::{FakeOutbox, FakeRowSnapshot, FakeStatus, StaticScopeProvisioner};
use aero_audit_connector::outbox::OutboxRepo;
use aero_audit_connector::relay::{audit_backoff, AuditRelay, MAX_BACKOFF_SECONDS};
use aero_audit_connector::stub::{SinkBehavior, StubSink};
use aero_common::AuditId;
use anyhow::{ensure, Context, Result};
use reqwest::Url;
use serde_json::{json, Value};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

const SOURCE_SYSTEM: &str = "aero-im.source";
const EXPECTED_ISSUER: &str = "https://idp.example.test";
const EXPECTED_AUDIENCE: &str = "audit-governance";
const EXPECTED_SCOPE: &str = "audit:event:write";
const EXPECTED_SUBJECT: &str = SOURCE_SYSTEM;
const CLIENT_SECRET: &str = "0123456789abcdef0123456789abcdef";

/// One isolated scenario fixture.  The sink is kept as a field so every
/// scenario can inspect its POST count and always abort its listener before
/// returning, including when an assertion fails.
struct Fixture {
    sink: StubSink,
    fake: Arc<FakeOutbox>,
    relay: AuditRelay,
    id: AuditId,
    t0: OffsetDateTime,
}

impl Fixture {
    async fn new(
        behavior: SinkBehavior,
        request_timeout: StdDuration,
        delivery_lease: StdDuration,
    ) -> Result<Self> {
        let sink = StubSink::start()
            .await
            .context("start loopback audit sink")?;
        sink.set_behavior(behavior).await;

        let fake = Arc::new(FakeOutbox::new());
        let t0 = OffsetDateTime::now_utc();
        fake.set_now(Some(t0)).await;
        let uuid = Uuid::new_v4();
        let id = AuditId::from_uuid(uuid);
        fake.insert(id, claim_payload(uuid), t0).await;

        let config = probe_config(&sink, request_timeout, delivery_lease);
        let client = AuditClient::new(config.clone()).context("build audit client")?;
        let relay = AuditRelay::new(fake.clone(), client, config)
            .with_scope_provisioner(Arc::new(StaticScopeProvisioner::new(true)));

        Ok(Self {
            sink,
            fake,
            relay,
            id,
            t0,
        })
    }

    async fn row(&self) -> Result<FakeRowSnapshot> {
        self.fake
            .row(self.id)
            .await
            .ok_or_else(|| anyhow::anyhow!("probe row disappeared"))
    }

    async fn dispatch(&self) -> Result<usize> {
        self.relay
            .dispatch_batch()
            .await
            .context("dispatch probe batch")
    }

    async fn set_now(&self, now: OffsetDateTime) {
        self.fake.set_now(Some(now)).await;
    }

    fn finish(self, result: Result<()>) -> Result<()> {
        self.sink.shutdown();
        result
    }
}

fn claim_payload(event_id: Uuid) -> Value {
    json!({
        "event_id": event_id.to_string(),
        "source_system": SOURCE_SYSTEM,
        "class": "admin",
        "priority": 0,
        "payload": {"action": "probe"},
    })
}

fn probe_config(
    sink: &StubSink,
    request_timeout: StdDuration,
    delivery_lease: StdDuration,
) -> RelayConfig {
    RelayConfig {
        token_endpoint: Url::parse(&sink.token_url()).expect("stub token URL is valid"),
        events_url: Url::parse(&sink.events_url()).expect("stub events URL is valid"),
        resource: EXPECTED_AUDIENCE.into(),
        client_id: "drill-client".into(),
        client_secret: CLIENT_SECRET.into(),
        expected_iss: EXPECTED_ISSUER.into(),
        expected_aud: EXPECTED_AUDIENCE.into(),
        expected_scope: EXPECTED_SCOPE.into(),
        expected_sub: EXPECTED_SUBJECT.into(),
        source_system: SOURCE_SYSTEM.into(),
        request_timeout,
        delivery_lease,
        poll_interval: StdDuration::from_secs(1),
        shutdown_drain: StdDuration::from_secs(2),
        batch_size: 100,
        concurrency: 4,
        jwks_uri: None,
        provision_freshness: StdDuration::from_secs(300),
    }
}

async fn happy_path() -> Result<()> {
    let fixture = Fixture::new(
        SinkBehavior::default(),
        StdDuration::from_secs(5),
        StdDuration::from_secs(30),
    )
    .await?;
    let result = async {
        ensure!(fixture.dispatch().await? == 1, "expected one claimed row");
        let row = fixture.row().await?;
        ensure!(
            row.status == FakeStatus::Delivered,
            "row did not settle: {row:?}"
        );
        ensure!(row.attempts == 1, "unexpected attempts: {}", row.attempts);
        ensure!(
            fixture.sink.posts() == 1,
            "unexpected POST count: {}",
            fixture.sink.posts()
        );
        ensure!(
            row.claim_token.is_none(),
            "settled row retained claim token"
        );
        ensure!(row.lease_expires_at.is_none(), "settled row retained lease");
        ensure!(
            row.last_error.is_none(),
            "settled row retained error: {:?}",
            row.last_error
        );
        ensure!(
            fixture.dispatch().await? == 0,
            "settled row was claimed again"
        );
        Ok(())
    }
    .await;
    fixture.finish(result)
}

async fn forbidden_403() -> Result<()> {
    let fixture = Fixture::new(
        SinkBehavior {
            events_status: 403,
            ..SinkBehavior::default()
        },
        StdDuration::from_secs(5),
        StdDuration::from_secs(30),
    )
    .await?;
    let result = async {
        ensure!(fixture.dispatch().await? == 1, "expected one claimed row");
        let row = fixture.row().await?;
        ensure!(
            row.status == FakeStatus::Dead,
            "403 did not dead-letter: {row:?}"
        );
        ensure!(
            row.attempts == 1,
            "403 retried unexpectedly: {}",
            row.attempts
        );
        ensure!(
            fixture.sink.posts() == 1,
            "unexpected POST count: {}",
            fixture.sink.posts()
        );
        ensure!(
            row.last_error
                .as_deref()
                .is_some_and(|error| error.contains("403")),
            "403 reason missing: {:?}",
            row.last_error
        );
        ensure!(fixture.dispatch().await? == 0, "dead row was claimed again");
        Ok(())
    }
    .await;
    fixture.finish(result)
}

async fn permanent_case(behavior: SinkBehavior, error_marker: &str) -> Result<()> {
    let fixture = Fixture::new(
        behavior,
        StdDuration::from_secs(5),
        StdDuration::from_secs(30),
    )
    .await?;
    let result = async {
        ensure!(fixture.dispatch().await? == 1, "expected first claim");
        let first = fixture.row().await?;
        ensure!(
            first.status == FakeStatus::Ready,
            "first permanent attempt was terminal: {first:?}"
        );
        ensure!(
            first.attempts == 1,
            "unexpected first attempts: {}",
            first.attempts
        );
        ensure!(
            first.available_at == fixture.t0 + Duration::seconds(1),
            "unexpected first backoff: {}",
            first.available_at
        );
        ensure!(
            first
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains(error_marker)),
            "missing permanent marker {error_marker:?}: {:?}",
            first.last_error
        );

        fixture.set_now(fixture.t0 + Duration::seconds(1)).await;
        ensure!(fixture.dispatch().await? == 1, "expected second claim");
        let second = fixture.row().await?;
        ensure!(
            second.status == FakeStatus::Dead,
            "second permanent attempt did not dead-letter: {second:?}"
        );
        ensure!(
            second.attempts == 2,
            "unexpected terminal attempts: {}",
            second.attempts
        );
        ensure!(
            fixture.sink.posts() == 2,
            "unexpected POST count: {}",
            fixture.sink.posts()
        );
        ensure!(
            fixture.dispatch().await? == 0,
            "dead permanent row was claimed again"
        );
        Ok(())
    }
    .await;
    fixture.finish(result)
}

async fn permanent_422() -> Result<()> {
    permanent_case(
        SinkBehavior {
            events_status: 422,
            ..SinkBehavior::default()
        },
        "Unprocessable",
    )
    .await
}

async fn permanent_409() -> Result<()> {
    permanent_case(
        SinkBehavior {
            events_status: 409,
            ..SinkBehavior::default()
        },
        "Conflict",
    )
    .await
}

async fn receipt_mismatch() -> Result<()> {
    permanent_case(
        SinkBehavior {
            receipt_valid: false,
            ..SinkBehavior::default()
        },
        "ReceiptMismatch",
    )
    .await
}

async fn transient_500() -> Result<()> {
    let expected = [1, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300];
    for (attempt, seconds) in expected.into_iter().enumerate() {
        ensure!(
            audit_backoff(i64::try_from(attempt + 1).expect("small probe index"))
                == Duration::seconds(seconds),
            "backoff mismatch at attempt {}",
            attempt + 1
        );
    }
    ensure!(MAX_BACKOFF_SECONDS == 300, "backoff cap drifted");

    let fixture = Fixture::new(
        SinkBehavior {
            events_status: 500,
            ..SinkBehavior::default()
        },
        StdDuration::from_secs(5),
        StdDuration::from_secs(30),
    )
    .await?;
    let result = async {
        ensure!(fixture.dispatch().await? == 1, "expected first claim");
        let first = fixture.row().await?;
        ensure!(
            first.status == FakeStatus::Ready,
            "500 became terminal: {first:?}"
        );
        ensure!(
            first.attempts == 1,
            "unexpected first attempts: {}",
            first.attempts
        );
        ensure!(
            first.available_at == fixture.t0 + Duration::seconds(1),
            "unexpected first backoff: {}",
            first.available_at
        );

        fixture.set_now(fixture.t0 + Duration::seconds(1)).await;
        ensure!(fixture.dispatch().await? == 1, "expected second claim");
        let second = fixture.row().await?;
        ensure!(
            second.status == FakeStatus::Ready,
            "500 became terminal on retry: {second:?}"
        );
        ensure!(
            second.attempts == 2,
            "unexpected second attempts: {}",
            second.attempts
        );
        ensure!(
            second.available_at == fixture.t0 + Duration::seconds(3),
            "unexpected second backoff: {}",
            second.available_at
        );
        ensure!(
            fixture.sink.posts() == 2,
            "unexpected POST count: {}",
            fixture.sink.posts()
        );
        Ok(())
    }
    .await;
    fixture.finish(result)
}

async fn transient_timeout() -> Result<()> {
    let fixture = Fixture::new(
        SinkBehavior {
            delay_ms: 2_000,
            ..SinkBehavior::default()
        },
        StdDuration::from_millis(200),
        StdDuration::from_secs(5),
    )
    .await?;
    let result = async {
        ensure!(fixture.dispatch().await? == 1, "expected timeout claim");
        let row = fixture.row().await?;
        ensure!(
            row.status == FakeStatus::Ready,
            "timeout became terminal: {row:?}"
        );
        ensure!(
            row.attempts == 1,
            "unexpected timeout attempts: {}",
            row.attempts
        );
        ensure!(
            row.available_at == fixture.t0 + Duration::seconds(1),
            "unexpected timeout backoff: {}",
            row.available_at
        );
        ensure!(
            row.last_error
                .as_deref()
                .is_some_and(|error| error.contains("transport failed")),
            "timeout transport error missing: {:?}",
            row.last_error
        );
        ensure!(
            fixture.sink.posts() == 1,
            "unexpected POST count: {}",
            fixture.sink.posts()
        );
        Ok(())
    }
    .await;
    fixture.finish(result)
}

async fn lease_invariant() -> Result<()> {
    ensure!(
        check_lease_invariant(StdDuration::from_secs(5), StdDuration::from_secs(12)).is_err(),
        "lease boundary 12s must be rejected"
    );
    ensure!(
        check_lease_invariant(StdDuration::from_secs(5), StdDuration::from_secs(13)).is_ok(),
        "lease 13s must exceed the strict boundary"
    );
    Ok(())
}

async fn fencing_stale_token() -> Result<()> {
    let fixture = Fixture::new(
        SinkBehavior::default(),
        StdDuration::from_secs(5),
        StdDuration::from_secs(30),
    )
    .await?;
    let result = async {
        let claims_a = fixture
            .fake
            .claim_due(Duration::seconds(30), 10)
            .await
            .context("claim token A")?;
        ensure!(claims_a.len() == 1, "expected one initial claim");
        let token_a = claims_a[0].claim_token;
        let first = fixture.row().await?;
        ensure!(
            first.status == FakeStatus::Claimed,
            "initial row not claimed: {first:?}"
        );
        ensure!(
            first.attempts == 1,
            "unexpected initial attempts: {}",
            first.attempts
        );
        ensure!(
            first.claim_token == Some(token_a),
            "initial token not persisted"
        );
        ensure!(
            first.lease_expires_at == Some(fixture.t0 + Duration::seconds(30)),
            "initial lease was not minted on the fake clock"
        );

        fixture.set_now(fixture.t0 + Duration::seconds(31)).await;
        fixture.fake.make_due_now(fixture.id).await;
        let claims_b = fixture
            .fake
            .claim_due(Duration::seconds(30), 10)
            .await
            .context("claim token B")?;
        ensure!(claims_b.len() == 1, "expected reclaimed claim");
        let token_b = claims_b[0].claim_token;
        ensure!(
            token_a != token_b,
            "reclaim did not rotate the fencing token"
        );
        let second = fixture.row().await?;
        ensure!(
            second.status == FakeStatus::Claimed,
            "reclaimed row not claimed: {second:?}"
        );
        ensure!(
            second.attempts == 2,
            "unexpected reclaimed attempts: {}",
            second.attempts
        );
        ensure!(
            second.claim_token == Some(token_b),
            "replacement token not persisted"
        );

        ensure!(
            !fixture.fake.settle(fixture.id, token_a).await?,
            "stale settle unexpectedly succeeded"
        );
        ensure!(
            !fixture
                .fake
                .requeue(fixture.id, token_a, 2, "stale")
                .await?,
            "stale requeue unexpectedly succeeded"
        );
        ensure!(
            !fixture
                .fake
                .mark_dead(fixture.id, token_a, 2, "stale")
                .await?,
            "stale mark_dead unexpectedly succeeded"
        );
        ensure!(
            fixture.fake.settle(fixture.id, token_b).await?,
            "fresh token did not settle"
        );
        ensure!(
            fixture.sink.posts() == 0,
            "fencing probe must not perform a POST"
        );
        Ok(())
    }
    .await;
    fixture.finish(result)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() > 1
        || args
            .first()
            .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        eprintln!("usage: aero-audit-relay-probe [mock-url]");
        std::process::exit(2);
    }

    let scenarios = vec![
        ("happy_path", happy_path().await),
        ("forbidden_403", forbidden_403().await),
        ("permanent_422", permanent_422().await),
        ("permanent_409", permanent_409().await),
        ("receipt_mismatch", receipt_mismatch().await),
        ("transient_500", transient_500().await),
        ("transient_timeout", transient_timeout().await),
        ("lease_invariant", lease_invariant().await),
        ("fencing_stale_token", fencing_stale_token().await),
    ];

    let mut failures = 0;
    for (name, result) in scenarios {
        match result {
            Ok(()) => println!("probe: {name}: PASS"),
            Err(error) => {
                failures += 1;
                println!("probe: {name}: FAIL {error:#}");
            }
        }
    }
    if failures > 0 {
        std::process::exit(1);
    }
}
