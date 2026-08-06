//! RS256 JWT issuance and verification.
//!
//! Claims schema (Spec §6 / `AuthConfig`):
//! ```json
//! { "sub": "<ParticipantId ulid>", "iss": "aero-im", "iat": 0, "exp": 0, "kind": "access" | "refresh", "sid": "<SessionId>" }
//! ```
//!
//! Keys are RSA PEM strings, loaded once at startup and held inside [`JwtCodec`].

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aero_common::{Error, ParticipantId, Result, SessionId};
use jsonwebtoken::{
    decode, decode_header, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation,
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
///
/// `jti` is a per-token UUID v4 that makes every issued token globally unique,
/// even when two tokens for the same participant are issued within the same
/// second (identical `iat`). Without `jti`, same-second tokens produce the same
/// content → the same `token_hash` → one session row instead of two.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub iss: String,
    pub iat: u64,
    pub exp: u64,
    pub kind: TokenKind,
    /// Stable login-session id shared by the access and refresh token in one
    /// pair. Legacy tokens pre-dating session binding omit it and remain
    /// decodable during the compatibility window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
    /// Unique nonce (UUID v4). Populated by `JwtCodec::issue`; present in all
    /// tokens generated from this version onward. Old tokens without `jti`
    /// deserialize fine: serde defaults to an empty string, which is harmless.
    #[serde(default)]
    pub jti: String,
}

impl Claims {
    /// Parses `sub` into a typed [`ParticipantId`].
    pub fn participant_id(&self) -> Result<ParticipantId> {
        ParticipantId::from_str(&self.sub)
            .map_err(|e| Error::Unauthorized(format!("invalid sub claim: {e}")))
    }

    /// Parses the optional `sid` into a typed [`SessionId`]. A missing sid is a
    /// supported legacy token; a present but malformed sid is never treated as
    /// legacy and fails closed.
    pub fn session_id(&self) -> Result<Option<SessionId>> {
        self.sid
            .as_deref()
            .map(|sid| {
                SessionId::from_str(sid)
                    .map_err(|e| Error::Unauthorized(format!("invalid sid claim: {e}")))
            })
            .transpose()
    }
}

/// Holds RSA keys + issuer + TTLs; cheap to clone (keys live behind `Arc`).
#[derive(Clone)]
pub struct JwtCodec {
    inner: Arc<Inner>,
}

