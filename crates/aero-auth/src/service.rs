//! High-level authentication service.
//!
//! Orchestrates [`ParticipantRepo`](aero_storage::ParticipantRepo), the
//! [`password`](crate::password) module, and the [`JwtCodec`](crate::jwt::JwtCodec)
//! into the four operations the HTTP layer needs: `register`, `login`, `refresh`,
//! `verify`.

use std::time::Duration;

use aero_common::{Error, Participant, Result};
use aero_storage::participant::NewHuman;
use aero_storage::ParticipantRepo;
use serde::{Deserialize, Serialize};

use crate::jwt::{Claims, JwtCodec, TokenKind};
use crate::password;
use crate::pat::SharedPatVerifier;

#[derive(Debug, Clone, Deserialize)]
pub struct RegisterRequest {
    pub email: String,
    pub password: String,
    pub display_name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthTokens {
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegisterResponse {
    pub participant: Participant,
    pub access_token: String,
    pub refresh_token: String,
}

/// Cheap-to-clone (clones share the underlying repo, JWT keys, and — when wired —
/// the PAT verifier behind an `Arc`).
#[derive(Clone)]
pub struct AuthService {
    repo: ParticipantRepo,
    jwt: JwtCodec,
    /// Optional hook for Personal Access Token (PAT) auth. When present, the
    /// [`AuthUser`](crate::extractor::AuthUser) extractor accepts an
    /// `aero_pat_*` bearer token wherever it accepts an access JWT, resolving it
    /// through this verifier. `None` (the default) means PAT auth is disabled and
    /// only JWTs are accepted — behaviour is then identical to before PATs.
    pat_verifier: Option<SharedPatVerifier>,
    /// Optional per-account login lockout (ROADMAP5 方向五). `None` (default) =
    /// disabled; when present, [`Self::login`] rejects locked accounts and records
    /// each auth failure / success. Shared (`Arc`) so the in-process failure state
    /// survives `AuthService` clones across handlers.
    login_throttle: Option<std::sync::Arc<crate::login_throttle::LoginThrottle>>,
}

impl AuthService {
    /// Constructs the service from its dependencies. Prefer [`Self::from_pem`]
    /// when starting up directly from `AuthConfig`. PAT auth is off until
    /// [`Self::with_pat_verifier`] is called.
    pub fn new(repo: ParticipantRepo, jwt: JwtCodec) -> Self {
        Self {
            repo,
            jwt,
            pat_verifier: None,
            login_throttle: None,
        }
    }

    /// Enable per-account login lockout by injecting a shared
    /// [`LoginThrottle`](crate::login_throttle::LoginThrottle) (typically
    /// [`LoginThrottle::from_env`](crate::login_throttle::LoginThrottle::from_env)).
    /// Builder-style, composes with [`Self::new`] / [`Self::from_pem`] at startup.
    #[must_use]
    pub fn with_login_throttle(
        mut self,
        throttle: std::sync::Arc<crate::login_throttle::LoginThrottle>,
    ) -> Self {
        self.login_throttle = Some(throttle);
        self
    }

    /// Enable Personal Access Token authentication by injecting the verifier that
    /// resolves a hashed PAT to its owner (typically an
    /// [`aero_storage::PatRepo`], which implements
    /// [`PatVerifier`](crate::pat::PatVerifier)). Builder-style so it composes
    /// with [`Self::new`] / [`Self::from_pem`] at startup:
    ///
    /// ```ignore
    /// let auth = AuthService::from_pem(repo.clone(), ..)?
    ///     .with_pat_verifier(Arc::new(PatRepo::new(pg.clone())));
    /// ```
    #[must_use]
    pub fn with_pat_verifier(mut self, verifier: SharedPatVerifier) -> Self {
        self.pat_verifier = Some(verifier);
        self
    }

    /// Convenience constructor that builds the JWT codec inline from PEM strings.
    pub fn from_pem(
        repo: ParticipantRepo,
        private_pem: &str,
        public_pem: &str,
        issuer: impl Into<String>,
        access_ttl: Duration,
        refresh_ttl: Duration,
    ) -> Result<Self> {
        let jwt = JwtCodec::from_pem(private_pem, public_pem, issuer, access_ttl, refresh_ttl)?;
        Ok(Self::new(repo, jwt))
    }

    /// Exposes the underlying JWT codec — handy for tests and for components
    /// that need to verify tokens but don't talk to the DB.
    pub fn jwt(&self) -> &JwtCodec {
        &self.jwt
    }

    /// Validates input, hashes the password, creates the participant + credential
    /// row, and issues a fresh token pair.
    ///
    /// Returns:
    /// * [`Error::Invalid`] for empty fields.
    /// * [`Error::Conflict`] if the email is already registered.
    /// * [`Error::Database`] for unexpected SQL errors.
    #[tracing::instrument(skip(self, req), fields(email = %req.email))]
    pub async fn register(&self, req: RegisterRequest) -> Result<RegisterResponse> {
        validate_email(&req.email)?;
        validate_password(&req.password)?;
        if req.display_name.trim().is_empty() {
            return Err(Error::Invalid("display_name must not be empty".into()));
        }

        let phc = password::hash(&req.password)?;
        let participant = match self
            .repo
            .create_human(NewHuman {
                email: req.email,
                display_name: req.display_name,
                password_hash: phc,
            })
            .await
        {
            Ok(p) => p,
            Err(err) => return Err(map_create_error(err)),
        };

        let tokens = self.issue_pair(participant.id)?;
        Ok(RegisterResponse {
            participant,
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
        })
    }

    /// Looks up the credential row, verifies the password, and issues tokens.
    ///
    /// Returns [`Error::Unauthorized`] on either unknown email or wrong password —
    /// we intentionally return the same error in both cases to avoid leaking
    /// account-existence information.
    #[tracing::instrument(skip(self, req), fields(email = %req.email))]
    pub async fn login(&self, req: LoginRequest) -> Result<RegisterResponse> {
        // Per-account lockout (opt-in). Reject a locked account before touching the
        // password hash, and fold the outcome (auth failure vs success) back into
        // the throttle. Only `Unauthorized` outcomes count as failures — a DB error
        // is not the user's fault and must not march them toward a lockout.
        let Some(throttle) = self.login_throttle.clone() else {
            return self.login_inner(req).await;
        };
        let account = req.email.clone();
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        if throttle.is_locked(&account, now) {
            return Err(Error::Unauthorized(
                "account temporarily locked after repeated failed logins".into(),
            ));
        }
        let result = self.login_inner(req).await;
        match &result {
            Ok(_) => throttle.record_success(&account),
            Err(Error::Unauthorized(_)) => throttle.record_failure(&account, now),
            Err(_) => {}
        }
        result
    }

    /// The credential check itself, factored out so [`Self::login`] can wrap it with
    /// the optional lockout bookkeeping.
    async fn login_inner(&self, req: LoginRequest) -> Result<RegisterResponse> {
        let creds = self
            .repo
            .find_credentials_by_email(&req.email)
            .await?
            .ok_or_else(|| Error::Unauthorized("invalid credentials".into()))?;

        password::verify(&req.password, &creds.password_hash)?;

        let participant = self
            .repo
            .get(creds.participant_id)
            .await?
            .ok_or_else(|| Error::Unauthorized("participant missing".into()))?;

        let tokens = self.issue_pair(participant.id)?;
        Ok(RegisterResponse {
            participant,
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
        })
    }

    /// Validates a refresh token and issues a new access token (refresh token
    /// is reused — rotation is left to a follow-up).
    #[tracing::instrument(skip(self, refresh_token))]
    pub async fn refresh(&self, refresh_token: &str) -> Result<AuthTokens> {
        let claims = self.jwt.verify(refresh_token)?;
        if claims.kind != TokenKind::Refresh {
            return Err(Error::Unauthorized("not a refresh token".into()));
        }
        let pid = claims.participant_id()?;
        let access_token = self.jwt.issue(pid, TokenKind::Access)?;
        Ok(AuthTokens {
            access_token,
            refresh_token: refresh_token.to_string(),
        })
    }

    /// Verifies a bearer token without consulting the DB. Returns the decoded
    /// claims. Used by the [`AuthUser`](crate::extractor::AuthUser) extractor.
    pub fn verify(&self, token: &str) -> Result<Claims> {
        self.jwt.verify(token)
    }

    /// Mint a fresh access+refresh token pair for an already-known participant,
    /// without a password check.
    ///
    /// This is the entry point for non-password logins that have *already*
    /// authenticated the participant by other means — e.g. the SSO/OIDC flow,
    /// which validates an external IdP's ID token and then needs *our* tokens for
    /// the resolved (or JIT-provisioned) participant. It is the same token
    /// material `register`/`login` issue, just decoupled from credential
    /// verification.
    pub fn issue_for_participant(&self, pid: aero_common::ParticipantId) -> Result<AuthTokens> {
        self.issue_pair(pid)
    }

    /// Attempt to authenticate a *plaintext* Personal Access Token, returning its
    /// owner on success.
    ///
    /// Returns `None` when PAT auth is not wired (no verifier injected), the token
    /// is not a **well-formed** PAT (see [`is_well_formed_pat`]), or it is unknown /
    /// revoked / expired. The [`AuthUser`](crate::extractor::AuthUser) extractor
    /// calls this **after** a JWT verification fails, so the JWT path is unchanged
    /// when no PAT is present.
    ///
    /// The structural check happens *before* hashing and the DB lookup, so a
    /// garbage bearer (`aero_pat_` with an empty / wrong-length / non-hex body, or
    /// an arbitrarily large blob) is rejected in constant time without burning a
    /// SHA-256 and a database round-trip — and can never collide with a stored
    /// hash, since every minted token is exactly this shape (see
    /// [`aero_storage::pat::generate_pat`]). On a well-formed token the plaintext
    /// is hashed here (via [`aero_storage::pat::hash_pat`]) and only the hash is
    /// handed to the verifier — the raw secret never reaches storage.
    pub async fn verify_pat(&self, token: &str) -> Option<aero_common::ParticipantId> {
        let verifier = self.pat_verifier.as_ref()?;
        if !is_well_formed_pat(token) {
            return None;
        }
        let hash = aero_storage::pat::hash_pat(token);
        verifier.verify(&hash).await
    }

    fn issue_pair(&self, pid: aero_common::ParticipantId) -> Result<AuthTokens> {
        Ok(AuthTokens {
            access_token: self.jwt.issue(pid, TokenKind::Access)?,
            refresh_token: self.jwt.issue(pid, TokenKind::Refresh)?,
        })
    }
}

/// Length, in hex characters, of a PAT's random body — 256 bits of entropy
/// (32 bytes) rendered as lowercase hex by
/// [`generate_pat`](aero_storage::pat::generate_pat).
const PAT_BODY_HEX_LEN: usize = 64;

/// Is `token` a structurally well-formed Personal Access Token?
///
/// A minted PAT is *exactly* [`PAT_PREFIX`](aero_storage::pat::PAT_PREFIX)
/// followed by [`PAT_BODY_HEX_LEN`] lowercase hex characters — nothing more,
/// nothing less. Checking that shape before hashing lets [`verify_pat`] reject a
/// malformed bearer (empty body, wrong length, uppercase/non-hex characters, or a
/// huge blob) in constant time, sparing a needless SHA-256 and DB lookup. The
/// check is purely structural — a well-formed *but unknown* token still returns
/// `None` from the verifier — so it widens no trust, only narrows wasted work.
///
/// Lowercase-only is deliberate: `generate_pat` emits `{:02x}` (lowercase) and
/// `hash_pat` is case-sensitive over the raw bytes, so an uppercased copy of a
/// real token would hash differently and never match anyway — rejecting it here
/// is both correct and cheaper.
///
/// [`verify_pat`]: AuthService::verify_pat
#[must_use]
fn is_well_formed_pat(token: &str) -> bool {
    let Some(body) = token.strip_prefix(aero_storage::pat::PAT_PREFIX) else {
        return false;
    };
    body.len() == PAT_BODY_HEX_LEN
        && body.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn validate_email(email: &str) -> Result<()> {
    let trimmed = email.trim();
    if trimmed.is_empty() || !trimmed.contains('@') || trimmed.len() > 320 {
        return Err(Error::Invalid("email is malformed".into()));
    }
    Ok(())
}

fn validate_password(password: &str) -> Result<()> {
    // Delegate to the configurable policy (ROADMAP 方向五). Default behaviour is
    // unchanged (min 8 / max 1024 / no class requirement); operators tighten via
    // AERO_PASSWORD_* env without a code change.
    crate::password_policy::validate(password)
}

/// Maps a `create_human` sqlx error to the right `aero_common::Error`.
/// Unique-violation on `credentials.email` becomes `Conflict`.
fn map_create_error(err: sqlx::Error) -> Error {
    if let Some(db_err) = err.as_database_error() {
        if db_err.is_unique_violation() {
            return Error::Conflict("email already registered".into());
        }
    }
    Error::Database(err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_email_rejects_garbage() {
        assert!(validate_email("").is_err());
        assert!(validate_email("noatsign").is_err());
        assert!(validate_email("ok@example.com").is_ok());
    }

    #[test]
    fn validate_password_enforces_min_len() {
        assert!(validate_password("short").is_err());
        assert!(validate_password("longenough").is_ok());
    }

    #[test]
    fn well_formed_pat_accepts_freshly_minted_tokens() {
        // Whatever `generate_pat` mints must pass the structural gate, or
        // `verify_pat` would reject every real token before the DB ever sees it.
        for _ in 0..16 {
            let token = aero_storage::pat::generate_pat();
            assert!(
                is_well_formed_pat(&token),
                "generate_pat output must be well-formed: {token}"
            );
        }
    }

    #[test]
    fn well_formed_pat_rejects_malformed_bodies() {
        let prefix = aero_storage::pat::PAT_PREFIX;
        // Missing prefix entirely (e.g. a JWT or random bearer).
        assert!(!is_well_formed_pat("not-a-pat"));
        assert!(!is_well_formed_pat(&"a".repeat(PAT_BODY_HEX_LEN)));
        // Bare prefix with an empty body.
        assert!(!is_well_formed_pat(prefix));
        // Body one hex char too short / too long.
        assert!(!is_well_formed_pat(&format!("{prefix}{}", "a".repeat(PAT_BODY_HEX_LEN - 1))));
        assert!(!is_well_formed_pat(&format!("{prefix}{}", "a".repeat(PAT_BODY_HEX_LEN + 1))));
        // Correct length but a non-hex character ('g') in the body.
        assert!(!is_well_formed_pat(&format!("{prefix}{}", "g".repeat(PAT_BODY_HEX_LEN))));
        // Correct length and hex digits but uppercase — generate_pat emits
        // lowercase, and hash_pat is case-sensitive, so this could never match.
        assert!(!is_well_formed_pat(&format!("{prefix}{}", "A".repeat(PAT_BODY_HEX_LEN))));
        // A pathologically large blob is rejected on length alone (no hashing).
        assert!(!is_well_formed_pat(&format!("{prefix}{}", "a".repeat(100_000))));
    }

    #[test]
    fn well_formed_pat_accepts_exact_lowercase_hex_body() {
        let prefix = aero_storage::pat::PAT_PREFIX;
        // Every lowercase hex digit, padded to the exact body length, is accepted.
        let body: String = "0123456789abcdef".repeat(PAT_BODY_HEX_LEN / 16);
        assert_eq!(body.len(), PAT_BODY_HEX_LEN);
        assert!(is_well_formed_pat(&format!("{prefix}{body}")));
    }
}
