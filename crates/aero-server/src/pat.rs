//! Personal Access Token (PAT) management HTTP API.
//!
//! Lets an authenticated participant mint, list, and revoke long-lived API
//! tokens (`aero_pat_<random>`) for programmatic REST access. These routes are
//! themselves [`AuthUser`]-gated (so you authenticate with a JWT — or, indeed,
//! an existing PAT — to manage your tokens); the minted token can then be used
//! as a `Authorization: Bearer` credential on every other `AuthUser` route,
//! because the [`AuthUser`] extractor accepts a PAT wherever it accepts a JWT
//! (wired via [`aero_auth::AuthService::with_pat_verifier`] in the binary).
//!
//! Purely additive: a NEW route module folded into the main router by
//! [`crate::routes::build`]. No existing route is touched.
//!
//! ## Token confidentiality
//!
//! The plaintext token is returned **once**, in the `POST` response, and never
//! stored — only its SHA-256 hash is (see [`aero_storage::pat`]). Listing returns
//! metadata only ([`PatSummary`](aero_storage::PatSummary)); there is no endpoint
//! that can reveal a token after mint.

use std::str::FromStr;

use aero_common::{Error as AeroError, PatId, Result as AeroResult};
use aero_storage::pat::{generate_pat, hash_pat};
use aero_storage::PatRepo;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::post,
    Json, Router,
};
use serde::Deserialize;

use aero_auth::AuthUser;

use crate::error::ApiResult;
use crate::state::AppState;

/// Construct the PAT repository on demand from the shared pool. Repos are cheap
/// `Arc<PgPool>` wrappers, so this avoids threading a dedicated `AppState` field
/// through for the feature. The pool is reached via `participants.pool()` since
/// this build's `AppState` carries no standalone `pg` handle.
fn pat_repo(s: &AppState) -> PatRepo {
    PatRepo::new(s.participants.pool().clone())
}

/// Upper bound on a requested PAT lifetime: 10 years in seconds. A token can be
/// non-expiring (omit `expires_in_secs`), but a *positive* requested lifetime is
/// clamped so an absurd value can't overflow the timestamp arithmetic.
const MAX_EXPIRES_IN_SECS: i64 = 10 * 365 * 24 * 60 * 60;

/// Largest number of scopes a single mint accepts — a sanity bound, not a policy.
const MAX_SCOPES: usize = 64;

/// Mount the PAT management routes. Folded into the main router by
/// [`crate::routes::build`].
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/pat", post(mint_pat).get(list_pat))
        .route("/api/pat/:id", axum::routing::delete(revoke_pat))
}

#[derive(Debug, Default, Deserialize)]
struct MintPatReq {
    /// Optional human label so the owner can tell tokens apart.
    #[serde(default)]
    name: Option<String>,
    /// Reserved for future fine-grained authorization; stored, not yet enforced.
    #[serde(default)]
    scopes: Option<Vec<String>>,
    /// Lifetime in seconds from now. Omit (or `null`) for a non-expiring token.
    #[serde(default)]
    expires_in_secs: Option<i64>,
}

/// Resolve a requested `expires_in_secs` into an absolute expiry instant.
///
/// * `None` ⇒ `Ok(None)` (a non-expiring token).
/// * `Some(n)` with `n <= 0` ⇒ [`AeroError::Invalid`] (a token that is already
///   expired at mint is a client mistake, not a useful credential).
/// * `Some(n)` ⇒ `now + n`, with `n` clamped to [`MAX_EXPIRES_IN_SECS`].
///
/// Pure (takes `now`), so the boundary logic unit-tests without a clock.
fn resolve_expiry(
    expires_in_secs: Option<i64>,
    now: time::OffsetDateTime,
) -> AeroResult<Option<time::OffsetDateTime>> {
    match expires_in_secs {
        None => Ok(None),
        Some(secs) if secs <= 0 => Err(AeroError::Invalid(
            "expires_in_secs must be positive (omit it for a non-expiring token)".into(),
        )),
        Some(secs) => {
            let secs = secs.min(MAX_EXPIRES_IN_SECS);
            Ok(Some(now + time::Duration::seconds(secs)))
        }
    }
}

