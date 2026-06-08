//! Shared contracts for the Aero workspace.
//!
//! This crate has **no business logic**. It defines:
//! - Strongly-typed IDs (`ParticipantId`, `RoomId`, `MessageId`, `BlobId`)
//! - The `Block`/`Message`/`Room`/`Participant` data model (mirrors DB schema)
//! - Cross-cutting `Error` and `Result` aliases
//! - Configuration loading (`AppConfig`)
//! - Tracing/OTel initialization
//! - A lightweight metrics registry + Prometheus exposition (`metrics`)
//!
//! Other crates depend on this one and re-export domain-specific types as needed.

pub mod config;
pub mod error;
pub mod ids;
pub mod live;
pub mod metrics;
pub mod mls;
pub mod model;
pub mod telemetry;
pub mod time;
pub mod workspace;

pub use error::{Error, Result};
pub use ids::{
    AnnouncementId, ApprovalId, AuditId, BlobId, CanvasId, ChannelBookmarkId, ChannelSectionId,
    CreatorTierId, EmojiId, InvitationId, JoinRequestId, KeywordAlertId, LegalHoldId, MessageEditId,
    MessageId, MessageTemplateId, NotificationId, ParticipantId, PatId, PollId, RecurringMessageId,
    RoomId, SavedSearchId, ScheduledMessageId, ScheduledStreamId, ScimTokenId, StreamCategoryId,
    SubscriptionId, TaskId, UserGroupId, VodId, WebhookId, WorkspaceId,
};
pub use live::{
    gift_by_id, gift_catalog, Gift, GiftLeaderRow, StreamChatLine, StreamEvent, StreamGiftLine,
};
pub use model::{
    Block, Blob, CallEvent, CallId, CallKind, CallMode, CallSession, FileKind, MembershipOp,
    Message, MessageEnvelope, Notification, NotificationKind, Participant, ParticipantKind, PinOp,
    PinnedMessage, Poll, PollOp, PollTally, Presence, ReactionOp, ReactionSummary, Reaction,
    ReadReceipt, Room, RoomEvent, RoomKind, RoomUnread, Span, SpanStyle, Stream, StreamProtocol,
    StreamStatus, ThreadSummary, UserStatus, Vod,
};
pub use workspace::{Workspace, WorkspaceMember, WorkspaceRole};
