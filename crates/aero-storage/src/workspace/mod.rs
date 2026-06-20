//! Workspace repository — tenant CRUD, membership, GDPR export/deletion, retention.
//!
//! Split from monolithic `workspace.rs` (1750 lines) as part of REFACTOR_PLAN.md Step 5.

use aero_common::{Message, Room, Workspace, WorkspaceMember};
use serde::Serialize;

use crate::audit::AuditEvent;

pub mod export;
pub mod members;
pub mod settings;
pub mod sweep;
pub mod workspace_impl;

pub use workspace_impl::*;

/// Largest number of messages exported per room.
pub const EXPORT_MESSAGES_PER_ROOM: i64 = 10_000;

/// A room within a workspace export, with its messages.
#[derive(Debug, Clone, Serialize)]
pub struct RoomExport {
    pub room: Room,
    pub messages: Vec<Message>,
    /// `true` when the room held more than [`EXPORT_MESSAGES_PER_ROOM`] messages.
    pub message_cap_hit: bool,
}

/// A complete snapshot of one tenant's data.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceExport {
    pub workspace: Workspace,
    pub members: Vec<WorkspaceMember>,
    pub rooms: Vec<RoomExport>,
    pub audit_events: Vec<AuditEvent>,
    #[serde(with = "time::serde::rfc3339")]
    pub exported_at: time::OffsetDateTime,
}
