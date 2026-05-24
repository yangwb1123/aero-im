//! NATS JetStream implementation of [`EventBus`].

use async_nats::jetstream::{self, consumer, stream};
use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use tracing::{debug, info, instrument};

use crate::traits::{BusError, BusResult, EventBus, Subscription};

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
