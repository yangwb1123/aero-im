//! Cooperative-shutdown primitives shared by long-running background tasks.

use std::sync::Arc;
use std::time::Duration;

use aero_bus::traits::BusResult;
use aero_bus::{EventBus, Subscription};
use futures::{stream::BoxStream, Stream, StreamExt};
use tokio_util::sync::CancellationToken;

pub(crate) type SubscriptionStream = BoxStream<'static, Box<dyn Subscription + Send>>;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NextOrCancelled<T> {
    Item(T),
    Ended,
    Cancelled,
}

/// Establish a bus subscription unless shutdown has already been requested.
pub(crate) async fn subscribe_or_cancelled(
    bus: &Arc<dyn EventBus>,
    subject: &str,
    durable: Option<&str>,
    cancel: &CancellationToken,
) -> Option<BusResult<SubscriptionStream>> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        result = bus.subscribe(subject, durable) => Some(result),
    }
}

/// Wait for one message or shutdown.
///
/// A ready message wins over simultaneous cancellation so work already handed
/// to this process reaches its normal processing and ACK outcome.
pub(crate) async fn next_or_cancelled<S>(
    stream: &mut S,
    cancel: &CancellationToken,
) -> NextOrCancelled<S::Item>
where
    S: Stream + Unpin,
{
    tokio::select! {
        biased;
        item = stream.next() => match item {
            Some(item) => NextOrCancelled::Item(item),
            None => NextOrCancelled::Ended,
        },
        () = cancel.cancelled() => NextOrCancelled::Cancelled,
    }
}

/// Wait for a retry/poll delay without making shutdown wait for that delay.
/// Returns `true` when cancellation won.
pub(crate) async fn delay_or_cancelled(duration: Duration, cancel: &CancellationToken) -> bool {
    tokio::select! {
        biased;
        () = cancel.cancelled() => true,
        () = tokio::time::sleep(duration) => false,
    }
}

/// Wait for an interval tick without making shutdown wait for the next tick.
/// Returns `true` when cancellation won.
pub(crate) async fn tick_or_cancelled(
    interval: &mut tokio::time::Interval,
    cancel: &CancellationToken,
) -> bool {
    tokio::select! {
        biased;
        () = cancel.cancelled() => true,
        _ = interval.tick() => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PendingBus;

    #[async_trait::async_trait]
    impl EventBus for PendingBus {
        async fn publish(&self, _subject: &str, _payload: bytes::Bytes) -> BusResult<()> {
            Ok(())
        }

        async fn subscribe(
            &self,
            _subject: &str,
            _durable: Option<&str>,
        ) -> BusResult<SubscriptionStream> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn pending_subscribe_stops_promptly_when_cancelled() {
        let bus: Arc<dyn EventBus> = Arc::new(PendingBus);
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            subscribe_or_cancelled(&bus, "im.room.*", Some("test"), &task_cancel).await
        });

        cancel.cancel();

        let outcome = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("subscribe should stop promptly")
            .expect("task should not panic");
        assert!(outcome.is_none());
    }

    #[tokio::test]
    async fn pending_stream_stops_promptly_when_cancelled() {
        let cancel = CancellationToken::new();
        let mut stream = futures::stream::pending::<u8>();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move { next_or_cancelled(&mut stream, &task_cancel).await });

        cancel.cancel();

        let outcome = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("stream wait should stop promptly")
            .expect("task should not panic");
        assert_eq!(outcome, NextOrCancelled::Cancelled);
    }

    #[tokio::test]
    async fn ready_item_finishes_before_simultaneous_cancellation() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut stream = futures::stream::iter([7_u8]);

        assert_eq!(
            next_or_cancelled(&mut stream, &cancel).await,
            NextOrCancelled::Item(7)
        );
    }

    #[tokio::test]
    async fn long_delay_stops_promptly_when_cancelled() {
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let task =
            tokio::spawn(
                async move { delay_or_cancelled(Duration::from_secs(60), &task_cancel).await },
            );

        cancel.cancel();

        assert!(tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("delay should stop promptly")
            .expect("task should not panic"));
    }

    #[tokio::test]
    async fn distant_interval_tick_stops_promptly_when_cancelled() {
        let cancel = CancellationToken::new();
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        interval.tick().await;
        let task_cancel = cancel.clone();
        let task =
            tokio::spawn(async move { tick_or_cancelled(&mut interval, &task_cancel).await });

        cancel.cancel();

        assert!(tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("interval wait should stop promptly")
            .expect("task should not panic"));
    }
}
