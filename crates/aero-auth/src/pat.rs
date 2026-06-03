//! Personal Access Token (PAT) verification hook for [`AuthService`].
//!
//! The [`AuthUser`](crate::extractor::AuthUser) extractor accepts a PAT wherever
//! it accepts an access JWT. To keep `aero-auth` from depending on a *concrete*
//! repository type for that lookup, the extractor talks to an injected
//! [`PatVerifier`] trait object instead. The server wires a real implementation
//! (over [`aero_storage::PatRepo`]) into the service at startup via
//! [`AuthService::with_pat_verifier`](crate::service::AuthService::with_pat_verifier).
//!
//! `aero-auth` already depends on `aero-storage`, so the blanket implementation
//! for [`PatRepo`] lives right here — no dependency cycle, and the server crate
//! needs no glue beyond constructing the repo and handing it to the service.
//!
//! [`PatRepo`]: aero_storage::PatRepo

use std::sync::Arc;

use aero_common::ParticipantId;
use async_trait::async_trait;

/// Resolves a *hashed* PAT to its owning participant, IFF the token is active.
///
/// The argument is the SHA-256 hash of the presented token (the extractor hashes
/// the plaintext via [`aero_storage::pat::hash_pat`] before calling this), so a
/// plaintext secret never crosses this boundary. Returns `None` for an unknown,
/// revoked, or expired token — indistinguishable on purpose, so a caller cannot
/// probe which tokens exist.
#[async_trait]
pub trait PatVerifier: Send + Sync {
    async fn verify(&self, token_hash: &str) -> Option<ParticipantId>;
}

/// The real verifier: an [`aero_storage::PatRepo`]. Defined here (rather than in
/// the storage crate) because the trait lives here and `aero-auth` already
/// depends on `aero-storage` — implementing it the other way would need a
/// dependency edge that does not exist.
#[async_trait]
impl PatVerifier for aero_storage::PatRepo {
    async fn verify(&self, token_hash: &str) -> Option<ParticipantId> {
        // A DB error during PAT resolution is treated as "no match" (the request
        // falls through to a 401) rather than surfaced — the extractor has no
        // error channel, and a transient DB blip must not authenticate anyone.
        match aero_storage::PatRepo::verify(self, token_hash).await {
            Ok(owner) => owner,
            Err(err) => {
                tracing::warn!(error = %err, "PAT verification query failed");
                None
            }
        }
    }
}

/// Convenience: a boxed verifier over any [`PatVerifier`], for stashing in
/// [`AuthService`](crate::service::AuthService). `Arc` so the cheap-to-clone
/// service can share one instance across all its clones.
pub type SharedPatVerifier = Arc<dyn PatVerifier>;
