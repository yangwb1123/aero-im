//! Authentication: password hashing (Argon2id), JWT issuance/verification (RS256),
//! and an Axum extractor that resolves the authenticated `ParticipantId`.
//!
//! Library boundary:
//! * [`service::AuthService`] is the high-level API used by the HTTP layer
//!   (`register`, `login`, `refresh`, `verify`).
//! * [`extractor::AuthUser`] is an Axum `FromRequestParts` extractor for handlers
//!   that need the authenticated participant.
//! * The lower-level [`password`] and [`jwt`] modules are exposed for tests and
//!   for callers that need fine-grained control.

pub mod audit_tokens;
pub mod bot;
pub mod extractor;
pub mod jwt;
pub mod login_throttle;
pub mod oidc;
pub mod password;
pub mod password_policy;
pub mod pat;
pub mod service;
pub mod totp;

#[cfg(test)]
mod db_tests;

pub use bot::{BotTokenVerifier, SharedBotVerifier};
pub use extractor::AuthUser;
pub use jwt::{Claims, JwtCodec, TokenKind};
pub use login_throttle::{LockoutConfig, LoginThrottle};
pub use oidc::{
    validate_client_credentials_token, validate_id_token, validate_jwks_uri,
    ClientCredentialsClaims, ClientCredentialsTokenConfig, JwksKeyProvider, JwksUriError,
    KeyProvider, OidcClaims, OidcConfig, OidcError, StaticKeyProvider,
};
pub use password_policy::PasswordPolicy;
pub use pat::{PatVerifier, SharedPatVerifier};
pub use service::{AuthService, AuthTokens, LoginRequest, RegisterRequest, RegisterResponse};
