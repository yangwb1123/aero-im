//! TOTP (Time-based One-Time Password, RFC 6238) crypto primitives.
//!
//! Authenticator-app two-factor authentication. A shared secret is minted once
//! ([`generate_secret`]), handed to the user's app as a base32 string (and an
//! `otpauth://` URI), and thereafter both sides derive the same rolling 6-digit
//! code from the secret and the current 30-second time-step
//! (`HMAC-SHA1(secret, floor(unix_secs / 30))` with RFC 4226 dynamic
//! truncation). [`verify`] accepts the current step plus ±1 step of clock skew.
//!
//! Pure functions over a secret + a unix timestamp — no I/O, no state — so the
//! whole module is exercised by offline unit tests (including the canonical
//! RFC 6238 SHA-1 vector). Base32 (RFC 4648, no padding) is hand-rolled here so
//! the crate takes no extra dependency for it.

use hmac::{Hmac, Mac};
use rand::{rngs::OsRng, RngCore};
use sha1::Sha1;

type HmacSha1 = Hmac<Sha1>;

/// RFC 4648 base32 alphabet (no padding).
const BASE32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// The TOTP time-step in seconds (RFC 6238 default `X = 30`).
const STEP_SECS: u64 = 30;

/// Number of digits in a generated code (RFC 6238 default).
const DIGITS: u32 = 6;

/// Encode bytes as RFC 4648 base32 *without* padding.
fn base32_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(5) * 8);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for &byte in data {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let idx = ((buffer >> bits) & 0x1f) as usize;
            out.push(BASE32_ALPHABET[idx] as char);
        }
    }
    if bits > 0 {
        let idx = ((buffer << (5 - bits)) & 0x1f) as usize;
        out.push(BASE32_ALPHABET[idx] as char);
    }
    out
}

