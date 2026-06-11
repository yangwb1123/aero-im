//! Newtype wrappers around [`Ulid`] for type-safe IDs.
//!
//! ULIDs are 128-bit, sortable by time, URL-safe, and database-friendly.
//! Each ID type is distinct so swapping a `RoomId` for a `MessageId` is a compile error.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use ulid::Ulid;

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Ulid);

        impl $name {
            #[must_use]
            pub fn new() -> Self {
                Self(Ulid::new())
            }

            #[must_use]
            pub fn from_ulid(u: Ulid) -> Self {
                Self(u)
            }

            #[must_use]
            pub fn as_ulid(&self) -> Ulid {
                self.0
            }

            #[must_use]
            pub fn to_uuid(&self) -> uuid::Uuid {
                uuid::Uuid::from_u128(self.0.0)
            }

            #[must_use]
            pub fn from_uuid(u: uuid::Uuid) -> Self {
                Self(Ulid(u.as_u128()))
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl FromStr for $name {
            type Err = ulid::DecodeError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(Ulid::from_str(s)?))
            }
        }

        impl From<Ulid> for $name {
            fn from(u: Ulid) -> Self {
                Self(u)
            }
        }

        impl From<$name> for Ulid {
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl From<$name> for uuid::Uuid {
            fn from(id: $name) -> Self {
                id.to_uuid()
            }
        }
    };
}

