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
            return Err(invalid("contains an empty token (leading, trailing, or doubled '.')"));
        }
        if token == "*" || token == ">" {
            return Err(invalid("contains a wildcard token ('*' or '>'); publish subjects must be concrete"));
        }
        if token.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(invalid("contains whitespace or a control character"));
        }
    }

    Ok(())
}

#[derive(Debug, Clone)]
pub struct JetStreamConfig {
    pub url: String,
    /// If true, declare/update the streams on connect.
    pub bootstrap_streams: bool,
}

impl Default for JetStreamConfig {
    fn default() -> Self {
        Self { url: "nats://127.0.0.1:4222".into(), bootstrap_streams: true }
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

/// Build the pull-consumer config with poison-message redelivery bounds.
///
/// Extracted (and unit-tested) so the DLQ guarantee — finite `max_deliver`, a
/// non-zero `ack_wait` spacing — is verifiable without a live NATS server.
fn poison_safe_pull_config(subject: &str, durable: Option<&str>) -> consumer::pull::Config {
    consumer::pull::Config {
        durable_name: durable.map(str::to_owned),
        filter_subject: subject.to_owned(),
        max_deliver: POISON_MAX_DELIVER,
        ack_wait: POISON_ACK_WAIT,
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

    /// Declare the streams used by the application. Idempotent.
    async fn bootstrap(&self) -> BusResult<()> {
        // Per-room IM messages.
        self.js
            .get_or_create_stream(stream::Config {
                name: "IM_MESSAGES".into(),
                subjects: vec!["im.room.*".into()],
                retention: stream::RetentionPolicy::Limits,
                max_age: std::time::Duration::from_secs(7 * 24 * 3600),
                storage: stream::StorageType::File,
                ..Default::default()
            })
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
        self.msg.ack().await.map_err(|e| BusError::Nats(e.to_string()))
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
    use super::{poison_safe_pull_config, validate_publish_subject, BusError, POISON_MAX_DELIVER};

    #[test]
    fn pull_config_bounds_redelivery_for_poison_messages() {
        let cfg = poison_safe_pull_config("im.room.1", Some("aero-server"));
        assert_eq!(cfg.durable_name.as_deref(), Some("aero-server"));
        assert_eq!(cfg.filter_subject, "im.room.1");
        // A finite, positive `max_deliver` is the whole point: the JetStream
        // default is unlimited redelivery, so a message that repeatedly crashes
        // its consumer before ack would redeliver forever and crash-loop the
        // node's fan-out. Bounding it parks the poison message instead.
        assert!(cfg.max_deliver > 0, "max_deliver must bound redelivery, got {}", cfg.max_deliver);
        assert_eq!(cfg.max_deliver, POISON_MAX_DELIVER);
        // A non-zero ack_wait spaces out redelivery so a crash-looping message
        // can't spin a tight hot loop between the bounded attempts.
        assert!(
            cfg.ack_wait > std::time::Duration::ZERO,
            "ack_wait must be non-zero so redelivery is spaced",
        );
    }

    #[test]
    fn pull_config_without_durable_is_ephemeral() {
        // A `None` durable yields an ephemeral consumer; the redelivery bound
        // still applies so even an ephemeral fan-out can't poison-loop.
        let cfg = poison_safe_pull_config("live.stream.x", None);
        assert!(cfg.durable_name.is_none());
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
                    assert!(reason.contains("empty token"), "subject {subject:?}: {reason}");
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