/// Decode an RFC 4648 base32 string (case-insensitive; padding/whitespace
/// ignored). Returns `None` on any character outside the base32 alphabet.
fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 5 / 8 + 1);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for c in s.chars() {
        if c == '=' || c.is_whitespace() {
            continue;
        }
        let uc = c.to_ascii_uppercase();
        let val: u8 = match uc {
            'A'..='Z' => uc as u8 - b'A',
            '2'..='7' => uc as u8 - b'2' + 26,
            _ => return None,
        };
        buffer = (buffer << 5) | u32::from(val);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

/// HMAC-based one-time password (RFC 4226): `HMAC-SHA1(key, counter)` reduced to
/// a zero-padded [`DIGITS`]-digit string via dynamic truncation.
fn hotp(key: &[u8], counter: u64) -> String {
    let mut mac = HmacSha1::new_from_slice(key).expect("HMAC-SHA1 accepts any key length");
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    // Dynamic truncation (RFC 4226 §5.3): low nibble of the last byte selects a
    // 4-byte window, whose high bit is masked off to stay positive.
    let offset = (digest[19] & 0x0f) as usize;
    let bin = (u32::from(digest[offset] & 0x7f) << 24)
        | (u32::from(digest[offset + 1]) << 16)
        | (u32::from(digest[offset + 2]) << 8)
        | u32::from(digest[offset + 3]);
    let modulo = 10_u32.pow(DIGITS);
    let code = bin % modulo;
    format!("{code:0width$}", width = DIGITS as usize)
}

/// Length-independent, branch-free byte comparison (avoids leaking how many
/// leading digits matched via timing). Returns `false` immediately on a length
/// mismatch — only the contents are compared in constant time.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Generate a fresh TOTP shared secret: 20 cryptographically-random bytes (the
/// SHA-1 block size) encoded as an unpadded base32 string suitable for an
/// authenticator app and an `otpauth://` URI.
///
/// Uses the OS CSPRNG ([`OsRng`]) — never a seeded/deterministic generator.
#[must_use]
pub fn generate_secret() -> String {
    let mut bytes = [0u8; 20];
    let mut rng = OsRng;
    rng.fill_bytes(&mut bytes);
    base32_encode(&bytes)
}

/// The current TOTP code for `secret_b32` at `unix_secs` (RFC 6238), as a
/// zero-padded 6-digit string. Returns `None` if `secret_b32` is not valid
/// base32.
#[must_use]
pub fn current_code(secret_b32: &str, unix_secs: u64) -> Option<String> {
    let key = base32_decode(secret_b32)?;
    Some(hotp(&key, unix_secs / STEP_SECS))
}

/// Verify `code` against `secret_b32` at `unix_secs`, accepting the current
/// time-step **and** ±1 step of clock skew (RFC 6238 §5.2). The 6-digit strings
/// are compared in (near) constant time. Returns `false` for a malformed code,
/// an unparseable secret, or no match in the accepted window.
#[must_use]
pub fn verify(secret_b32: &str, code: &str, unix_secs: u64) -> bool {
    let code = code.trim();
    if code.len() != DIGITS as usize || !code.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let Some(key) = base32_decode(secret_b32) else {
        return false;
    };
    let counter = unix_secs / STEP_SECS;
    // Current step plus ±1; `checked_*` drops out-of-range counters near 0/MAX
    // without a panicking cast. Accumulate with `|` (not `||`) so all candidates
    // are evaluated regardless of an early match.
    let candidates = [
        counter.checked_sub(1),
        Some(counter),
        counter.checked_add(1),
    ];
    let mut matched = false;
    for candidate in candidates.into_iter().flatten() {
        matched |= ct_eq(hotp(&key, candidate).as_bytes(), code.as_bytes());
    }
    matched
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_round_trips() {
        for sample in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar"] {
            let encoded = base32_encode(sample);
            assert_eq!(base32_decode(&encoded).as_deref(), Some(sample));
        }
    }

    #[test]
    fn base32_decode_rejects_non_alphabet() {
        // '0', '1', '8', '9' are outside the base32 alphabet.
        assert!(base32_decode("0189").is_none());
    }

    #[test]
    fn current_code_matches_rfc6238_sha1_vector() {
        // RFC 6238 Appendix B: shared secret "12345678901234567890" (ASCII),
        // SHA-1, at T=59s (time-step counter 1) the 8-digit TOTP is 94287082,
        // so the 6-digit code is its low six digits: 287082.
        let secret = base32_encode(b"12345678901234567890");
        assert_eq!(current_code(&secret, 59).as_deref(), Some("287082"));
    }

    #[test]
    fn current_code_none_for_bad_secret() {
        assert!(current_code("0189", 59).is_none());
    }

    #[test]
    fn verify_accepts_current_and_one_step_skew() {
        let secret = base32_encode(b"12345678901234567890");
        let code = current_code(&secret, 59).expect("valid secret"); // step counter 1
                                                                     // Same step, and one step either side (±30s) all accept the same code.
        assert!(verify(&secret, &code, 59), "current step");
        assert!(verify(&secret, &code, 59 + STEP_SECS), "+1 step skew");
        assert!(verify(&secret, &code, 59 - STEP_SECS), "-1 step skew");
    }

    #[test]
    fn verify_rejects_far_skew_and_wrong_code() {
        let secret = base32_encode(b"12345678901234567890");
        let code = current_code(&secret, 59).expect("valid secret");
        // Four steps later (+120s) is outside the ±1-step window.
        assert!(
            !verify(&secret, &code, 59 + 120),
            "far-future step rejected"
        );
        // A code that is not the value at any accepted step is rejected.
        let wrong = if code == "000000" { "111111" } else { "000000" };
        assert!(!verify(&secret, wrong, 59), "wrong code rejected");
        // Malformed codes (wrong length / non-digit) are rejected.
        assert!(!verify(&secret, "12345", 59), "too short");
        assert!(!verify(&secret, "abcdef", 59), "non-digit");
    }

    #[test]
    fn generate_secret_is_decodable_and_fresh() {
        let a = generate_secret();
        let b = generate_secret();
        assert_ne!(a, b, "OS CSPRNG yields distinct secrets");
        // 20 bytes → 32 base32 chars (no padding).
        assert_eq!(a.len(), 32);
        assert_eq!(base32_decode(&a).map(|d| d.len()), Some(20));
    }
}
