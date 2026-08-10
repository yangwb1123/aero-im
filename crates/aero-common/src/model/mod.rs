//! Core domain types — mirrors the SQL schema in `migrations/`.
//!
//! Split from monolithic `model_impl.rs` (1552 lines) as part of `REFACTOR_PLAN.md`.
//! Sub-modules each own a domain concern; `pub use *` preserves the flat public API.

pub mod blob;
pub mod block;
pub mod event;
pub mod media;
pub mod message;
pub mod notification;
pub mod participant;
pub mod presence;
pub mod room;

#[cfg(test)]
mod tests;

pub use blob::*;
pub use block::*;
pub use event::*;
pub use media::*;
pub use message::*;
pub use notification::*;
pub use participant::*;
pub use presence::*;
pub use room::*;
