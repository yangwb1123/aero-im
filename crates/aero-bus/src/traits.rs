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
