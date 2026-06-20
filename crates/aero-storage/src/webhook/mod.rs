//! Webhook persistence + pure signing/delivery logic (integration plane).
//!
//! Split from monolithic `webhook.rs` (1321 lines) as part of REFACTOR_PLAN.md.
//! Sub-modules each own a domain concern; `pub use *` preserves the flat public API.

pub mod breaker;
pub mod crypto;
pub mod delivery;
pub mod repo;
pub mod types;

pub use breaker::*;
pub use crypto::*;
pub use delivery::*;
pub use repo::*;
pub use types::*;
