//! NATS JetStream implementation of [`EventBus`].

use async_nats::jetstream::{self, consumer, stream};
use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use tracing::{debug, info, instrument};

use crate::traits::{BusError, BusResult, EventBus, Subscription};

/// Validate that `subject` is a well-formed NATS subject suitable for
/// *publishing* (a concrete subject — no wildcards).
///
/// NATS treats a subject as a list of `.`-separated tokens. The broker rejects
/// or silently misroutes malformed subjects, and the failure modes are opaque
/// (publish timeouts, "no responders"). Validating up front converts those into
/// a precise [`BusError::InvalidSubject`].
///
/// Rejected:
/// - empty subject;
/// - leading/trailing `.` or any empty token (`im..room`, `im.room.`);
/// - whitespace or ASCII control characters inside a token;
/// - the wildcard tokens `*` and `>` — these are subscribe-side patterns and a
///   message published to them never reaches the intended subscribers.
fn validate_publish_subject(subject: &str) -> Result<(), BusError> {
    let invalid = |reason: &'static str| BusError::InvalidSubject {
        subject: subject.to_owned(),
        reason,
    };

    if subject.is_empty() {
        return Err(invalid("subject is empty"));
    }

    // `split('.')` always yields at least one element; an empty element means a
    // leading/trailing dot or a `..` sequence.
    for token in subject.split('.') {
        if token.is_empty() {
            return Err(invalid(
                "contains an empty token (leading, trailing, or doubled '.')",
            ));
        }
        if token == "*" || token == ">" {
            return Err(invalid(
                "contains a wildcard token ('*' or '>'); publish subjects must be concrete",
            ));
        }
        if token.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(invalid("contains whitespace or a control character"));
        }
    }

    Ok(())
}

/// Validate an idempotent publish request and build its JetStream dedup header.
///
/// `Nats-Msg-Id` is scoped to a stream's duplicate window. Retrying the same
/// logical outbox row with the same id lets the server acknowledge the retry
/// without storing a second message.
fn idempotent_publish_headers(subject: &str, message_id: &str) -> BusResult<async_nats::HeaderMap> {
    validate_publish_subject(subject)?;
    if message_id.is_empty() {
        return Err(BusError::InvalidMessageId {
            reason: "message id is empty",
        });
    }
    let value =
        message_id
            .parse::<async_nats::HeaderValue>()
            .map_err(|_| BusError::InvalidMessageId {
                reason: "message id contains CR or LF",
            })?;
    let mut headers = async_nats::HeaderMap::new();
    headers.insert(async_nats::header::NATS_MESSAGE_ID, value);
    Ok(headers)
}

#[derive(Debug, Clone)]
pub struct JetStreamConfig {
    pub url: String,
    /// If true, declare/update the streams on connect.
    pub bootstrap_streams: bool,
}

impl Default for JetStreamConfig {
    fn default() -> Self {
        Self {
            url: "nats://127.0.0.1:4222".into(),
            bootstrap_streams: true,
        }
    }
}

/// Maximum `JetStream` delivery attempts before a message is parked (no further
/// redelivery). A message that fails this many times — because it repeatedly
/// crashes its consumer *before* ack — is poison; parking it after a bounded
/// number of tries stops one bad message from crash-looping a node's fan-out
/// forever. (Decode-failure poison is already ACK-dropped at the app layer —
/// see `ws.rs` — so this is specifically the broker-side backstop for the
/// consumer-crash class the app layer can't observe.) 16 is high enough that no
/// merely-transient failure is ever discarded.
const POISON_MAX_DELIVER: i64 = 16;

