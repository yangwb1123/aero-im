//! Core domain model — mirrors the SQL schema in `migrations/`.
//!
//! These types are wire-format AND storage-format. The `Block` enum is intentionally
//! the same shape as Slack Block Kit / Discord Embeds / LLM tool-call payloads,
//! so messages can flow through the system without lossy transformations.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::ids::ParticipantId;

// ---------- User custom status + presence ----------

/// A user's coarse presence preference. This is the DURABLE, user-chosen
/// preference shown on a profile — distinct from the ephemeral Redis online
/// tracking in `aero_storage::PresenceStore` (which records whether a client is
/// currently connected to a room). Serialized as a lowercase token on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Presence {
    /// Available / online by choice.
    Active,
    /// Stepped away but still reachable.
    Away,
    /// Appears offline by choice.
    Offline,
}

impl Presence {
    /// Lowercase DB/wire token.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Away => "away",
            Self::Offline => "offline",
        }
    }

    /// Parse a DB/wire token, defaulting unknown/empty values to [`Self::Active`].
    /// Mirrors `NotificationKind::from_str_lenient` so a malformed stored token
    /// never fails a read.
    #[must_use]
    pub fn from_str_lenient(s: &str) -> Self {
        match s {
            "away" => Self::Away,
            "offline" => Self::Offline,
            _ => Self::Active,
        }
    }
}

/// A user-set custom status (emoji + free text, like Slack's "🏝️ On vacation")
/// plus a coarse [`Presence`] preference. One per participant. The custom status
/// (`emoji`/`text`) may auto-expire at `expires_at`; once it has passed, readers
/// treat the custom status as cleared (emoji/text become `None`) while keeping
/// the `presence` preference. Backs `migrations/0022_user_status.sql`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserStatus {
    pub participant_id: ParticipantId,
    /// Emoji shorthand, e.g. `:palm_tree:`. `None` when no custom status is set
    /// (or it has expired).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emoji: Option<String>,
    /// Free-form status text, e.g. "On vacation". `None` when unset/expired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub presence: Presence,
    /// Optional auto-expiry for the custom status. `None` means it never expires.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl UserStatus {
    /// Whether the custom status (`emoji` + `text`) has auto-expired as of `now`.
    ///
    /// `expires_at == None` (no expiry) is never expired. Otherwise it is expired
    /// once `now` has reached or passed `expires_at` (the boundary is inclusive,
    /// so an instant *equal* to `expires_at` counts as expired). This is the
    /// canonical, DB-agnostic counterpart to the storage layer's row-level check,
    /// so any in-memory reader (event payload, cache, test) applies the same rule.
    #[must_use]
    pub fn is_custom_status_expired(&self, now: OffsetDateTime) -> bool {
        matches!(self.expires_at, Some(at) if now >= at)
    }

    /// Return a normalized copy as a reader should see it at `now`: if the custom
    /// status has expired, `emoji`, `text`, and `expires_at` are dropped while the
    /// coarse [`Presence`] preference and `updated_at` are kept (a user who set
    /// "away until 5pm" stays away after 5pm; only the decoration drops). When not
    /// expired, the status is returned unchanged.
    ///
    /// This owns the contract promised in this type's docs so non-DB code paths do
    /// not have to re-implement the expiry-clearing rule.
    #[must_use]
    pub fn with_expiry_applied(mut self, now: OffsetDateTime) -> Self {
        if self.is_custom_status_expired(now) {
            self.emoji = None;
            self.text = None;
            self.expires_at = None;
        }
        self
    }

    /// The emoji a reader should see at `now` — `None` once the custom status has
    /// expired, without allocating a normalized copy.
    #[must_use]
    pub fn effective_emoji(&self, now: OffsetDateTime) -> Option<&str> {
        if self.is_custom_status_expired(now) {
            None
        } else {
            self.emoji.as_deref()
        }
    }

    /// The status text a reader should see at `now` — `None` once the custom
    /// status has expired, without allocating a normalized copy.
    #[must_use]
    pub fn effective_text(&self, now: OffsetDateTime) -> Option<&str> {
        if self.is_custom_status_expired(now) {
            None
        } else {
            self.text.as_deref()
        }
    }
}
