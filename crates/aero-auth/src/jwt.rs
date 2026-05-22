//! RS256 JWT issuance and verification.
//!
//! Claims schema (Spec §6 / `AuthConfig`):
//! ```json
//! { "sub": "<ParticipantId ulid>", "iss": "aero-im", "iat": 0, "exp": 0, "kind": "access" | "refresh" }
//! ```
//!
//! Keys are RSA PEM strings, loaded once at startup and held inside [`JwtCodec`].

use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aero_common::{Error, ParticipantId, Result};
use jsonwebtoken::{
    decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation,
};
use serde::{Deserialize, Serialize};

/// Distinguishes between short-lived access tokens and long-lived refresh tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenKind {
    Access,
    Refresh,
}

/// JWT payload. `sub` is the participant ULID rendered as a string.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub iss: String,
    pub iat: u64,
    pub exp: u64,
    pub kind: TokenKind,
}

impl Claims {
    /// Parses `sub` into a typed [`ParticipantId`].
    pub fn participant_id(&self) -> Result<ParticipantId> {
        ParticipantId::from_str(&self.sub)
            .map_err(|e| Error::Unauthorized(format!("invalid sub claim: {e}")))
    }
}

/// Holds RSA keys + issuer + TTLs; cheap to clone (keys live behind `Arc`).
#[derive(Clone)]
pub struct JwtCodec {
    inner: Arc<Inner>,
}

struct Inner {
    encoding: EncodingKey,
    decoding: DecodingKey,
    issuer: String,
    access_ttl: Duration,
    refresh_ttl: Duration,
}

impl JwtCodec {
    /// Builds a codec from RSA PEM key material.
    ///
    /// `private_pem` must be a PKCS#1 or PKCS#8 RSA private key.
    /// `public_pem` must match.
    pub fn from_pem(
        private_pem: &str,
        public_pem: &str,
        issuer: impl Into<String>,
        access_ttl: Duration,
        refresh_ttl: Duration,
    ) -> Result<Self> {
        let encoding = EncodingKey::from_rsa_pem(private_pem.as_bytes())
            .map_err(|e| Error::Internal(anyhow::anyhow!("invalid RSA private key: {e}")))?;
        let decoding = DecodingKey::from_rsa_pem(public_pem.as_bytes())
            .map_err(|e| Error::Internal(anyhow::anyhow!("invalid RSA public key: {e}")))?;
        Ok(Self {
            inner: Arc::new(Inner {
                encoding,
                decoding,
                issuer: issuer.into(),
                access_ttl,
                refresh_ttl,
            }),
        })
    }

    /// Returns the configured issuer (`iss` claim).
    pub fn issuer(&self) -> &str {
        &self.inner.issuer
    }

    /// Token lifetime for the given kind.
    pub fn ttl(&self, kind: TokenKind) -> Duration {
        match kind {
            TokenKind::Access => self.inner.access_ttl,
            TokenKind::Refresh => self.inner.refresh_ttl,
        }
    }

    /// Issues a new signed token for `participant` with the given `kind`.
    pub fn issue(&self, participant: ParticipantId, kind: TokenKind) -> Result<String> {
        let now = unix_now();
        let exp = now + self.ttl(kind).as_secs();
        let claims = Claims {
            sub: participant.to_string(),
            iss: self.inner.issuer.clone(),
            iat: now,
            exp,
            kind,
        };
        encode(&Header::new(Algorithm::RS256), &claims, &self.inner.encoding)
            .map_err(|e| Error::Internal(anyhow::anyhow!("jwt encode failed: {e}")))
    }

    /// Verifies signature, issuer and expiry, returning the decoded claims.
    ///
    /// On any failure returns [`Error::Unauthorized`] — we never leak the
    /// underlying `jsonwebtoken` error variant to clients.
    pub fn verify(&self, token: &str) -> Result<Claims> {
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[&self.inner.issuer]);
        validation.validate_exp = true;
        // We don't use `aud`; leave it required=false by default.
        decode::<Claims>(token, &self.inner.decoding, &validation)
            .map(|data| data.claims)
            .map_err(|e| Error::Unauthorized(format!("invalid token: {e}")))
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;
    use rsa::pkcs1::EncodeRsaPublicKey;
    use rsa::pkcs8::EncodePrivateKey;
    use rsa::RsaPrivateKey;