define_id!(
    /// Identifies a participant — Human, Agent, or Bot.
    ParticipantId
);
define_id!(
    /// Identifies a chat room (direct/group/channel).
    RoomId
);
define_id!(
    /// Identifies a single message.
    MessageId
);
define_id!(
    /// Identifies a stored blob (file, voice, image).
    BlobId
);
define_id!(
    /// Identifies a workspace (tenant / org). Rooms and members belong to one.
    WorkspaceId
);
define_id!(
    /// Identifies a single audit-trail event (workspace administration log).
    AuditId
);
define_id!(
    /// Identifies a single notification (mention / thread-reply inbox entry).
    NotificationId
);
define_id!(
    /// Identifies a single webhook (incoming inbound-message hook or outgoing
    /// event-delivery hook).
    WebhookId
);
define_id!(
    /// Identifies a SCIM 2.0 provisioning bearer token (per-workspace, RFC 7644).
    ScimTokenId
);
define_id!(
    /// Identifies a scheduled message ("Send later") / reminder, pending delivery.
    ScheduledMessageId
);
define_id!(
    /// Identifies a workspace invitation / shareable invite link.
    InvitationId
);
define_id!(
    /// Identifies a workspace custom emoji (`:shipit:`), backed by an image blob.
    EmojiId
);
define_id!(
    /// Identifies a Personal Access Token (PAT) — a long-lived, participant-owned
    /// credential for programmatic REST API access.
    PatId
);
define_id!(
    /// Identifies a poll (question + options) created in a room.
    PollId
);
define_id!(
    /// Identifies a stored stream VOD / recording (a finalized live stream that
    /// members can list and play back later).
    VodId
);
define_id!(
    /// Identifies a scheduled live-stream announcement — an upcoming stream a
    /// workspace member announces ahead of time (title + time + optional room),
    /// distinct from the actual stream the ingest path later creates.
    ScheduledStreamId
);
define_id!(
    /// Identifies a saved search — a per-user, workspace-scoped named search
    /// query the owner can list, re-run, or delete.
    SavedSearchId
);
define_id!(
    /// Identifies a per-user channel-sidebar section — a named, ordered folder a
    /// participant groups their channels under, private to that user and scoped to
    /// one workspace.
    ChannelSectionId
);
define_id!(
    /// Identifies a user group (`@-usergroup`) — a workspace-scoped, named set of
    /// participants that can be `@`-mentioned as one to notify every member.
    UserGroupId
);
define_id!(
    /// Identifies a single archived prior version of an edited message (one row per
    /// edit, capturing the blocks that were replaced).
    MessageEditId
);
define_id!(
    /// Identifies a per-user keyword / highlight alert — a workspace-scoped term that
    /// notifies its owner whenever a message contains it.
    KeywordAlertId
);
define_id!(
    /// Identifies a workspace announcement / banner posted by an admin (optionally
    /// time-limited via an expiry).
    AnnouncementId
);
define_id!(
    /// Identifies a recurring scheduled message (a message template posted on a
    /// repeating cadence — hourly/daily/weekly).
    RecurringMessageId
);
define_id!(
    /// Identifies a channel join request (a pending request to join a channel,
    /// awaiting owner/admin approval).
    JoinRequestId
);
define_id!(
    /// Identifies a saved message template / canned response (a reusable message
    /// body a user can post into a room on demand).
    MessageTemplateId
);
// ---- Wave 16 ----
define_id!(
    /// Identifies a channel canvas — a per-channel collaborative document (a
    /// titled rich document whose body is a JSON array of blocks). A channel may
    /// own several; any member with room access may edit one.
    CanvasId
);
define_id!(
    /// Identifies a channel bookmark — a pinned link / resource shown in a
    /// channel's header (title + url + optional emoji), distinct from a pinned
    /// message or a personal saved item.
    ChannelBookmarkId
);
define_id!(
    /// Identifies a live-stream discovery category (Twitch-style: Gaming, Music,
    /// Coding…) — a slug-addressable bucket a stream can be assigned to so viewers
    /// can browse live streams by category.
    StreamCategoryId
);
define_id!(
    /// Identifies a creator membership tier (name + monthly price + perks) that a
    /// creator defines for viewers to subscribe at — like a Twitch sub tier.
    CreatorTierId
);
define_id!(
    /// Identifies a creator subscription — one viewer's recurring membership to a
    /// creator at a chosen tier (at most one per creator/subscriber pair).
    SubscriptionId
);
// ---- Wave 17 ----
define_id!(
    /// Identifies a legal hold / retention exemption — an admin-placed
    /// preservation order over a single room (or, when its `room_id` is NULL, a
    /// whole workspace) that exempts the covered messages from the periodic
    /// retention sweep until released (eDiscovery preservation).
    LegalHoldId
);
define_id!(
    /// Identifies a task / to-do item tracked in a room — a durable, assignable,
    /// stateful work item (title + optional assignee/due-date/source-message),
    /// distinct from the AI action-item extraction that only summarizes.
    TaskId
);
define_id!(
    /// Identifies an approval request — a lightweight workspace-scoped approval
    /// (Lark 审批 / approvals-lite): a requester addresses a single approver who
    /// approves or denies it with an optional note (single-approver MVP).
    ApprovalId
);
// ---- Wave 18 ----
define_id!(
    /// Identifies a live-stream clip — a viewer-marked timestamped `[start, end]`
    /// range of a live stream / VOD shared for playback (Twitch/YouTube-style).
    /// Playback reuses the stream's existing HLS playlist with a client-side seek;
    /// no media is processed, so a clip is just metadata over a stream.
    ClipId
);
// ---- Wave 19 ----
define_id!(
    /// Identifies an information barrier / ethical wall — an admin-defined barred
    /// PAIR of user-groups (Microsoft Purview-style). Members across a barred pair
    /// may not DM each other or share a channel; the pair is symmetric.
    BarrierId
);
// ---- Wave 21 ----
define_id!(
    /// Identifies an active login session / device — one row per refresh-token-backed
    /// login, recorded so a user (or admin) can list their active sessions and revoke
    /// one or all-others ("sign out everywhere else").
    SessionId
);
define_id!(
    /// Identifies a single activity-feed entry — a durable, per-participant notice of
    /// a non-message event (e.g. a followed creator going live). Distinct from the
    /// message+room-scoped notification inbox: carries only a kind, optional
    /// actor/subject, and a human summary, with no room or message.
    ActivityId
);
// ---- Collaboration parity batch ----
define_id!(
    /// Identifies a bookmark collection / folder — a per-user named, ordered group
    /// of saved items (Slack "Saved items" folders). A saved message may belong to
    /// at most one collection (a nullable `collection_id` on `bookmarks`); deleting
    /// a collection nulls its items' assignment rather than removing the bookmark.
    BookmarkCollectionId
);

