//! High-level authentication service.
//!
//! Orchestrates [`ParticipantRepo`](aero_storage::ParticipantRepo), the
//! [`password`](crate::password) module, and the [`JwtCodec`](crate::jwt::JwtCodec)
//! into the four operations the HTTP layer needs: `register`, `login`, `refresh`,
//! `verify`.

use std::time::Duration;

use aero_common::{Error, Participant, ParticipantId, Result, SessionId, WorkspaceId};
use aero_storage::participant::NewHuman;
use aero_storage::revoked_token::hash_token;
use aero_storage::{AuditRepo, NewRegistration, ParticipantRepo, RegistrationRepo, SessionRepo};
use serde::{Deserialize, Serialize};

use crate::audit_tokens::{AUTH_LOGIN_FAILED, AUTH_LOGIN_LOCKED};
use crate::bot::SharedBotVerifier;
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
    /// Internal stable id shared by both JWTs; intentionally absent from the
    /// public JSON response because it is already carried in the signed claims.
    #[serde(skip)]
    pub session_id: SessionId,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegisterResponse {
    pub participant: Participant,
    pub access_token: String,
    pub refresh_token: String,
    /// Internal stable id shared by both JWTs.
    #[serde(skip)]
    pub session_id: SessionId,
}

/// Cheap-to-clone (clones share the underlying repo, JWT keys, and — when wired —
/// the PAT verifier behind an `Arc`).
#[derive(Clone)]
pub struct AuthService {
    repo: ParticipantRepo,
    sessions: SessionRepo,
    jwt: JwtCodec,
    /// Optional hook for Personal Access Token (PAT) auth. When present, the
    /// [`AuthUser`](crate::extractor::AuthUser) extractor accepts an
    /// `aero_pat_*` bearer token wherever it accepts an access JWT, resolving it
    /// through this verifier. `None` (the default) means PAT auth is disabled and
    /// only JWTs are accepted — behaviour is then identical to before PATs.
    pat_verifier: Option<SharedPatVerifier>,
    /// Optional hook for bot-token auth (方向三 — open platform). When present, the
    /// [`AuthUser`](crate::extractor::AuthUser) extractor accepts a `bot_*` bearer
    /// token wherever it accepts an access JWT or a PAT, resolving it to the bot's
    /// participant identity through this verifier. `None` (the default) means bot
    /// auth is disabled and the extractor's behaviour is identical to before — only
    /// JWTs and (when wired) PATs are accepted.
    bot_verifier: Option<SharedBotVerifier>,
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
        let sessions = SessionRepo::new(repo.pool().clone());
        Self {
            repo,
            sessions,
            jwt,
            pat_verifier: None,
            bot_verifier: None,
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

    /// Periodic maintenance hook: evict decision-dead entries from the in-process
    /// login throttle so its failure map cannot grow unbounded under
    /// credential-stuffing across many distinct accounts. No-op when the throttle
    /// is disabled (the default) or Redis-backed (self-expiring). `now` is unix
    /// seconds. Returns the number of entries removed. Driven from the gateway's
    /// periodic sweep loop alongside the rate-limiter / spam-guard sweeps.
    pub async fn sweep_login_throttle(&self, now: i64) -> usize {
        match &self.login_throttle {
            Some(t) => t.sweep(now).await,
            None => 0,
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

    /// Enable bot-token authentication (方向三 — open platform) by injecting the
    /// verifier that resolves an opaque bot token to its bot participant (typically
    /// an [`aero_storage::BotRepo`], which implements
    /// [`BotTokenVerifier`](crate::bot::BotTokenVerifier)). Builder-style so it
    /// composes with [`Self::new`] / [`Self::from_pem`] / [`Self::with_pat_verifier`]
    /// at startup:
    ///
    /// ```ignore
    /// let auth = AuthService::new(repo.clone(), jwt)
    ///     .with_pat_verifier(Arc::new(PatRepo::new(pg.clone())))
    ///     .with_bot_verifier(Arc::new(BotRepo::new(pg.clone())));
    /// ```
    #[must_use]
    pub fn with_bot_verifier(mut self, verifier: SharedBotVerifier) -> Self {
        self.bot_verifier = Some(verifier);
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
        validate_registration(&req)?;

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
            session_id: tokens.session_id,
        })
    }

    /// Register a first-party account together with its initial tenant
    /// enrollment and refresh-session inventory.
    ///
    /// JWTs are prepared before database mutation; all persistent rows then
    /// commit through [`RegistrationRepo`] in one transaction. A database failure
    /// therefore occupies neither the email nor participant id, and a successful
    /// response can always be revoked through the session inventory it exposes.
    #[tracing::instrument(
        skip(self, req, user_agent),
        fields(email = %req.email, %workspace)
    )]
    pub async fn register_enrolled(
        &self,
        req: RegisterRequest,
        workspace: WorkspaceId,
        user_agent: Option<&str>,
    ) -> Result<RegisterResponse> {
        validate_registration(&req)?;
        let password_hash = password::hash(&req.password)?;
        let participant_id = ParticipantId::new();
        let tokens = self.issue_pair(participant_id)?;
        let refresh_token_hash = hash_token(&tokens.refresh_token);
        let participant = RegistrationRepo::new(self.repo.pool().clone())
            .create(NewRegistration {
                participant_id,
                email: req.email,
                display_name: req.display_name,
                password_hash,
                workspace_id: workspace,
                session_id: tokens.session_id,
                refresh_token_hash,
                user_agent: user_agent.map(ToOwned::to_owned),
            })
            .await
            .map_err(map_create_error)?;

        Ok(RegisterResponse {
            participant,
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            session_id: tokens.session_id,
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
        //
        // Security-event audit: every `Unauthorized` outcome emits an
        // `auth.login.failed` row, and the lockout-reject path emits
        // `auth.login.locked` — best-effort (`let _` + warn), so the audit can
        // never alter the login outcome, and funneled through ONE emission point
        // per event class so the throttle-less early return cannot skip the trail
        // (the throttle is a config; the audit trail is not).
        let account = req.email.clone();
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let result = match self.login_throttle.clone() {
            Some(throttle) if throttle.is_locked(&account, now).await => {
                self.audit_login_event(AUTH_LOGIN_LOCKED, &account, "locked")
                    .await;
                return Err(Error::Unauthorized(
                    "account temporarily locked after repeated failed logins".into(),
                ));
            }
            Some(throttle) => {
                let result = self.login_inner(req).await;
                // Record a FAILURE here for a bad password. SUCCESS is deliberately
                // NOT recorded yet: a post-password gate (2FA, enforced by the HTTP
                // layer) may still reject this attempt. Recording success now would
                // clear the failure counter before 2FA is checked, letting an
                // attacker who holds the password brute-force the second factor with
                // the lockout permanently reset. The caller MUST call
                // [`finalize_login`] once all gates have run.
                if let Err(Error::Unauthorized(_)) = &result {
                    throttle.record_failure(&account, now).await;
                }
                result
            }
            None => self.login_inner(req).await,
        };
        if let Err(Error::Unauthorized(_)) = &result {
            self.audit_login_event(AUTH_LOGIN_FAILED, &account, "invalid_credentials")
                .await;
        }
        result
    }

    /// Record the FINAL login outcome on the per-account lockout throttle, AFTER
    /// every post-password gate (e.g. 2FA) has run. `success = true` clears the
    /// failure counter; `success = false` records a failure so a wrong second
    /// factor advances the lockout exactly like a wrong password. No-op when the
    /// throttle is disabled. See the deferral rationale in [`login`](Self::login).
    ///
    /// Security-event audit: a `success = false` outcome emits an
    /// `auth.login.failed` row with `reason = "2fa_failed"` — BEFORE the throttle
    /// early-return, so the trail is throttle-independent (a wrong second factor
    /// is an account-takeover signal regardless of config). `success = true` is
    /// deliberately silent (durable `login_events` already records successes).
    pub async fn finalize_login(&self, account: &str, success: bool) {
        if !success {
            self.audit_login_event(AUTH_LOGIN_FAILED, account, "2fa_failed")
                .await;
        }
        let Some(throttle) = self.login_throttle.clone() else {
            return;
        };
        if success {
            throttle.record_success(account).await;
        } else {
            let now = time::OffsetDateTime::now_utc().unix_timestamp();
            throttle.record_failure(account, now).await;
        }
    }

    /// Best-effort `auth.login.*` audit append (workspace nil, actor None — the
    /// account is not resolvable on the failure path; the 0239 envelope CASE
    /// yields `{id: "system", type: "system"}`).
    ///
    /// Fail-open invariant (mirrors `record_login_event`'s "Never fails the
    /// login"): the `Result` is discarded — a storage hiccup must never turn a
    /// login attempt into a 500, and must never march an account toward/away
    /// from a lockout. The loss is made LOUD: a structured `tracing::warn!`
    /// carrying `token`/`account`/`reason` keeps the log stream a reconstructable
    /// fallback trail, and `aero_audit_append_failed_total{token}` makes the
    /// silent-loss window operationally visible. (`audit_events` and the durable
    /// `login_failures` brute-force trail share the same PG, so at-least-once
    /// staging would need a second store — deliberately not added; the handler
    /// over-records `login_failures` on any `Err` including DB errors while the
    /// audit trail under-records by design — documented divergence, not a bug.)
    async fn audit_login_event(&self, token: &str, account: &str, reason: &str) {
        let detail = serde_json::json!({ "email": account, "reason": reason });
        if let Err(e) = AuditRepo::new(self.repo.pool().clone())
            .append(WorkspaceId::nil(), None, token, None, detail)
            .await
        {
            tracing::warn!(
                error = %e, token, account, reason,
                "auth audit append failed (login outcome unaffected)"
            );
            // Literal metric name: single emit site today (keeps the 3-crate
            // footprint); promote to `aero_common::metrics::names` if a second
            // emit site appears. `inc_counter_labeled` auto-creates the series
            // on first use.
            aero_common::metrics::inc_counter_labeled(
                "aero_audit_append_failed_total",
                1,
                &[("token", token)],
            );
        }
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
            session_id: tokens.session_id,
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
        let session_id = if let Some(id) = claims.session_id()? {
            if !self.sessions.is_active(id, pid).await? {
                return Err(Error::Unauthorized("session is not active".into()));
            }
            id
        } else {
            let hash = aero_storage::revoked_token::hash_token(refresh_token);
            self.sessions
                .active_id_by_hash(pid, &hash)
                .await?
                .ok_or_else(|| Error::Unauthorized("refresh session is not active".into()))?
        };
        let access_token = self
            .jwt
            .issue_for_session(pid, TokenKind::Access, session_id)?;
        Ok(AuthTokens {
            access_token,
            refresh_token: refresh_token.to_string(),
            session_id,
        })
    }

    /// Verifies a bearer token without consulting the DB. Returns the decoded
    /// claims. Used by the [`AuthUser`](crate::extractor::AuthUser) extractor.
    pub fn verify(&self, token: &str) -> Result<Claims> {
        self.jwt.verify(token)
    }

    /// Verify an access JWT, require its stable `sid`, and require the
    /// corresponding owner-scoped session row to still be active. Sid-less
    /// access tokens are rejected: without a session binding, remote/global
    /// sign-out could not invalidate the token before its expiry.
    pub async fn verify_access(&self, token: &str) -> Result<Claims> {
        let claims = self.jwt.verify(token)?;
        self.assert_access_claims_active(&claims).await?;
        Ok(claims)
    }

    /// Validate already-signature-checked claims. Kept crate-visible so the Axum
    /// extractor can avoid verifying the JWT twice while still distinguishing a
    /// valid-but-revoked JWT (hard reject) from an opaque PAT/bot candidate.
    pub(crate) async fn assert_access_claims_active(&self, claims: &Claims) -> Result<()> {
        if claims.kind != TokenKind::Access {
            return Err(Error::Unauthorized("not an access token".into()));
        }
        let participant = claims.participant_id()?;
        let session = claims
            .session_id()?
            .ok_or_else(|| Error::Unauthorized("access token missing session id".into()))?;
        if !self.sessions.is_active(session, participant).await? {
            return Err(Error::Unauthorized("session is not active".into()));
        }
        Ok(())
    }

    /// Mint a fresh access+refresh token pair for an already-known participant,
    /// without a password check.
    ///
    /// This is the entry point for non-password logins that have *already*
    /// authenticated the participant by other means — e.g. the SSO/OIDC flow,
    /// which validates an external `IdP`'s ID token and then needs *our* tokens for
    /// the resolved (or JIT-provisioned) participant. It is the same token
    /// material `register`/`login` issue, just decoupled from credential
    /// verification.
    pub fn issue_for_participant(&self, pid: aero_common::ParticipantId) -> Result<AuthTokens> {
        self.issue_pair(pid)
    }

    /// Mint a fresh pair for an existing stable session (refresh rotation).
    pub fn issue_for_session(
        &self,
        pid: aero_common::ParticipantId,
        session_id: SessionId,
    ) -> Result<AuthTokens> {
        self.issue_pair_for_session(pid, session_id)
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

    /// Attempt to authenticate a *plaintext* bot token (方向三), returning the bot's
    /// participant id on success.
    ///
    /// Returns `None` when bot auth is not wired (no verifier injected), the token
    /// is not a **well-formed** bot token (the `bot_` prefix — see
    /// [`is_well_formed_bot_token`]), or it is unknown / un-issued / belongs to a
    /// deleted bot. The [`AuthUser`](crate::extractor::AuthUser) extractor calls this
    /// **after** both the JWT and PAT paths fail, so those paths are unchanged when
    /// no bot token is present.
    ///
    /// The prefix check happens *before* the DB lookup, so a bearer that is clearly
    /// not a bot token (a JWT, a PAT, or random garbage) is rejected without a query.
    /// Unlike PATs, the plaintext (not a pre-hash) is handed to the verifier: bot
    /// tokens hash with a different helper than PATs, so the hashing stays on the
    /// storage side ([`aero_storage::BotRepo::verify_token`]) next to that helper.
    /// The deleted-bot rejection is enforced there (a `participants.deleted_at IS
    /// NULL` guard mirroring the PAT verifier), so a deleted bot can never authenticate.
    pub async fn verify_bot_token(&self, token: &str) -> Option<aero_common::ParticipantId> {
        let verifier = self.bot_verifier.as_ref()?;
        if !is_well_formed_bot_token(token) {
            return None;
        }
        verifier.verify(token).await
    }

    fn issue_pair(&self, pid: aero_common::ParticipantId) -> Result<AuthTokens> {
        self.issue_pair_for_session(pid, SessionId::new())
    }

    fn issue_pair_for_session(
        &self,
        pid: aero_common::ParticipantId,
        session_id: SessionId,
    ) -> Result<AuthTokens> {
        Ok(AuthTokens {
            access_token: self
                .jwt
                .issue_for_session(pid, TokenKind::Access, session_id)?,
            refresh_token: self
                .jwt
                .issue_for_session(pid, TokenKind::Refresh, session_id)?,
            session_id,
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
        && body
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Is `token` a structurally well-formed bot token?
///
/// A minted bot token is [`BotRepo::TOKEN_PREFIX`](aero_storage::BotRepo::TOKEN_PREFIX)
/// (`bot_`) followed by a non-empty body (a UUID v4 — see
/// [`BotRepo::rotate_token`](aero_storage::BotRepo::rotate_token)). Checking the
/// prefix before the DB lookup lets [`verify_bot_token`] reject a bearer that is
/// clearly not a bot token (a JWT, a PAT, or random garbage) without a query. The
/// check is purely structural — a well-formed *but unknown* token still returns
/// `None` from the verifier — so it widens no trust, only narrows wasted work.
///
/// We deliberately do **not** validate the UUID shape here: bot tokens are looked
/// up by exact hash, so a malformed body simply fails to match. Keeping the gate to
/// "has the prefix and a non-empty body" avoids coupling auth to the exact token
/// format while still cheaply skipping non-bot bearers.
///
/// [`verify_bot_token`]: AuthService::verify_bot_token
#[must_use]
fn is_well_formed_bot_token(token: &str) -> bool {
    aero_storage::BotRepo::TOKEN_PREFIX
        .len()
        .checked_add(1)
        .is_some_and(|min_len| {
            token.starts_with(aero_storage::BotRepo::TOKEN_PREFIX) && token.len() >= min_len
        })
}

fn validate_registration(req: &RegisterRequest) -> Result<()> {
    validate_email(&req.email)?;
    validate_password(&req.password)?;
    if req.display_name.trim().is_empty() {
        return Err(Error::Invalid("display_name must not be empty".into()));
    }
    Ok(())
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
    use rand::rngs::OsRng;
    use rsa::pkcs1::EncodeRsaPublicKey;
    use rsa::pkcs8::EncodePrivateKey;
    use rsa::RsaPrivateKey;

    fn token_service() -> AuthService {
        let mut rng = OsRng;
        let private = RsaPrivateKey::new(&mut rng, 2048).expect("rsa keygen");
        let public = private.to_public_key();
        let private_pem = private
            .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
            .unwrap()
            .to_string();
        let public_pem = public.to_pkcs1_pem(rsa::pkcs8::LineEnding::LF).unwrap();
        let jwt = JwtCodec::from_pem(
            &private_pem,
            &public_pem,
            "aero-im",
            Duration::from_secs(60),
            Duration::from_secs(600),
        )
        .unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://aero:aero_dev_pw@localhost:5432/aero")
            .unwrap();
        AuthService::new(ParticipantRepo::new(pool), jwt)
    }

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
        assert!(!is_well_formed_pat(&format!(
            "{prefix}{}",
            "a".repeat(PAT_BODY_HEX_LEN - 1)
        )));
        assert!(!is_well_formed_pat(&format!(
            "{prefix}{}",
            "a".repeat(PAT_BODY_HEX_LEN + 1)
        )));
        // Correct length but a non-hex character ('g') in the body.
        assert!(!is_well_formed_pat(&format!(
            "{prefix}{}",
            "g".repeat(PAT_BODY_HEX_LEN)
        )));
        // Correct length and hex digits but uppercase — generate_pat emits
        // lowercase, and hash_pat is case-sensitive, so this could never match.
        assert!(!is_well_formed_pat(&format!(
            "{prefix}{}",
            "A".repeat(PAT_BODY_HEX_LEN)
        )));
        // A pathologically large blob is rejected on length alone (no hashing).
        assert!(!is_well_formed_pat(&format!(
            "{prefix}{}",
            "a".repeat(100_000)
        )));
    }

    #[test]
    fn well_formed_bot_token_accepts_minted_shape() {
        // A token shaped like `BotRepo::rotate_token` mints (`bot_<uuid>`) must
        // pass the structural gate, or `verify_bot_token` would reject every real
        // token before the DB ever sees it.
        let prefix = aero_storage::BotRepo::TOKEN_PREFIX;
        for _ in 0..16 {
            let token = format!("{prefix}{}", uuid::Uuid::new_v4());
            assert!(is_well_formed_bot_token(&token), "must accept: {token}");
        }
    }

    #[test]
    fn well_formed_bot_token_rejects_non_bot_bearers() {
        let prefix = aero_storage::BotRepo::TOKEN_PREFIX;
        // No prefix at all (a JWT, random garbage).
        assert!(!is_well_formed_bot_token("not-a-bot-token"));
        assert!(!is_well_formed_bot_token(""));
        // A PAT must NOT be treated as a bot token — disjoint prefixes.
        assert!(!is_well_formed_bot_token(&aero_storage::pat::generate_pat()));
        // Bare prefix with an empty body is rejected (needs a non-empty body).
        assert!(!is_well_formed_bot_token(prefix));
        // Prefix + a single char body is the minimal acceptable shape.
        assert!(is_well_formed_bot_token(&format!("{prefix}x")));
    }

    #[test]
    fn pat_and_bot_token_gates_are_disjoint() {
        // A real PAT passes the PAT gate but not the bot gate, and a real bot
        // token passes the bot gate but not the PAT gate. This disjointness is
        // what makes the extractor's "try PAT, then bot" ordering safe — a given
        // bearer self-routes to at most one DB lookup.
        let pat = aero_storage::pat::generate_pat();
        let bot = format!(
            "{}{}",
            aero_storage::BotRepo::TOKEN_PREFIX,
            uuid::Uuid::new_v4()
        );
        assert!(is_well_formed_pat(&pat) && !is_well_formed_bot_token(&pat));
        assert!(is_well_formed_bot_token(&bot) && !is_well_formed_pat(&bot));
    }

    #[test]
    fn well_formed_pat_accepts_exact_lowercase_hex_body() {
        let prefix = aero_storage::pat::PAT_PREFIX;
        // Every lowercase hex digit, padded to the exact body length, is accepted.
        let body: String = "0123456789abcdef".repeat(PAT_BODY_HEX_LEN / 16);
        assert_eq!(body.len(), PAT_BODY_HEX_LEN);
        assert!(is_well_formed_pat(&format!("{prefix}{body}")));
    }

    #[tokio::test]
    async fn issued_auth_pair_shares_session_and_has_distinct_jtis() {
        let service = token_service();
        let participant = aero_common::ParticipantId::new();
        let tokens = service.issue_for_participant(participant).unwrap();
        let access = service.verify(&tokens.access_token).unwrap();
        let refresh = service.verify(&tokens.refresh_token).unwrap();

        assert_eq!(access.session_id().unwrap(), Some(tokens.session_id));
        assert_eq!(refresh.session_id().unwrap(), Some(tokens.session_id));
        assert_ne!(access.jti, refresh.jti);
    }

    #[tokio::test]
    async fn sidless_access_claims_fail_before_session_lookup() {
        let service = token_service();
        let claims = Claims {
            sub: aero_common::ParticipantId::new().to_string(),
            iss: "aero-im".into(),
            iat: 1,
            exp: u64::MAX,
            kind: TokenKind::Access,
            sid: None,
            jti: "legacy".into(),
        };
        let error = service
            .assert_access_claims_active(&claims)
            .await
            .expect_err("sid-less access cannot be revoked and must fail closed");
        assert!(matches!(error, Error::Unauthorized(message) if message.contains("session id")));
    }
}
