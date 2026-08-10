//! aero-audit-connector (B5-2) — leased-relay audit connector.
//!
//! The relay state machine is cloned from the proven `AiUsageRepo`
//! claim/settle/requeue contract (`crates/aero-storage/src/ai_usage.rs`):
//! per-row leased claims with a rotated `gen_random_uuid()` fencing token,
//! fenced one-transaction settlement, and exponential backoff capped at 300s.
//! Two surfaces are new versus that pattern:
//!
//! - a **dead terminal** (cloned from `AiJobRepo::fail`'s attempts threshold):
//!   permanent classes (422 / 409 / receipt mismatch / local payload guard)
//!   requeue once then die; HTTP 403 dies immediately (T-11 fail-closed);
//!   transients never die;
//! - **claim validation** (iss / aud / scope / sub) of every token *before*
//!   any delivery POST — v1 Snaplink only shape-checks the bearer.
//!
//! The connector deliberately does not import `AiUsageRepo` nor touch the
//! `snaplink_delivery_outbox` (v1) table; the PG impl binds B5-1's 0239
//! governance outbox through the [`outbox::OutboxRepo`] trait seam, with an
//! in-memory fake (A1) and stub HTTP endpoints (A2) as test doubles.

pub mod client;
pub mod config;
pub mod fake;
pub mod outbox;
pub mod pg;
pub mod relay;
pub mod stub;
