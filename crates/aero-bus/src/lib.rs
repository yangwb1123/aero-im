//! Event bus abstraction over NATS `JetStream`.
//!
//! Two layers:
//! 1. `EventBus` trait — generic publish/subscribe that hides NATS specifics.
//! 2. `JetStreamBus` — concrete implementation that declares streams and consumers
//!    according to the design spec (see `docs/specs/2026-05-22-aero-im-design.md` §4.2).
//!
//! Streams declared:
//! - `IM_MESSAGES`  subjects = `im.room.*`            retention=limits, 7d, file
//! - `IM_EVENTS`    subjects = `im.events.>`          retention=limits, 30d, file
//! - `AI_QUEUE`     subjects = `ai.queue.*`           retention=work-queue, 1d
//!
//! Implementation is filled in by the storage/bus agent.

pub mod jetstream;
pub mod seq;
pub mod traits;

pub use jetstream::{JetStreamBus, JetStreamConfig};
pub use seq::{
    extract_seq, extract_traceparent, stamp_seq, stamp_traceparent, stamped_event_bytes,
};
pub use traits::{EventBus, Subscription};
