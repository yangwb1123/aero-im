//! Generic publish/subscribe traits decoupled from NATS specifics.
//!
//! Callers should hold `Arc<dyn EventBus>` so the implementation can be swapped
//! (e.g. an in-memory bus for tests).

use async_trait::async_trait;
use futures::stream::BoxStream;

#[derive(Debug, thiserror::Error)]
pub enum BusError {
    #[error("nats: {0}")]
    Nats(String),
    #[error("serialize: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// The idempotency key cannot be represented safely as a NATS header value.
    #[error("invalid message id: {reason}")]
    InvalidMessageId { reason: &'static str },
    /// The subject is not a publishable NATS subject (empty, contains an empty
    /// token, whitespace, control characters, or a `*`/`>` wildcard). Surfaced
    /// before the bytes ever reach the broker so callers get a precise reason
    /// instead of an opaque publish timeout or a silently misrouted message.
    #[error("invalid subject {subject:?}: {reason}")]
    InvalidSubject {
        subject: String,
        reason: &'static str,
    },
}

pub type BusResult<T> = Result<T, BusError>;

/// A delivered message on a subscription. `ack()` must be called once processing succeeds
/// for work-queue style consumers; broadcast subscribers can ignore it.
#[async_trait]
pub trait Subscription: Send {
    fn subject(&self) -> &str;
    fn payload(&self) -> &[u8];
    async fn ack(&self) -> BusResult<()>;
    async fn nack(&self) -> BusResult<()>;
}

#[async_trait]
pub trait EventBus: Send + Sync {
    /// Publish raw bytes to a subject. Must be persisted on JetStream.
    async fn publish(&self, subject: &str, payload: bytes::Bytes) -> BusResult<()>;

    /// Publish one logical message with a stable idempotency key.
    ///
    /// Backends without broker-level deduplication fall back to [`Self::publish`],
    /// preserving compatibility with lightweight fakes. Concrete durable
    /// backends should override this method and use `message_id` to collapse
    /// retries of the same logical publish.
    async fn publish_idempotent(
        &self,
        subject: &str,
        payload: bytes::Bytes,
        _message_id: &str,
    ) -> BusResult<()> {
        self.publish(subject, payload).await
    }

    /// Publish a JSON-serializable event.
    ///
    /// `Self: Sized` so the trait remains dyn-compatible — callers holding
    /// `Arc<dyn EventBus>` should serialize externally and call [`publish`].
    /// Concrete impls (`JetStreamBus`) get the ergonomic shortcut.
    async fn publish_json<T: serde::Serialize + Send + Sync>(
        &self,
        subject: &str,
        value: &T,
    ) -> BusResult<()>
    where
        Self: Sized,
    {
        let bytes = serde_json::to_vec(value)?;
        self.publish(subject, bytes.into()).await
    }

    /// Subscribe to a subject pattern with a durable consumer name (or ephemeral if empty).
    ///
    /// Returns a stream of `Subscription` handles. Each must be `ack()`ed to advance
    /// the consumer cursor (broadcast subscribers can ignore acks).
    async fn subscribe(
        &self,
        subject: &str,
        durable: Option<&str>,
    ) -> BusResult<BoxStream<'static, Box<dyn Subscription + Send>>>;
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Default)]
    struct FakeBus {
        published: Mutex<Vec<(String, bytes::Bytes)>>,
    }

    #[async_trait]
    impl EventBus for FakeBus {
        async fn publish(&self, subject: &str, payload: bytes::Bytes) -> BusResult<()> {
            self.published
                .lock()
                .expect("published mutex")
                .push((subject.to_owned(), payload));
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

    #[tokio::test]
    async fn object_safe_default_idempotent_publish_falls_back_to_publish() {
        let fake = Arc::new(FakeBus::default());
        let bus: Arc<dyn EventBus> = fake.clone();
        let payload = bytes::Bytes::from_static(b"payload");

        bus.publish_idempotent("im.room.1", payload.clone(), "outbox-1")
            .await
            .expect("default publish");

        assert_eq!(
            *fake.published.lock().expect("published mutex"),
            vec![("im.room.1".to_owned(), payload)]
        );
    }
}
