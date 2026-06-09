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

        let cfg = consumer::pull::Config {
            durable_name: durable.map(str::to_owned),
            filter_subject: subject.to_owned(),
            ..Default::default()
        };

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
    use super::{validate_publish_subject, BusError};

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
