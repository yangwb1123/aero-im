//! Shared in-memory test fixtures.
//!
//! Compiled only for `cfg(test)`; not part of the public API.

use std::sync::Mutex;

use aero_bus::traits::{BusError, BusResult, EventBus, Subscription};
use async_trait::async_trait;
use futures::stream::BoxStream;

/// In-memory [`EventBus`] that records every publish call for assertions.
///
/// `subscribe` is intentionally unimplemented — no test in this crate consumes from
/// the bus; we only verify what was *published*.
#[derive(Default)]
pub(crate) struct MockBus {
    pub published: Mutex<Vec<(String, Vec<u8>)>>,
}

#[async_trait]
impl EventBus for MockBus {
    async fn publish(&self, subject: &str, payload: bytes::Bytes) -> BusResult<()> {
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
