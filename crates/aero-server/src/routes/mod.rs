//! HTTP route definitions — all API endpoints mounted in [`build`].
//!
//! Split from monolithic `routes.rs` (2633 lines) as part of `REFACTOR_PLAN.md` Step 7.

pub mod agents;
pub mod ai;
pub mod health;
pub mod helpers;
pub mod live;
pub mod mls;
pub mod reads;
// Module-inception name (routes::routes) is intentional — AGENTS.md anchors the
// single assembly point as `routes/routes.rs::build()`.
#[allow(clippy::module_inception)]
pub mod routes;
pub mod rtc;
pub mod search;

pub use routes::*;