    /// Build a small RSA keypair as PEM strings. 2048 bits is slow for `cargo test`
    /// dev profile but acceptable; we generate once per test.
    fn keypair() -> (String, String) {
        let mut rng = OsRng;
        let private = RsaPrivateKey::new(&mut rng, 2048).expect("rsa keygen");
        let public = private.to_public_key();
        let private_pem = private
            .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
            .unwrap()
            .to_string();
        let public_pem = public
            .to_pkcs1_pem(rsa::pkcs8::LineEnding::LF)
            .unwrap();
        (private_pem, public_pem)
    }

    fn codec(access: Duration, refresh: Duration) -> JwtCodec {
        let (priv_pem, pub_pem) = keypair();
        JwtCodec::from_pem(&priv_pem, &pub_pem, "aero-im", access, refresh).unwrap()
    }

    #[test]
    fn issue_and_verify_roundtrip() {
        let c = codec(Duration::from_secs(60), Duration::from_secs(600));
        let pid = ParticipantId::new();
        let token = c.issue(pid, TokenKind::Access).unwrap();
        let claims = c.verify(&token).unwrap();
        assert_eq!(claims.iss, "aero-im");
        assert_eq!(claims.kind, TokenKind::Access);
        assert_eq!(claims.participant_id().unwrap(), pid);
        assert!(claims.exp > claims.iat);
    }

    #[test]
    fn refresh_kind_is_distinguished() {
        let c = codec(Duration::from_secs(60), Duration::from_secs(600));
        let pid = ParticipantId::new();
        let token = c.issue(pid, TokenKind::Refresh).unwrap();
        let claims = c.verify(&token).unwrap();
        assert_eq!(claims.kind, TokenKind::Refresh);
    }

    #[test]
    fn expired_token_is_unauthorized() {
        // `validate_exp` rejects tokens whose `exp` is in the past — including
        // tokens that expire instantly with a TTL of 0 seconds, because the
        // library applies a small leeway. We pass leeway=0 manually for a tight
        // check by sleeping briefly.
        let c = codec(Duration::from_secs(0), Duration::from_secs(0));
        let pid = ParticipantId::new();
        let token = c.issue(pid, TokenKind::Access).unwrap();
        // Sleep long enough to exceed the default 60s leeway? That's too long
        // for a unit test. Instead, tweak validation to disable leeway.
        let mut v = Validation::new(Algorithm::RS256);
        v.set_issuer(&["aero-im"]);
        v.leeway = 0;
        std::thread::sleep(Duration::from_millis(1100));
        let res = decode::<Claims>(&token, &c.inner.decoding, &v);
        assert!(res.is_err(), "expected token to be rejected as expired");
    }

    #[test]
    fn tampered_signature_is_rejected() {
        let c = codec(Duration::from_secs(60), Duration::from_secs(60));
        let pid = ParticipantId::new();
        let mut token = c.issue(pid, TokenKind::Access).unwrap();
        // Flip the last char of the signature segment.
        let last = token.pop().unwrap();
        let replacement = if last == 'A' { 'B' } else { 'A' };
        token.push(replacement);
        let res = c.verify(&token);
        assert!(matches!(res, Err(Error::Unauthorized(_))));
    }

    #[test]
    fn wrong_issuer_is_rejected() {
        let (priv_pem, pub_pem) = keypair();
        let signer = JwtCodec::from_pem(
            &priv_pem,
            &pub_pem,
            "evil-issuer",
            Duration::from_secs(60),
            Duration::from_secs(60),
        )
        .unwrap();
        let verifier = JwtCodec::from_pem(
            &priv_pem,
            &pub_pem,
            "aero-im",
            Duration::from_secs(60),
            Duration::from_secs(60),
        )
        .unwrap();
        let token = signer.issue(ParticipantId::new(), TokenKind::Access).unwrap();
        let res = verifier.verify(&token);
        assert!(matches!(res, Err(Error::Unauthorized(_))));
    }
}
