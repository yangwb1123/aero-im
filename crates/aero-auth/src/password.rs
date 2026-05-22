//! Password hashing using Argon2id with PHC-string encoding.
//!
//! Hashes are self-describing (`$argon2id$v=19$m=…,t=…,p=…$salt$hash`) so parameters
//! can evolve without a schema migration. Verification reads the parameters back
//! out of the stored hash.

use aero_common::{Error, Result};
use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use password_hash::{PasswordHash, SaltString};
use rand::rngs::OsRng;

/// Hashes `password` with Argon2id and a freshly-generated random salt.
///
/// Returns the PHC string suitable for storing in `credentials.password_hash`.
pub fn hash(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let argon = Argon2::default();
    let phc = argon
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| Error::Internal(anyhow::anyhow!("argon2 hash failed: {e}")))?;
    Ok(phc.to_string())
}

/// Verifies `password` against a previously stored PHC hash.
///
/// Returns `Ok(())` on match; `Err(Unauthorized)` on mismatch or malformed hash.
pub fn verify(password: &str, phc: &str) -> Result<()> {
    let parsed = PasswordHash::new(phc)
        .map_err(|e| Error::Internal(anyhow::anyhow!("malformed password hash: {e}")))?;
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .map_err(|_| Error::Unauthorized("invalid credentials".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify_roundtrip() {
        let h = hash("correct horse battery staple").unwrap();
        assert!(h.starts_with("$argon2id$"));
        verify("correct horse battery staple", &h).unwrap();
    }

    #[test]
    fn wrong_password_is_unauthorized() {
        let h = hash("hunter2").unwrap();
        let err = verify("hunter3", &h).unwrap_err();
        assert!(matches!(err, Error::Unauthorized(_)));
    }

    #[test]
    fn malformed_hash_is_internal() {
        let err = verify("x", "not-a-phc-string").unwrap_err();
        assert!(matches!(err, Error::Internal(_)));
    }

    #[test]
    fn two_hashes_of_same_password_differ() {
        // Different random salts produce different output.
        let a = hash("same").unwrap();
        let b = hash("same").unwrap();
        assert_ne!(a, b);
    }
}