// ---- Interactive-live / creator parity (migrations 0079-0083) ----
define_id!(
    /// Identifies a hype-train session — an escalating momentum mechanic on a live
    /// stream where rapid successive gifts build "levels" within a sliding window
    /// (Twitch Hype Train). At most one `active` session per stream at a time.
    HypeTrainSessionId
);
define_id!(
    /// Identifies a recorded raid — a creator sending their viewers from a source
    /// stream to a target stream at the end of a broadcast (Twitch/Kick raid).
    RaidId
);
define_id!(
    /// Identifies a VOD chapter / marker — a timestamped table-of-contents entry on
    /// a recording (`start_secs` + title) the creator adds so viewers can jump to a
    /// section; playback reuses the VOD's HLS playlist with a client-side seek.
    VodChapterId
);
define_id!(
    /// Identifies a stream-moderator assignment — a participant granted MOD ROLE on
    /// a stream's danmaku chat (chat-ban/timeout authority), distinct from a
    /// `stream_bans` row which records a banned chatter.
    StreamModeratorId
);

// ---- AI digests + operability ----
define_id!(
    /// Identifies a scheduled / recurring AI digest subscription — a participant's
    /// standing request for a `daily` / `weekly` AI summary of a room or workspace,
    /// delivered by the digest dispatcher.
    DigestSubscriptionId
);
define_id!(
    /// Identifies a webhook delivery-log row — one outgoing-webhook delivery attempt
    /// with retry/backoff state (pending/delivered/failed/dead).
    WebhookDeliveryId
);
define_id!(
    /// Identifies a single recorded interaction with an interactive message block —
    /// one row per (message, participant) click/select on a `Button`/`Select`
    /// [`Block`](crate::Block), so bots/webhooks/apps can collect actionable replies.
    BlockInteractionId
);
define_id!(
    /// Identifies a user-filed message report — one row in the workspace moderation
    /// review queue, where a room member flags a message and a workspace admin then
    /// keeps (dismisses) or removes (soft-deletes) it. Distinct from the AI
    /// moderation pipeline, which auto-removes async with no human review.
    MessageReportId
);

// ---- Live / creator economy (migrations 0089-0091) ----
define_id!(
    /// Identifies a custom-reward definition a creator offers for channel points — a
    /// titled, point-priced reward viewers redeem (Twitch Channel Points custom
    /// reward). Distinct from a [`RedemptionId`], which is one viewer's claim of it.
    RewardId
);
define_id!(
    /// Identifies a single channel-points redemption — one viewer's claim of a
    /// [`RewardId`], queued for the creator/mods to fulfill or reject (Twitch reward
    /// redemption queue entry).
    RedemptionId
);
define_id!(
    /// Identifies a creator goal / bounty bar — a titled target on a metric
    /// (`gifts`/`viewers`/`points`) that fills toward `target` as progress accrues
    /// (Twitch/streamer goal bar). A single progress threshold-cross flips it to
    /// `reached` exactly once.
    GoalId
);
define_id!(
    /// Identifies a ban/timeout appeal — a banned viewer's request that a stream's
    /// creator/mods review and lift their ban. FKs to the `stream_bans` row being
    /// appealed; approval triggers the existing unban path.
    BanAppealId
);

// ---- Community predictions / channel betting (migration 0092) ----
define_id!(
    /// Identifies a community prediction — a creator-opened bet on a live stream
    /// (Twitch-style Channel Prediction): a question with 2+ outcomes that viewers
    /// STAKE channel points on, then the creator LOCKS and RESOLVES to a winning
    /// outcome, paying winners proportionally from the pool. Distinct from a
    /// [`PollId`], which is plain voting with no stakes / payouts.
    PredictionId
);
define_id!(
    /// Identifies one viewer's stake in a prediction — a `prediction_stakes` row
    /// (the wagered outcome + points, and the payout stamped on settlement). At most
    /// one stake per viewer per [`PredictionId`].
    PredictionStakeId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_roundtrip_string() {
        let id = ParticipantId::new();
        let s = id.to_string();
        let parsed: ParticipantId = s.parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn ids_roundtrip_uuid() {
        let id = MessageId::new();
        let u: uuid::Uuid = id.into();
        let back = MessageId::from_uuid(u);
        assert_eq!(id, back);
    }

    #[test]
    fn distinct_id_types_do_not_mix() {
        // This is a compile-fence: the following must NOT compile.
        // let r: RoomId = ParticipantId::new();
        // We assert that types are different by checking type_id.
        use std::any::TypeId;
        assert_ne!(TypeId::of::<RoomId>(), TypeId::of::<ParticipantId>());
    }
}
