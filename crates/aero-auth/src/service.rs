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
        }
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
    /// does not carry the PAT prefix, or it is unknown / revoked / expired. The
    /// [`AuthUser`](crate::extractor::AuthUser) extractor calls this **after** a
    /// JWT verification fails, so the JWT path is unchanged when no PAT is present.
    ///
    /// The plaintext is hashed here (via [`aero_storage::pat::hash_pat`]) and only
    /// the hash is handed to the verifier — the raw secret never reaches storage.
    pub async fn verify_pat(&self, token: &str) -> Option<aero_common::ParticipantId> {
        let verifier = self.pat_verifier.as_ref()?;
        if !token.starts_with(aero_storage::pat::PAT_PREFIX) {
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

fn validate_email(email: &str) -> Result<()> {
    let trimmed = email.trim();
    if trimmed.is_empty() || !trimmed.contains('@') || trimmed.len() > 320 {
        return Err(Error::Invalid("email is malformed".into()));
    }
    Ok(())
}

fn validate_password(password: &str) -> Result<()> {
    if password.len() < 8 {
        return Err(Error::Invalid("password must be at least 8 chars".into()));
    }
    if password.len() > 1024 {
        return Err(Error::Invalid("password is too long".into()));
    }
    Ok(())
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
}
