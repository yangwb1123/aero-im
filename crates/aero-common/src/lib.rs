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
pub mod markdown;
pub mod metrics;
pub mod mls;
pub mod model;
pub mod telemetry;
pub mod time;
pub mod workspace;

pub use error::{Error, Result};
pub use ids::{
    ActivityId, AnnouncementId, ApprovalId, AuditId, BanAppealId, BarrierId, BlobId,
    BlockInteractionId, BookmarkCollectionId, CanvasId, ChannelBookmarkId, ChannelSectionId,
    ClipCollectionId, ClipId, CreatorTierId, DigestSubscriptionId, EmojiId, GoalId,
    HypeTrainSessionId, InvitationId, JoinRequestId, KeywordAlertId, LegalHoldId, MessageEditId,
    MessageId, MessageReportId, MessageTemplateId, NotificationId, ParticipantId, PatId, PollId,
    PredictionId, PredictionStakeId, RaidId, RecurringMessageId, RedemptionId, RewardId, RoomId,
    SavedSearchId, ScheduledMessageId, ScheduledStreamId, ScimTokenId, SessionId, StreamCategoryId,
    StreamModeratorId, SubscriptionId, TaskId, UserGroupId, VodChapterId, VodId, WebhookDeliveryId,
    WebhookId, WorkspaceId,
};
pub use live::{
    gift_by_id, gift_catalog, Gift, GiftLeaderRow, StreamChatLine, StreamEvent, StreamGiftLine,
};
pub use model::{
    check_audience, check_issuer, check_scope, check_subject, is_valid_identity_component,
    message_has_action, recall_window_expired, AuditActor, AuditClaimPayload, AuditClass,
    AuditTarget, Blob, Block, CallEvent, CallId, CallKind, CallMode, CallSession,
    ClientCredentialsClaims, ClientCredentialsGateClaims, ClientCredentialsTokenConfig, FileKind,
    MembershipOp, Message, MessageEnvelope, Notification, NotificationKind, NotifyTarget,
    OutboxStatus, Participant, ParticipantKind, PinOp, PinnedMessage, Poll, PollOp, PollTally,
    Presence, Reaction, ReactionOp, ReactionSummary, ReadReceipt, Room, RoomEvent, RoomKind,
    RoomUnread, SelectOption, SfuMediaKind, SfuPublishedTrack, SfuPublisherDescription,
    SfuSubscription, Span, SpanStyle, Stream, StreamProtocol, StreamStatus, ThreadSummary,
    UserStatus, Vod, AUDIT_ACTOR_TYPE_PARTICIPANT, AUDIT_ACTOR_TYPE_SYSTEM, AUDIT_AGGREGATE_TYPE,
    AUDIT_DATA_CLASSIFICATION, AUDIT_EVENT_TYPE, AUDIT_OUTCOME_SUCCESS, AUDIT_RETENTION_CLASS,
    AUDIT_SCHEMA_ID, AUDIT_SCHEMA_VERSION, AUDIT_TARGET_TYPE_RESOURCE, CLAIM_AUD, CLAIM_CLIENT_ID,
    CLAIM_IAT, CLAIM_ISS, CLAIM_JTI, CLAIM_SCOPE, CLAIM_SCOPES, CLAIM_SUB, GOVERNANCE_CLASS_ADMIN,
    GOVERNANCE_CLASS_MESSAGE, GOVERNANCE_CLASS_ROOM, LOCAL_ACTION_MODERATED,
    MODERATION_OUTBOUND_ACTION, RECALLED_MESSAGE_PLACEHOLDER, TOKEN_TYPE_AT_JWT,
    TOKEN_TYPE_AT_JWT_APPLICATION, AGGREGATED_MESSAGE_ACTION, AUDIT_SOURCE_SYSTEM,
    L1_WINDOW_SECONDS, LOCAL_ACTION_MESSAGE_CREATE, LOCAL_ACTION_MESSAGE_EDIT,
    LOCAL_ACTION_ROOM_ARCHIVED, LOCAL_ACTION_ROOM_CREATE, LOCAL_ACTION_MESSAGE_RECALLED,
};
pub use workspace::{Workspace, WorkspaceMember, WorkspaceRole};
