//! IM business logic for Aero.
//!
//! This crate sits between the HTTP/WS transport layer (`aero-server`) and the
//! storage / bus crates. It enforces business invariants — membership, validation,
//! event publishing — and exposes a single [`ImService`] facade.
//!
//! See `docs/specs/2026-05-22-aero-im-design.md` §4.2 for the protocol contract.
//!
//! ## Layout
//! - [`service`] — the [`ImService`] facade (rooms, messages, history).
//! - [`validation`] — [`Block`](aero_common::Block) limit checks.
//! - [`events`] — high-level [`ImEvent`] published on `im.events.*`.

mod events;
mod moderator;
mod service;
mod validation;

pub use events::ImEvent;
pub use moderator::{AllowAllModerator, KeywordModerator, Moderator, ModerationVerdict};
pub use service::{
    can_access_room, can_create_channel, can_join_public_channel, BusSink, ImService,
};
pub use validation::{
    validate_blocks, ValidationError, MAX_BLOCKS, MAX_CODE_BYTES, MAX_TEXT_BYTES,
};

#[cfg(test)]
mod test_util;

#[cfg(test)]
mod db_tests;
