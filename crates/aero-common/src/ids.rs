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
