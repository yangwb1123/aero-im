//! Bounded, process-wide concurrency control for outgoing webhook HTTP calls.
//!
//! Both the first-delivery dispatcher and the retry loop share one limiter. The
//! endpoint permit is acquired before the global permit so a backlog for one
//! slow URL cannot occupy every global slot while merely waiting.

use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, OnceLock},
    time::Instant,
};

use aero_common::metrics::{self, names};
use aero_storage::{Delivery, DeliveryResponse, WebhookSender};
use dashmap::DashMap;
use futures::{stream, StreamExt as _};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

const DEFAULT_GLOBAL_CONCURRENCY: usize = 32;
const DEFAULT_ENDPOINT_CONCURRENCY: usize = 4;
const MAX_GLOBAL_CONCURRENCY: usize = 256;
const MAX_ENDPOINT_CONCURRENCY: usize = 64;

/// Fixed, bounded metric label distinguishing first attempts from retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeliveryStage {
    Initial,
    Retry,
}

impl DeliveryStage {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::Retry => "retry",
        }
    }
}

/// Fixed outcome vocabulary. Keeping this an enum prevents endpoint URLs,
/// webhook ids, error strings, or HTTP status values entering metric labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeliveryOutcome {
    Success,
    HttpError,
    TransportError,
    BreakerOpen,
    Deduplicated,
    RecordError,
    TargetUnavailable,
    Cancelled,
}

impl DeliveryOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::HttpError => "http_error",
            Self::TransportError => "transport_error",
            Self::BreakerOpen => "breaker_open",
            Self::Deduplicated => "deduplicated",
            Self::RecordError => "record_error",
            Self::TargetUnavailable => "target_unavailable",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Operator-tunable concurrency limits. Values are clamped so a typo cannot
/// create an unbounded number of HTTP requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DeliveryConcurrency {
    global: usize,
    endpoint: usize,
}

impl DeliveryConcurrency {
    #[cfg(test)]
    pub(super) fn new(global: usize, endpoint: usize) -> Self {
        Self::sanitised(global, endpoint)
    }

    fn from_env() -> Self {
        let global = env_limit(
            "AERO_WEBHOOK_GLOBAL_CONCURRENCY",
            DEFAULT_GLOBAL_CONCURRENCY,
            MAX_GLOBAL_CONCURRENCY,
        );
        let endpoint = env_limit(
            "AERO_WEBHOOK_ENDPOINT_CONCURRENCY",
            DEFAULT_ENDPOINT_CONCURRENCY,
            MAX_ENDPOINT_CONCURRENCY,
        );
        Self::sanitised(global, endpoint)
    }

    fn sanitised(global: usize, endpoint: usize) -> Self {
        let global = global.clamp(1, MAX_GLOBAL_CONCURRENCY);
        let endpoint = endpoint.clamp(1, MAX_ENDPOINT_CONCURRENCY).min(global);
        Self { global, endpoint }
    }
}

fn env_limit(name: &str, default: usize, maximum: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .map_or(default, |value| value.min(maximum))
}

/// Shared two-level semaphore plus a lazily-created endpoint semaphore map.
pub(super) struct DeliveryLimiter {
    config: DeliveryConcurrency,
    global: Arc<Semaphore>,
    endpoints: DashMap<String, Arc<Semaphore>>,
}

impl DeliveryLimiter {
    pub(super) fn new(config: DeliveryConcurrency) -> Self {
        Self {
            global: Arc::new(Semaphore::new(config.global)),
            endpoints: DashMap::new(),
            config,
        }
    }

    pub(super) const fn global_limit(&self) -> usize {
        self.config.global
    }

    pub(super) const fn endpoint_limit(&self) -> usize {
        self.config.endpoint
    }

