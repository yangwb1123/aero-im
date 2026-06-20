//! WebSocket handler — connection lifecycle, message dispatch, bus listeners.
//!
//! Split from monolithic `ws.rs` (1379 lines) as part of REFACTOR_PLAN.md Step 4.
//! The main implementations live in [`ws_impl`], with sub-modules extracted for
//! specific domains.

pub mod frame;
pub mod ws_impl;

pub use ws_impl::*;

#[cfg(test)]
pub mod tests;
