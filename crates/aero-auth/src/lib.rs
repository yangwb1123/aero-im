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

pub mod extractor;
pub mod jwt;
pub mod oidc;
pub mod password;
pub mod service;

pub use extractor::AuthUser;
pub use jwt::{Claims, JwtCodec, TokenKind};
pub use oidc::{
    validate_id_token, JwksKeyProvider, KeyProvider, OidcClaims, OidcConfig, OidcError,
    StaticKeyProvider,
};
pub use service::{AuthService, AuthTokens, LoginRequest, RegisterRequest, RegisterResponse};