    /// Wait for this endpoint and the process-wide HTTP permit. Cancellation
    /// wins ties, so retry callers can release an untouched durable claim.
    pub(super) async fn acquire(
        &self,
        endpoint: &str,
        stage: DeliveryStage,
        cancel: &CancellationToken,
    ) -> Option<DeliveryPermit> {
        let _queued = QueuedGauge::new(stage);
        let endpoint_semaphore = self
            .endpoints
            .entry(endpoint.to_owned())
            .or_insert_with(|| Arc::new(Semaphore::new(self.config.endpoint)))
            .clone();

        let endpoint_permit = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                record_outcome(stage, DeliveryOutcome::Cancelled);
                return None;
            }
            permit = endpoint_semaphore.acquire_owned() => {
                let Ok(permit) = permit else {
                    record_outcome(stage, DeliveryOutcome::Cancelled);
                    return None;
                };
                permit
            }
        };
        let global_permit = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                record_outcome(stage, DeliveryOutcome::Cancelled);
                return None;
            }
            permit = self.global.clone().acquire_owned() => {
                let Ok(permit) = permit else {
                    record_outcome(stage, DeliveryOutcome::Cancelled);
                    return None;
                };
                permit
            }
        };
        if cancel.is_cancelled() {
            record_outcome(stage, DeliveryOutcome::Cancelled);
            return None;
        }
        Some(DeliveryPermit {
            stage,
            _endpoint: endpoint_permit,
            _global: global_permit,
        })
    }

    /// Remove endpoint semaphores that are no longer referenced by an active or
    /// waiting delivery. Called after each batch to bound churn over revoked URLs.
    pub(super) fn prune_idle_endpoints(&self) {
        self.endpoints
            .retain(|_, semaphore| Arc::strong_count(semaphore) > 1);
    }

    #[cfg(test)]
    fn endpoint_entry_count(&self) -> usize {
        self.endpoints.len()
    }

    #[cfg(test)]
    fn available_global_permits(&self) -> usize {
        self.global.available_permits()
    }
}

/// A pair of already-acquired permits. A durable delivery claim may exist before
/// this value is obtained, but `begin_attempt` increments the HTTP-attempt count
/// only after these permits are held. Cancellation while queued therefore
/// reparks the claim without consuming an attempt.
pub(super) struct DeliveryPermit {
    stage: DeliveryStage,
    _endpoint: OwnedSemaphorePermit,
    _global: OwnedSemaphorePermit,
}

impl DeliveryPermit {
    /// Execute exactly one sender call. `ReqwestSender` retains its existing
    /// ten-second request timeout; this layer only controls concurrency.
    pub(super) async fn deliver<S: WebhookSender + ?Sized>(
        self,
        sender: &S,
        delivery: &Delivery,
    ) -> Result<DeliveryResponse, String> {
        let _in_flight = InFlightGauge::new(self.stage);
        let started = Instant::now();
        let result = sender.deliver(delivery).await;
        metrics::observe_histogram_labeled(
            names::WEBHOOK_DELIVERY_DURATION_SECONDS,
            started.elapsed().as_secs_f64(),
            &[("stage", self.stage.as_str())],
        );
        let outcome = match &result {
            Ok(response) if (200..300).contains(&response.status) => DeliveryOutcome::Success,
            Ok(_) => DeliveryOutcome::HttpError,
            Err(_) => DeliveryOutcome::TransportError,
        };
        record_outcome(self.stage, outcome);
        result
    }
}

struct QueuedGauge(DeliveryStage);

impl QueuedGauge {
    fn new(stage: DeliveryStage) -> Self {
        metrics::add_gauge_labeled(
            names::WEBHOOK_DELIVERY_QUEUE_DEPTH,
            1.0,
            &[("stage", stage.as_str())],
        );
        Self(stage)
    }
}

impl Drop for QueuedGauge {
    fn drop(&mut self) {
        metrics::add_gauge_labeled(
            names::WEBHOOK_DELIVERY_QUEUE_DEPTH,
            -1.0,
            &[("stage", self.0.as_str())],
        );
    }
}

struct InFlightGauge(DeliveryStage);

impl InFlightGauge {
    fn new(stage: DeliveryStage) -> Self {
        metrics::add_gauge_labeled(
            names::WEBHOOK_DELIVERY_IN_FLIGHT,
            1.0,
            &[("stage", stage.as_str())],
        );
        Self(stage)
    }
}

impl Drop for InFlightGauge {
    fn drop(&mut self) {
        metrics::add_gauge_labeled(
            names::WEBHOOK_DELIVERY_IN_FLIGHT,
            -1.0,
            &[("stage", self.0.as_str())],
        );
    }
}

pub(super) fn record_outcome(stage: DeliveryStage, outcome: DeliveryOutcome) {
    metrics::inc_counter_labeled(
        names::WEBHOOK_DELIVERY_OUTCOMES_TOTAL,
        1,
        &[("stage", stage.as_str()), ("outcome", outcome.as_str())],
    );
}

static PRODUCTION_LIMITER: OnceLock<Arc<DeliveryLimiter>> = OnceLock::new();