/// How long the broker waits for an ack before redelivering. Set generously —
/// longer than any single consumer's worst-case processing — so a slow-but-
/// healthy consumer is never redelivered mid-process (which would double-process
/// it). The pacing case is the transcribe bot, which acks only AFTER its
/// `ai.transcribe()` call returns (a long voice note → a multi-tens-of-seconds
/// Whisper round-trip); 120s clears that with margin. (The NATS default when
/// unset is just 30s.) It still spaces redelivery enough that a crash-looping
/// message can't spin a tight hot loop. Together with [`POISON_MAX_DELIVER`]
/// this is a backoff-free, bounded redelivery DLQ: at most 16 attempts, each
/// ≥120s apart.
const POISON_ACK_WAIT: std::time::Duration = std::time::Duration::from_secs(120);
/// Remove abandoned per-process Hub cursors after a day. They are realtime-only:
/// a replacement process has no old local sockets and starts at `New`.
const LOCAL_FANOUT_INACTIVE_THRESHOLD: std::time::Duration =
    std::time::Duration::from_secs(24 * 60 * 60);
/// Keep producer message ids for the full retained lifetime of an IM event.
///
/// The producer outbox retries without a delivery deadline, so this cannot be
/// the only idempotency boundary: durable side-effect consumers also persist
/// completed event receipts. Matching the stream's retention horizon still
/// guarantees every duplicate that can coexist with its original retained
/// message is collapsed at the broker before reaching those consumers.
const IM_MESSAGE_DUPLICATE_WINDOW: std::time::Duration =
    std::time::Duration::from_secs(7 * 24 * 60 * 60);

fn im_messages_stream_config() -> stream::Config {
    stream::Config {
        name: "IM_MESSAGES".into(),
        subjects: vec!["im.room.*".into()],
        retention: stream::RetentionPolicy::Limits,
        max_age: std::time::Duration::from_secs(7 * 24 * 3600),
        duplicate_window: IM_MESSAGE_DUPLICATE_WINDOW,
        storage: stream::StorageType::File,
        ..Default::default()
    }
}

/// Build the pull-consumer config with poison-message redelivery bounds.
///
/// Extracted (and unit-tested) so the DLQ guarantee — finite `max_deliver`, a
/// non-zero `ack_wait` spacing — is verifiable without a live NATS server.
fn poison_safe_pull_config(subject: &str, durable: Option<&str>) -> consumer::pull::Config {
    let process_local_fanout = durable.is_some_and(|name| name.starts_with("aero-server-"));
    consumer::pull::Config {
        durable_name: durable.map(str::to_owned),
        // Ephemeral and per-instance Hub consumers are process-local realtime
        // fan-out. A newly-created node has no local sockets that could benefit
        // from replaying days of retained events; clients hydrate history from
        // PG. Once created, the named durable still resumes its cursor normally.
        // Queue/work consumers (bots/workers) retain `All` on first creation.
        deliver_policy: if durable.is_none() || process_local_fanout {
            consumer::DeliverPolicy::New
        } else {
            consumer::DeliverPolicy::All
        },
        filter_subject: subject.to_owned(),
        max_deliver: POISON_MAX_DELIVER,
        ack_wait: POISON_ACK_WAIT,
        inactive_threshold: if process_local_fanout {
            LOCAL_FANOUT_INACTIVE_THRESHOLD
        } else {
            std::time::Duration::ZERO
        },
        ..Default::default()
    }
}

pub struct JetStreamBus {
    js: jetstream::Context,
}

/// Snapshot of a durable consumer's lag — how many messages are waiting to be
/// delivered. Used for the `aero_nats_consumer_pending_messages` gauge.
#[derive(Debug, Clone)]
pub struct ConsumerLag {
    pub stream: String,
    pub consumer: String,
    pub pending: u64,
}

impl JetStreamBus {
    #[instrument(skip(cfg))]
    pub async fn connect(cfg: JetStreamConfig) -> BusResult<Self> {
        let client = async_nats::connect(&cfg.url)
            .await
            .map_err(|e| BusError::Nats(e.to_string()))?;
        let js = jetstream::new(client);
        let bus = Self { js };
        if cfg.bootstrap_streams {
            bus.bootstrap().await?;
        }
        Ok(bus)
    }

