//! ImService — the IM business facade.
//!
//! This module is split into sub-modules by business domain:
//! - [`messages`] — send, edit, delete, moderate-delete
//! - [`orig`] — remaining methods (room, reactions, reads, notifications, etc.)
//!
//! The original monolithic `service.rs` was split to keep each file under 800
//! lines (REFACTOR_PLAN.md Step 1).

/// Message operations: send, edit, delete, moderate-delete.
pub(crate) mod messages;

/// Reaction operations: toggle, list aggregates.
pub(crate) mod reactions;

/// Read-receipt + typing-indicator operations.
pub(crate) mod reads;

/// Call operations — start, relay, end.
pub(crate) mod calls;

/// Pin operations — pin, unpin, list, broadcast room events.
pub(crate) mod pins;

/// Event publishing to NATS — seq-stamped fan-out for room events.
pub(crate) mod events;

/// Room lifecycle operations — create, access-control, add-member.
pub(crate) mod room;

/// Channel operations — join, leave, archive, metadata, post-policy.
pub(crate) mod channels;

/// Original monolithic service module — remaining methods not yet extracted.
/// TODO(REFACTOR): further split into `room.rs`, `reactions.rs`, `reads.rs`,
/// `notifications.rs` as each sub-module crosses 600 lines.
pub(crate) mod orig;

// Re-export everything so the crate-level `pub use service::{ImService, BusSink, ...}`
// continues to work unchanged.  This delegates to `orig` for all methods that
// haven't been extracted yet, and to the extracted sub-modules for the rest.
pub use events::BusSink;
pub use orig::{
    can_access_room, can_create_channel, can_join_public_channel, post_allowed, ImService,
};
