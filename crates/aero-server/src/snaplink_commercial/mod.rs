//! Snaplink commercial control-plane integration.
//!
//! The remote systems are asynchronously projected/delivered. Request-path
//! authorization and quota decisions use only the versioned `PostgreSQL`
//! projection, so a short central outage cannot block an already-entitled
//! tenant while an unprojected tenant remains fail-closed.

mod config;
mod http;
mod runtime;

pub use runtime::SnaplinkCommercialRuntime;