    /// Query how many messages are pending (undelivered) for a durable consumer.
    ///
    /// Returns `None` when the stream or consumer does not exist (e.g. before
    /// the AI worker has subscribed for the first time). Used by the metrics
    /// polling task — failures are warn-logged by the caller, not propagated.
    pub async fn consumer_pending(
        &self,
        stream_name: &str,
        consumer_name: &str,
    ) -> BusResult<Option<u64>> {
        let stream = match self.js.get_stream(stream_name).await {
            Ok(s) => s,
            Err(_) => return Ok(None),
        };
        match stream.consumer_info(consumer_name).await {
            Ok(info) => Ok(Some(info.num_pending)),
            Err(_) => Ok(None),
        }
    }

    /// Publish with JetStream's stream-scoped message-id deduplication and return
    /// the broker acknowledgment. The public [`EventBus`] method intentionally
    /// erases the ack details; this helper keeps them available for diagnostics
    /// and the live-NATS integration test.
    async fn publish_idempotent_ack(
        &self,
        subject: &str,
        payload: bytes::Bytes,
        message_id: &str,
    ) -> BusResult<jetstream::publish::PublishAck> {
        let headers = idempotent_publish_headers(subject, message_id)?;
        let ack = self
            .js
            .publish_with_headers(subject.to_owned(), headers, payload)
            .await
            .map_err(|e| BusError::Nats(e.to_string()))?;
        ack.await.map_err(|e| BusError::Nats(e.to_string()))
    }

    /// Declare the streams used by the application. Idempotent.
    async fn bootstrap(&self) -> BusResult<()> {
        // Per-room IM messages.
        let im_messages = im_messages_stream_config();
        self.js
            .get_or_create_stream(im_messages.clone())
            .await
            .map_err(|e| BusError::Nats(e.to_string()))?;
        // `get_or_create_stream` intentionally leaves an existing stream's
        // configuration untouched. Apply the desired config so upgrading an
        // existing cluster receives the longer duplicate window too.
        self.js
            .update_stream(im_messages)
            .await
            .map_err(|e| BusError::Nats(e.to_string()))?;
        info!(stream = "IM_MESSAGES", "declared");

        self.js
            .get_or_create_stream(stream::Config {
                name: "IM_EVENTS".into(),
                subjects: vec!["im.events.>".into()],
                retention: stream::RetentionPolicy::Limits,
                max_age: std::time::Duration::from_secs(30 * 24 * 3600),
                storage: stream::StorageType::File,
                ..Default::default()
            })
            .await
            .map_err(|e| BusError::Nats(e.to_string()))?;
        info!(stream = "IM_EVENTS", "declared");

        self.js
            .get_or_create_stream(stream::Config {
                name: "AI_QUEUE".into(),
                subjects: vec!["ai.queue.*".into()],
                retention: stream::RetentionPolicy::WorkQueue,
                max_age: std::time::Duration::from_secs(24 * 3600),
                storage: stream::StorageType::File,
                ..Default::default()
            })
            .await
            .map_err(|e| BusError::Nats(e.to_string()))?;
        info!(stream = "AI_QUEUE", "declared");

        // Live-stream interactivity (danmaku/gifts/viewers). High-volume and
        // short-lived — a small retention window is plenty for late-joiner
        // backlog and cross-instance fan-out.
        self.js
            .get_or_create_stream(stream::Config {
                name: "LIVE_EVENTS".into(),
                subjects: vec!["live.stream.*".into()],
                retention: stream::RetentionPolicy::Limits,
                max_age: std::time::Duration::from_secs(6 * 3600),
                max_messages: 200_000,
                storage: stream::StorageType::File,
                ..Default::default()
            })
            .await
            .map_err(|e| BusError::Nats(e.to_string()))?;
        info!(stream = "LIVE_EVENTS", "declared");

        Ok(())
    }
}