/// Normalize the requested scopes: default to empty, trim blanks, and enforce a
/// sane upper bound. Pure, so it unit-tests offline.
fn normalize_scopes(scopes: Option<Vec<String>>) -> AeroResult<Vec<String>> {
    let scopes: Vec<String> = scopes
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    if scopes.len() > MAX_SCOPES {
        return Err(AeroError::Invalid(format!(
            "too many scopes (max {MAX_SCOPES})"
        )));
    }
    Ok(scopes)
}

/// `POST /api/pat` — mint a Personal Access Token for the caller. The plaintext
/// token is returned **once** in the response (`{ "id", "token" }`) and never
/// stored; only its hash is. Clients must capture it now.
async fn mint_pat(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<MintPatReq>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let name = req
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let scopes = normalize_scopes(req.scopes)?;
    let expires_at = resolve_expiry(req.expires_in_secs, time::OffsetDateTime::now_utc())?;

    // Generate the plaintext once; persist only its hash.
    let token = generate_pat();
    let id = pat_repo(&s)
        .create(auth.participant_id, &hash_pat(&token), name, &scopes, expires_at)
        .await
        .map_err(AeroError::from)?;

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "id": id,
            // Shown ONCE — the client must store it now; the server keeps only its hash.
            "token": token,
        })),
    ))
}

/// `GET /api/pat` — list the caller's tokens (metadata only; never the hash or
/// plaintext), newest first.
async fn list_pat(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let tokens = pat_repo(&s)
        .list(auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "tokens": tokens })))
}

/// `DELETE /api/pat/:id` — revoke one of the caller's own tokens. Owner-scoped:
/// revoking a token you don't own (or one already revoked/absent) yields `404`,
/// so the endpoint never confirms the existence of another participant's token.
async fn revoke_pat(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let pat_id =
        PatId::from_str(&id).map_err(|e| AeroError::Invalid(format!("pat id: {e}")))?;
    let revoked = pat_repo(&s)
        .revoke(pat_id, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    if revoked {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AeroError::NotFound("pat token".into()).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_expiry_none_is_non_expiring() {
        let now = time::OffsetDateTime::now_utc();
        assert_eq!(resolve_expiry(None, now).unwrap(), None);
    }

    #[test]
    fn resolve_expiry_rejects_non_positive() {
        let now = time::OffsetDateTime::now_utc();
        assert!(resolve_expiry(Some(0), now).is_err());
        assert!(resolve_expiry(Some(-1), now).is_err());
    }

    #[test]
    fn resolve_expiry_adds_seconds_and_clamps() {
        let now = time::OffsetDateTime::now_utc();
        let in_hour = resolve_expiry(Some(3600), now).unwrap().unwrap();
        assert_eq!(in_hour, now + time::Duration::seconds(3600));

        // An absurd lifetime is clamped to the max, not passed through.
        let clamped = resolve_expiry(Some(i64::MAX), now).unwrap().unwrap();
        assert_eq!(clamped, now + time::Duration::seconds(MAX_EXPIRES_IN_SECS));
    }

    #[test]
    fn normalize_scopes_defaults_trims_and_filters() {
        assert_eq!(normalize_scopes(None).unwrap(), Vec::<String>::new());
        assert_eq!(
            normalize_scopes(Some(vec![" read ".into(), String::new(), "write".into()])).unwrap(),
            vec!["read".to_owned(), "write".to_owned()]
        );
    }

    #[test]
    fn normalize_scopes_rejects_too_many() {
        let many: Vec<String> = (0..=MAX_SCOPES).map(|i| format!("s{i}")).collect();
        assert!(normalize_scopes(Some(many)).is_err());
    }
}
