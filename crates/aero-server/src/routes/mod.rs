//! HTTP route definitions — all API endpoints mounted in [`build`].
//!
//! Split from monolithic `routes.rs` (2633 lines) as part of REFACTOR_PLAN.md Step 7.

pub mod routes;
pub mod agents;
pub mod ai;
pub mod live;
pub mod rtc;
pub mod health;
pub mod mls;
pub mod helpers;

pub use routes::*;