#[async_trait]
impl EventBus for JetStreamBus {
    #[instrument(skip(self, payload), fields(subject = %subject, bytes = payload.len()))]
    async fn publish(&self, subject: &str, payload: bytes::Bytes) -> BusResult<()> {
        validate_publish_subject(subject)?;
        let ack = self
            .js
            .publish(subject.to_owned(), payload)
            .await
            .map_err(|e| BusError::Nats(e.to_string()))?;
        ack.await.map_err(|e| BusError::Nats(e.to_string()))?;
        debug!("published");
        Ok(())
    }

    #[instrument(
        skip(self, payload, message_id),
        fields(subject = %subject, bytes = payload.len())
    )]
    async fn publish_idempotent(
        &self,
        subject: &str,
        payload: bytes::Bytes,
        message_id: &str,
    ) -> BusResult<()> {
        let ack = self
            .publish_idempotent_ack(subject, payload, message_id)
            .await?;
        debug!(
            stream = %ack.stream,
            sequence = ack.sequence,
            duplicate = ack.duplicate,
            "published idempotently"
        );
        Ok(())
    }

    #[instrument(skip(self), fields(subject, durable))]
    async fn subscribe(
        &self,
        subject: &str,
        durable: Option<&str>,
    ) -> BusResult<BoxStream<'static, Box<dyn Subscription + Send>>> {
        // Determine which stream the subject belongs to.
        let stream_name = if subject.starts_with("im.room.") {
            "IM_MESSAGES"
        } else if subject.starts_with("im.events.") {
            "IM_EVENTS"
        } else if subject.starts_with("ai.queue.") {
            "AI_QUEUE"
        } else if subject.starts_with("live.stream.") {
            "LIVE_EVENTS"
        } else {
            return Err(BusError::Nats(format!("unknown subject prefix: {subject}")));
        };

        let stream = self
            .js
            .get_stream(stream_name)
            .await
            .map_err(|e| BusError::Nats(e.to_string()))?;

        let cfg = poison_safe_pull_config(subject, durable);

        let consumer = stream
            .create_consumer(cfg)
            .await
            .map_err(|e| BusError::Nats(e.to_string()))?;

        let msgs = consumer
            .messages()
            .await
            .map_err(|e| BusError::Nats(e.to_string()))?;

        let mapped = msgs.filter_map(|m| async move {
            match m {
                Ok(msg) => {
                    let sub: Box<dyn Subscription + Send> = Box::new(JsSubscription { msg });
                    Some(sub)
                }
                Err(err) => {
                    tracing::warn!(?err, "nats message error");
                    None
                }
            }
        });

        Ok(mapped.boxed())
    }
}

struct JsSubscription {
    msg: async_nats::jetstream::Message,
}

#[async_trait]
impl Subscription for JsSubscription {
    fn subject(&self) -> &str {
        &self.msg.subject
    }

    fn payload(&self) -> &[u8] {
        &self.msg.payload
    }

    async fn ack(&self) -> BusResult<()> {
        self.msg
            .ack()
            .await
            .map_err(|e| BusError::Nats(e.to_string()))
    }

