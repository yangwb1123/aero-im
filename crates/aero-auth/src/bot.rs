//! Bot token verification hook for [`AuthService`].
//!
//! Bots registered through the open platform (方向三) authenticate with their own
//! opaque bearer token (`bot_<uuid>`, see
//! [`BotRepo::rotate_token`](aero_storage::BotRepo::rotate_token)). The
//! [`AuthUser`](crate::extractor::AuthUser) extractor accepts a bot token wherever
//! it accepts an access JWT or a PAT, resolving it to the bot's *participant*
//! identity (a bot is a participant with `kind = 'bot'`).
//!
//! Exactly like [`PatVerifier`](crate::pat::PatVerifier), the extractor talks to an
//! injected trait object rather than a concrete repository type, so `aero-auth`
//! keeps no compile-time edge to the bot table beyond the blanket impl below. The
//! server wires a real implementation (over [`aero_storage::BotRepo`]) into the
//! service at startup via
//! [`AuthService::with_bot_verifier`](crate::service::AuthService::with_bot_verifier).
//!
//! `aero-auth` already depends on `aero-storage`, so the blanket implementation for
//! [`BotRepo`] lives right here — no dependency cycle, and the server crate needs no
//! glue beyond constructing the repo and handing it to the service.
//!
//! [`BotRepo`]: aero_storage::BotRepo

use std::sync::Arc;

use aero_common::ParticipantId;
use async_trait::async_trait;

/// Resolves a *plaintext* bot token to its bot participant, IFF the token is
/// active and the bot is not deleted.
///
/// Unlike [`PatVerifier`](crate::pat::PatVerifier), the argument here is the raw
/// token rather than a pre-computed hash: bot tokens are hashed with a different
/// helper ([`aero_storage::revoked_token::hash_token`]) than PATs, so the hashing
/// is kept on the storage side ([`aero_storage::BotRepo::verify_token`]) where that
/// helper lives. The plaintext therefore does not leave the process boundary that
/// already holds the DB pool. Returns `None` for an unknown / un-issued / deleted
/// bot — indistinguishable on purpose, so a caller cannot probe which bots exist.
#[async_trait]
pub trait BotTokenVerifier: Send + Sync {
    async fn verify(&self, token: &str) -> Option<ParticipantId>;
}

/// The real verifier: an [`aero_storage::BotRepo`]. Defined here (rather than in
/// the storage crate) because the trait lives here and `aero-auth` already depends
/// on `aero-storage` — implementing it the other way would need a dependency edge
/// that does not exist.
#[async_trait]
impl BotTokenVerifier for aero_storage::BotRepo {
    async fn verify(&self, token: &str) -> Option<ParticipantId> {
        // A DB error during bot-token resolution is treated as "no match" (the
        // request falls through to a 401) rather than surfaced — the extractor has
        // no error channel, and a transient DB blip must not authenticate anyone.
        match aero_storage::BotRepo::verify_token(self, token).await {
            Ok(owner) => owner,
            Err(err) => {
                tracing::warn!(error = %err, "bot token verification query failed");
                None
            }
        }
    }
}

/// Convenience: a boxed verifier over any [`BotTokenVerifier`], for stashing in
/// [`AuthService`](crate::service::AuthService). `Arc` so the cheap-to-clone
/// service can share one instance across all its clones.
pub type SharedBotVerifier = Arc<dyn BotTokenVerifier>;
