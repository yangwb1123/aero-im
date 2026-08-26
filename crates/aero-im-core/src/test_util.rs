//! Shared in-memory test fixtures.
//!
//! Compiled only for `cfg(test)`; not part of the public API.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};

use aero_bus::traits::{BusError, BusResult, EventBus, Subscription};
use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio::sync::oneshot;

/// One-shot rendezvous used by the outbox fencing tests. The sender is notified
/// after the relay has entered `publish`; the receiver is held until the test has
/// modified the claimed row and explicitly releases the publish call.
struct PublishGate {
    ready: Option<oneshot::Sender<()>>,
    release: oneshot::Receiver<()>,
}

/// In-memory [`EventBus`] that records every publish call for assertions.
///
/// `subscribe` is intentionally unimplemented — no test in this crate consumes from
/// the bus; we only verify what was *published*.
#[derive(Default)]
pub(crate) struct MockBus {
    pub published: Mutex<Vec<(String, Vec<u8>)>>,
    /// Number of future publish calls that should fail. `usize::MAX` means all
    /// calls fail until the test changes the value again.
    pub(crate) fail_publishes: AtomicUsize,
    publish_gate: Mutex<Option<PublishGate>>,
}

impl MockBus {
    /// Arm a single blocked failure and return `(entered, release)` rendezvous
    /// halves. The `entered` receiver resolves only after a relay has claimed a
    /// row and reached the bus publish call, making lease-fencing tests
    /// deterministic instead of relying on sleeps.
    pub(crate) fn arm_blocking_failure(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (ready_tx, ready_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        *self
            .publish_gate
            .lock()
            .expect("mock publish gate lock is not poisoned") = Some(PublishGate {
            ready: Some(ready_tx),
            release: release_rx,
        });
        self.fail_publishes.store(usize::MAX, Ordering::SeqCst);
        (ready_rx, release_tx)
    }

    fn take_failure(&self) -> bool {
        let mut remaining = self.fail_publishes.load(Ordering::SeqCst);
        loop {
            if remaining == 0 {
                return false;
            }
            if remaining == usize::MAX {
                return true;
            }
            match self.fail_publishes.compare_exchange(
                remaining,
                remaining - 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(current) => remaining = current,
            }
        }
    }
}

#[async_trait]
impl EventBus for MockBus {
    async fn publish(&self, subject: &str, payload: bytes::Bytes) -> BusResult<()> {
        if self.take_failure() {
            let gate = self
                .publish_gate
                .lock()
                .map_err(|e| BusError::Nats(format!("mock gate lock poisoned: {e}")))?
                .take();
            if let Some(mut gate) = gate {
                if let Some(ready) = gate.ready.take() {
                    let _ = ready.send(());
                }
                let _ = gate.release.await;
            }
            return Err(BusError::Nats("injected failure".into()));
        }
        self.published
            .lock()
            .map_err(|e| BusError::Nats(format!("mock lock poisoned: {e}")))?
            .push((subject.to_owned(), payload.to_vec()));
        Ok(())
    }

    async fn subscribe(
        &self,
        _subject: &str,
        _durable: Option<&str>,
    ) -> BusResult<BoxStream<'static, Box<dyn Subscription + Send>>> {
        unimplemented!("MockBus.subscribe not needed for ImService tests")
    }
}