    async fn nack(&self) -> BusResult<()> {
        self.msg
            .ack_with(async_nats::jetstream::AckKind::Nak(None))
            .await
            .map_err(|e| BusError::Nats(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use async_nats::jetstream::consumer::DeliverPolicy;

    use super::{
        idempotent_publish_headers, im_messages_stream_config, poison_safe_pull_config,
        validate_publish_subject, BusError, JetStreamBus, JetStreamConfig,
        IM_MESSAGE_DUPLICATE_WINDOW, LOCAL_FANOUT_INACTIVE_THRESHOLD, POISON_MAX_DELIVER,
    };

    #[test]
    fn idempotent_publish_builds_exact_nats_message_id_header() {
        let headers =
            idempotent_publish_headers("im.room.1", "outbox-01").expect("publish headers");
        assert_eq!(
            headers
                .get(async_nats::header::NATS_MESSAGE_ID)
                .map(async_nats::HeaderValue::as_str),
            Some("outbox-01")
        );
    }

    #[test]
    fn idempotent_publish_reuses_subject_validation() {
        let err = idempotent_publish_headers("im.room.*", "outbox-01").unwrap_err();
        assert!(matches!(err, BusError::InvalidSubject { .. }));
    }

    #[test]
    fn idempotent_publish_rejects_unsafe_message_ids() {
        assert!(matches!(
            idempotent_publish_headers("im.room.1", ""),
            Err(BusError::InvalidMessageId { .. })
        ));
        assert!(matches!(
            idempotent_publish_headers("im.room.1", "outbox\r\ninjected"),
            Err(BusError::InvalidMessageId { .. })
        ));
    }

    #[test]
    fn im_stream_duplicate_window_covers_outbox_retries() {
        let config = im_messages_stream_config();
        assert_eq!(config.duplicate_window, IM_MESSAGE_DUPLICATE_WINDOW);
        assert_eq!(config.duplicate_window, config.max_age);
    }

    #[tokio::test]
    #[ignore = "requires a live NATS JetStream at AERO__NATS__URL"]
    async fn live_nats_deduplicates_same_message_id() {
        let url = std::env::var("AERO__NATS__URL").expect("AERO__NATS__URL");
        let bus = JetStreamBus::connect(JetStreamConfig {
            url,
            bootstrap_streams: true,
        })
        .await
        .expect("connect JetStream");
        let mut im_stream = bus.js.get_stream("IM_MESSAGES").await.expect("IM_MESSAGES");
        let info = im_stream.info().await.expect("IM_MESSAGES info");
        assert_eq!(
            info.config.duplicate_window, IM_MESSAGE_DUPLICATE_WINDOW,
            "bootstrap must update an existing stream, not only fresh installs"
        );
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let subject = format!("im.events.idempotent.{nonce}");
        let message_id = format!("aero-bus-test-{nonce}");
        let payload = bytes::Bytes::from_static(b"one logical event");

        let first = bus
            .publish_idempotent_ack(&subject, payload.clone(), &message_id)
            .await
            .expect("first publish");
        let retry = bus
            .publish_idempotent_ack(&subject, payload, &message_id)
            .await
            .expect("retry publish");

        assert!(!first.duplicate);
        assert!(retry.duplicate);
        assert_eq!(retry.sequence, first.sequence);

        if let Ok(stream) = bus.js.get_stream("IM_EVENTS").await {
            let _ = stream.purge().filter(subject).await;
        }
    }
    #[test]
    fn pull_config_bounds_redelivery_for_poison_messages() {
        let cfg = poison_safe_pull_config("im.room.1", Some("aero-server"));
        assert_eq!(cfg.durable_name.as_deref(), Some("aero-server"));
        assert_eq!(cfg.filter_subject, "im.room.1");
        // A finite, positive `max_deliver` is the whole point: the JetStream
        // default is unlimited redelivery, so a message that repeatedly crashes
        // its consumer before ack would redeliver forever and crash-loop the
        // node's fan-out. Bounding it parks the poison message instead.
        assert!(
            cfg.max_deliver > 0,
            "max_deliver must bound redelivery, got {}",
            cfg.max_deliver
        );
        assert_eq!(cfg.max_deliver, POISON_MAX_DELIVER);
        // A non-zero ack_wait spaces out redelivery so a crash-looping message
        // can't spin a tight hot loop between the bounded attempts.
        assert!(
            cfg.ack_wait > std::time::Duration::ZERO,
            "ack_wait must be non-zero so redelivery is spaced",
        );
    }

    #[test]
    fn pull_config_with_durable_preserves_all_delivery_policy() {
        let cfg = poison_safe_pull_config("im.room.1", Some("aero-bot"));
        assert_eq!(cfg.deliver_policy, DeliverPolicy::All);
    }

    #[test]
    fn per_instance_hub_durable_starts_at_new_events() {
        let cfg = poison_safe_pull_config("im.room.1", Some("aero-server-node-a-deadbeef"));
        assert_eq!(cfg.deliver_policy, DeliverPolicy::New);
        assert_eq!(cfg.inactive_threshold, LOCAL_FANOUT_INACTIVE_THRESHOLD);
    }

    #[test]
    fn pull_config_without_durable_is_ephemeral() {
        // A `None` durable yields an ephemeral consumer that starts with events
        // published after it was created; retained history is not replayed.
        // The redelivery bound still applies so even an ephemeral fan-out can't
        // poison-loop.
        let cfg = poison_safe_pull_config("live.stream.x", None);
        assert!(cfg.durable_name.is_none());
        assert_eq!(cfg.deliver_policy, DeliverPolicy::New);
        assert!(cfg.max_deliver > 0);
    }

    #[test]
    fn accepts_well_formed_concrete_subjects() {
        for subject in [
            "im.room.42",
            "im.events.message.created",
            "ai.queue.summarize",
            "live.stream.abc123",
            "a",
            "a.b.c.d.e.f",
        ] {
            assert!(
                validate_publish_subject(subject).is_ok(),
                "expected {subject:?} to be accepted",
            );
        }
    }

    #[test]
    fn rejects_empty_subject() {
        let err = validate_publish_subject("").unwrap_err();
        assert!(matches!(err, BusError::InvalidSubject { .. }));
    }

    #[test]
    fn rejects_empty_tokens() {
        // Leading dot, trailing dot, and a doubled dot all produce an empty token.
        for subject in [".im.room.1", "im.room.1.", "im..room.1", "."] {
            let err = validate_publish_subject(subject).unwrap_err();
            match err {
                BusError::InvalidSubject { reason, .. } => {
                    assert!(
                        reason.contains("empty token"),
                        "subject {subject:?}: {reason}"
                    );
                }
                other => panic!("subject {subject:?}: unexpected error {other:?}"),
            }
        }
    }

    #[test]
    fn rejects_wildcard_tokens_on_publish() {
        // `*` and `>` are subscribe-side patterns; publishing to them silently
        // misroutes, so they must be rejected as a whole token.
        for subject in ["im.room.*", "im.events.>", "*", ">"] {
            let err = validate_publish_subject(subject).unwrap_err();
            match err {
                BusError::InvalidSubject { reason, .. } => {
                    assert!(reason.contains("wildcard"), "subject {subject:?}: {reason}");
                }
                other => panic!("subject {subject:?}: unexpected error {other:?}"),
            }
        }
        // A literal `*` embedded in a larger token is NOT a wildcard token; NATS
        // treats wildcards positionally, so only a standalone `*`/`>` is rejected
        // here. (`a*b` is an unusual-but-legal token.)
        assert!(validate_publish_subject("im.room.a*b").is_ok());
    }

    #[test]
    fn rejects_whitespace_and_control_chars() {
        for subject in ["im.room. 1", "im.room.\t1", "im room.1", "im.room.1\n"] {
            let err = validate_publish_subject(subject).unwrap_err();
            match err {
                BusError::InvalidSubject { reason, .. } => {
                    assert!(
                        reason.contains("whitespace") || reason.contains("control"),
                        "subject {subject:?}: {reason}",
                    );
                }
                other => panic!("subject {subject:?}: unexpected error {other:?}"),
            }
        }
    }

    #[test]
    fn error_message_includes_the_offending_subject() {
        let err = validate_publish_subject("bad subject").unwrap_err();
        let rendered = err.to_string();
        assert!(rendered.contains("bad subject"), "rendered: {rendered}");
    }
}