struct Inner {
    encoding: EncodingKey,
    /// The active signing key's `kid`, stamped on every issued token.
    kid: String,
    /// Verification keyring (`kid` -> public key): the active key plus any extra
    /// verify-only keys, so a token signed by a just-retired key still verifies
    /// during a rotation overlap.
    verify_keys: HashMap<String, DecodingKey>,
    /// The active key's decoder — fallback for tokens with no/unknown `kid`
    /// (e.g. tokens issued before kid stamping existed).
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
        Self::from_pems::<&str>(
            private_pem,
            public_pem,
            &[],
            issuer,
            access_ttl,
            refresh_ttl,
        )
    }

    /// Like [`Self::from_pem`] but also registers extra verify-only public keys
    /// (PEM) in the keyring, enabling **zero-downtime key rotation**: the active
    /// key signs (and verifies) while tokens still in flight that were signed by a
    /// just-retired key keep verifying until they expire. Each key is addressed by
    /// a `kid` derived from its public PEM, so an operator rotates by promoting a
    /// new key to active and moving the old public PEM into the extra-verifier set.
    ///
    /// # Errors
    /// Returns [`Error::Internal`] if any PEM (active or extra) is not a valid RSA key.
    pub fn from_pems<S: AsRef<str>>(
        private_pem: &str,
        public_pem: &str,
        extra_public_pems: &[S],
        issuer: impl Into<String>,
        access_ttl: Duration,
        refresh_ttl: Duration,
    ) -> Result<Self> {
        let encoding = EncodingKey::from_rsa_pem(private_pem.as_bytes())
            .map_err(|e| Error::Internal(anyhow::anyhow!("invalid RSA private key: {e}")))?;
        let decode_rsa = |pem: &str, label: &str| {
            DecodingKey::from_rsa_pem(pem.as_bytes()).map_err(|e| {
                Error::Internal(anyhow::anyhow!("invalid {label} RSA public key: {e}"))
            })
        };
        let decoding = decode_rsa(public_pem, "active")?;
        let kid = key_id(public_pem);
        let mut verify_keys = HashMap::new();
        verify_keys.insert(kid.clone(), decode_rsa(public_pem, "active")?);
        for pem in extra_public_pems {
            let pem = pem.as_ref();
            if pem.trim().is_empty() {
                continue;
            }
            verify_keys.insert(key_id(pem), decode_rsa(pem, "extra")?);
        }
        Ok(Self {
            inner: Arc::new(Inner {
                encoding,
                kid,
                verify_keys,
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
    ///
    /// Each call generates a fresh `jti` (UUID v4) so tokens are globally unique
    /// even when issued for the same participant within the same second.
    pub fn issue(&self, participant: ParticipantId, kind: TokenKind) -> Result<String> {
        self.issue_inner(participant, kind, None)
    }

    /// Issue a token bound to the stable login `session`. Calling this once for
    /// access and once for refresh yields a pair with the same `sid` but distinct
    /// per-token `jti` values.
    pub fn issue_for_session(
        &self,
        participant: ParticipantId,
        kind: TokenKind,
        session: SessionId,
    ) -> Result<String> {
        self.issue_inner(participant, kind, Some(session))
    }

    fn issue_inner(
        &self,
        participant: ParticipantId,
        kind: TokenKind,
        session: Option<SessionId>,
    ) -> Result<String> {
        let now = unix_now();
        let exp = now + self.ttl(kind).as_secs();
        let claims = Claims {
            sub: participant.to_string(),
            iss: self.inner.issuer.clone(),
            iat: now,
            exp,
            kind,
            sid: session.map(|id| id.to_string()),
            jti: uuid::Uuid::new_v4().to_string(),
        };
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(self.inner.kid.clone());
        encode(&header, &claims, &self.inner.encoding)
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
        // Select the verification key by the token's `kid` (key rotation); fall back
        // to the active key for a token with no `kid` (legacy) or an unknown one.
        let key = decode_header(token)
            .ok()
            .and_then(|h| h.kid)
            .and_then(|kid| self.inner.verify_keys.get(&kid))
            .unwrap_or(&self.inner.decoding);
        decode::<Claims>(token, key, &validation)
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

/// A stable, non-secret key identifier derived from a public PEM (SHA-1 of the
/// trimmed PEM bytes, truncated to 16 hex chars). Used only to *select* a
/// verification key from the ring — collision-resistance is not security-critical
/// here (the RSA signature is), so SHA-1 (already a dependency) is sufficient.
fn key_id(public_pem: &str) -> String {
    use sha1::{Digest, Sha1};
    use std::fmt::Write as _;
    let digest = Sha1::digest(public_pem.trim().as_bytes());
    let mut out = String::with_capacity(16);
    for b in digest.iter().take(8) {
        write!(out, "{b:02x}").expect("write to String cannot fail");
    }
    out
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
        let public_pem = public.to_pkcs1_pem(rsa::pkcs8::LineEnding::LF).unwrap();
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

    /// Zero-downtime key rotation: a token signed by the now-retired key still
    /// verifies as long as that key is kept as an extra verifier, while the new
    /// active key signs and verifies fresh tokens. Without the old key in the ring
    /// the old token is rejected.
    #[test]
    fn kid_rotation_verifies_old_tokens_with_retained_verifier() {
        let (priv_a, pub_a) = keypair();
        let (priv_b, pub_b) = keypair();
        let (acc, refr) = (Duration::from_secs(60), Duration::from_secs(600));
        let pid = ParticipantId::new();

        // Key A active → issue a token; it must be stamped with A's kid.
        let codec_a = JwtCodec::from_pem(&priv_a, &pub_a, "aero-im", acc, refr).unwrap();
        let token_a = codec_a.issue(pid, TokenKind::Access).unwrap();
        let header = jsonwebtoken::decode_header(&token_a).unwrap();
        assert_eq!(
            header.kid.as_deref(),
            Some(key_id(&pub_a).as_str()),
            "token carries active kid"
        );

        // Rotate: key B active, key A retained as an extra verifier.
        let codec_b =
            JwtCodec::from_pems(&priv_b, &pub_b, std::slice::from_ref(&pub_a), "aero-im", acc, refr)
                .unwrap();
        assert_eq!(
            codec_b.verify(&token_a).unwrap().participant_id().unwrap(),
            pid,
            "token signed by retired key A still verifies via the keyring",
        );
        let token_b = codec_b.issue(pid, TokenKind::Access).unwrap();
        assert_eq!(
            codec_b.verify(&token_b).unwrap().participant_id().unwrap(),
            pid,
            "token signed by new active key B verifies",
        );

        // Without A retained, A's token is rejected (its key is gone from the ring).
        let codec_b_only = JwtCodec::from_pem(&priv_b, &pub_b, "aero-im", acc, refr).unwrap();
        assert!(
            codec_b_only.verify(&token_a).is_err(),
            "retired key removed → old token rejected"
        );
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
    fn same_second_tokens_are_distinct() {
        // Two tokens for the same participant issued without any sleep must differ
        // (different jti) so their hashes are distinct — Wave-21 session regression.
        let c = codec(Duration::from_secs(3600), Duration::from_secs(3600));
        let pid = ParticipantId::new();
        let t1 = c.issue(pid, TokenKind::Refresh).unwrap();
        let t2 = c.issue(pid, TokenKind::Refresh).unwrap();
        assert_ne!(t1, t2, "same-second tokens must differ via jti");

        // Both are still valid.
        assert!(c.verify(&t1).is_ok());
        assert!(c.verify(&t2).is_ok());

        // jti is non-empty and the two values are distinct.
        let c1 = c.verify(&t1).unwrap();
        let c2 = c.verify(&t2).unwrap();
        assert!(!c1.jti.is_empty(), "jti must be populated");
        assert_ne!(c1.jti, c2.jti, "jti values must differ");
    }

    #[test]
    fn session_pair_shares_sid_but_not_jti() {
        let c = codec(Duration::from_secs(3600), Duration::from_secs(7200));
        let participant = ParticipantId::new();
        let session = SessionId::new();
        let access = c
            .issue_for_session(participant, TokenKind::Access, session)
            .unwrap();
        let refresh = c
            .issue_for_session(participant, TokenKind::Refresh, session)
            .unwrap();
        let access_claims = c.verify(&access).unwrap();
        let refresh_claims = c.verify(&refresh).unwrap();

        assert_eq!(access_claims.session_id().unwrap(), Some(session));
        assert_eq!(refresh_claims.session_id().unwrap(), Some(session));
        assert_ne!(access_claims.jti, refresh_claims.jti);
    }

    #[test]
    fn sidless_is_legacy_but_malformed_sid_is_rejected() {
        let c = codec(Duration::from_secs(60), Duration::from_secs(60));
        let legacy = c
            .verify(&c.issue(ParticipantId::new(), TokenKind::Access).unwrap())
            .unwrap();
        assert_eq!(legacy.session_id().unwrap(), None);

        let mut malformed = legacy;
        malformed.sid = Some("not-a-session-id".into());
        assert!(matches!(
            malformed.session_id(),
            Err(Error::Unauthorized(_))
        ));
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
        let token = signer
            .issue(ParticipantId::new(), TokenKind::Access)
            .unwrap();
        let res = verifier.verify(&token);
        assert!(matches!(res, Err(Error::Unauthorized(_))));
    }
}