/// One limiter shared by first deliveries and retries in this process.
pub(super) fn production_limiter() -> Arc<DeliveryLimiter> {
    PRODUCTION_LIMITER
        .get_or_init(|| Arc::new(DeliveryLimiter::new(DeliveryConcurrency::from_env())))
        .clone()
}

/// Run endpoint groups concurrently while bounding both the number of active
/// groups and the number of scheduled jobs within each group. The limiter still
/// gates the actual HTTP call, including across simultaneous dispatcher/retry
/// batches.
pub(super) async fn for_each_endpoint_bounded<T, F, Fut>(
    limiter: &DeliveryLimiter,
    items: Vec<(String, T)>,
    work: F,
) where
    T: Send,
    F: Fn(T) -> Fut + Clone,
    Fut: Future<Output = ()> + Send,
{
    let mut groups: HashMap<String, Vec<T>> = HashMap::new();
    for (endpoint, item) in items {
        groups.entry(endpoint).or_default().push(item);
    }
    stream::iter(groups.into_values())
        .for_each_concurrent(Some(limiter.global_limit()), |group| {
            let work = work.clone();
            async move {
                stream::iter(group)
                    .for_each_concurrent(Some(limiter.endpoint_limit()), work)
                    .await;
            }
        })
        .await;
    limiter.prune_idle_endpoints();
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use async_trait::async_trait;
    use tokio::sync::Notify;

    use super::*;

    #[derive(Default)]
    struct Gate {
        open: AtomicBool,
        notify: Notify,
    }

    impl Gate {
        async fn wait(&self) {
            loop {
                let notified = self.notify.notified();
                if self.open.load(Ordering::Acquire) {
                    return;
                }
                notified.await;
            }
        }

        fn open(&self) {
            self.open.store(true, Ordering::Release);
            self.notify.notify_waiters();
        }
    }

    struct BlockingSender {
        active: AtomicUsize,
        maximum: AtomicUsize,
        gate: Gate,
    }

    impl BlockingSender {
        fn new() -> Self {
            Self {
                active: AtomicUsize::new(0),
                maximum: AtomicUsize::new(0),
                gate: Gate::default(),
            }
        }
    }

    struct ActiveCall<'a>(&'a AtomicUsize);

    impl Drop for ActiveCall<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::AcqRel);
        }
    }

    #[async_trait]
    impl WebhookSender for BlockingSender {
        async fn deliver(&self, _delivery: &Delivery) -> Result<DeliveryResponse, String> {
            let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
            self.maximum.fetch_max(active, Ordering::AcqRel);
            let _active = ActiveCall(&self.active);
            self.gate.wait().await;
            Ok(DeliveryResponse::new(204))
        }
    }

    fn delivery(url: String) -> Delivery {
        Delivery {
            url,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    async fn wait_for_active(sender: &BlockingSender, expected: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while sender.active.load(Ordering::Acquire) < expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("deliveries did not become active");
    }

    fn spawn_delivery(
        limiter: Arc<DeliveryLimiter>,
        sender: Arc<BlockingSender>,
        url: String,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let cancel = CancellationToken::new();
            let permit = limiter
                .acquire(&url, DeliveryStage::Initial, &cancel)
                .await
                .expect("limiter unexpectedly cancelled");
            permit
                .deliver(sender.as_ref(), &delivery(url))
                .await
                .expect("fake delivery");
        })
    }

    #[tokio::test]
    async fn global_concurrency_is_never_exceeded() {
        let limiter = Arc::new(DeliveryLimiter::new(DeliveryConcurrency::new(2, 2)));
        let sender = Arc::new(BlockingSender::new());
        let tasks: Vec<_> = (0..6)
            .map(|i| {
                spawn_delivery(
                    limiter.clone(),
                    sender.clone(),
                    format!("https://e{i}.test"),
                )
            })
            .collect();

        wait_for_active(&sender, 2).await;
        assert_eq!(sender.maximum.load(Ordering::Acquire), 2);
        sender.gate.open();
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(sender.maximum.load(Ordering::Acquire), 2);
        assert_eq!(limiter.available_global_permits(), 2);
    }

    #[tokio::test]
    async fn endpoint_concurrency_is_never_exceeded() {
        let limiter = Arc::new(DeliveryLimiter::new(DeliveryConcurrency::new(4, 2)));
        let sender = Arc::new(BlockingSender::new());
        let tasks: Vec<_> = (0..6)
            .map(|_| {
                spawn_delivery(
                    limiter.clone(),
                    sender.clone(),
                    "https://slow.test/hook".to_owned(),
                )
            })
            .collect();

        wait_for_active(&sender, 2).await;
        assert_eq!(sender.maximum.load(Ordering::Acquire), 2);
        sender.gate.open();
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(sender.maximum.load(Ordering::Acquire), 2);
    }

    struct IsolationSender {
        slow_gate: Gate,
        slow_started: Notify,
        fast_finished: Notify,
    }

    #[async_trait]
    impl WebhookSender for IsolationSender {
        async fn deliver(&self, delivery: &Delivery) -> Result<DeliveryResponse, String> {
            if delivery.url.contains("slow") {
                self.slow_started.notify_one();
                self.slow_gate.wait().await;
            } else {
                self.fast_finished.notify_one();
            }
            Ok(DeliveryResponse::new(200))
        }
    }

    #[tokio::test]
    async fn queued_slow_endpoint_does_not_block_fast_endpoint() {
        let limiter = Arc::new(DeliveryLimiter::new(DeliveryConcurrency::new(2, 1)));
        let sender = Arc::new(IsolationSender {
            slow_gate: Gate::default(),
            slow_started: Notify::new(),
            fast_finished: Notify::new(),
        });
        let mut tasks = Vec::new();
        for _ in 0..3 {
            let limiter = limiter.clone();
            let sender = sender.clone();
            tasks.push(tokio::spawn(async move {
                let cancel = CancellationToken::new();
                let url = "https://slow.test/hook";
                let permit = limiter
                    .acquire(url, DeliveryStage::Retry, &cancel)
                    .await
                    .unwrap();
                permit
                    .deliver(sender.as_ref(), &delivery(url.to_owned()))
                    .await
                    .unwrap();
            }));
        }
        sender.slow_started.notified().await;

        let fast_limiter = limiter.clone();
        let fast_sender = sender.clone();
        tasks.push(tokio::spawn(async move {
            let cancel = CancellationToken::new();
            let url = "https://fast.test/hook";
            let permit = fast_limiter
                .acquire(url, DeliveryStage::Retry, &cancel)
                .await
                .unwrap();
            permit
                .deliver(fast_sender.as_ref(), &delivery(url.to_owned()))
                .await
                .unwrap();
        }));
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            sender.fast_finished.notified(),
        )
        .await
        .expect("fast endpoint was head-of-line blocked by slow endpoint");

        sender.slow_gate.open();
        for task in tasks {
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn cancellation_releases_waiting_and_owned_permits() {
        let limiter = Arc::new(DeliveryLimiter::new(DeliveryConcurrency::new(1, 1)));
        let owner_cancel = CancellationToken::new();
        let owner = limiter
            .acquire("https://one.test/hook", DeliveryStage::Retry, &owner_cancel)
            .await
            .unwrap();

        let waiter_limiter = limiter.clone();
        let waiter_cancel = CancellationToken::new();
        let waiter_signal = waiter_cancel.clone();
        let waiter = tokio::spawn(async move {
            waiter_limiter
                .acquire(
                    "https://one.test/hook",
                    DeliveryStage::Retry,
                    &waiter_cancel,
                )
                .await
                .is_none()
        });
        tokio::task::yield_now().await;
        waiter_signal.cancel();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
                .await
                .expect("cancelled waiter did not return")
                .unwrap()
        );

        drop(owner);
        limiter.prune_idle_endpoints();
        assert_eq!(limiter.available_global_permits(), 1);
        assert_eq!(limiter.endpoint_entry_count(), 0);
    }

    #[test]
    fn concurrency_values_are_positive_bounded_and_endpoint_scoped() {
        assert_eq!(
            DeliveryConcurrency::new(0, 0),
            DeliveryConcurrency {
                global: 1,
                endpoint: 1
            }
        );
        assert_eq!(
            DeliveryConcurrency::new(usize::MAX, usize::MAX),
            DeliveryConcurrency {
                global: MAX_GLOBAL_CONCURRENCY,
                endpoint: MAX_ENDPOINT_CONCURRENCY
            }
        );
        assert_eq!(
            DeliveryConcurrency::new(3, 8),
            DeliveryConcurrency {
                global: 3,
                endpoint: 3
            }
        );
    }
}
