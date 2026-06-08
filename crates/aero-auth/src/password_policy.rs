//! Configurable password-strength policy (ROADMAP 方向三 hardening).
//!
//! Replaces the previously-hardcoded `len < 8` check with a policy that an
//! operator can tighten via environment without a code change. The **default**
//! preserves the historical behaviour exactly (min 8, max 1024, no character-
//! class requirement) so nothing already in the field breaks; deployments that
//! want a stronger floor opt in:
//!
//! * `AERO_PASSWORD_MIN_LEN`           — minimum length (default 8)
//! * `AERO_PASSWORD_MAX_LEN`           — maximum length (default 1024)
//! * `AERO_PASSWORD_REQUIRED_CLASSES`  — how many of {lowercase, uppercase,
//!   digit, symbol} a password must include, 0–4 (default 0 = no requirement)
//!
//! NB participants are *global* (a participant can belong to many workspaces),
//! so the policy is process-global rather than per-workspace — a per-workspace
//! policy can't bind a single global credential. Per-participant *reuse history*
//! (the companion control) lives in `aero_storage::PasswordHistoryRepo`.

use aero_common::{Error, Result};

/// A password-strength policy. [`PasswordPolicy::default`] is the historical
/// behaviour; [`PasswordPolicy::from_env`] applies operator overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasswordPolicy {
    pub min_len: usize,
    pub max_len: usize,
    /// Required distinct character classes (0–4): lowercase, uppercase, digit, symbol.
    pub required_classes: u8,
}

impl Default for PasswordPolicy {
    fn default() -> Self {
        Self { min_len: 8, max_len: 1024, required_classes: 0 }
    }
}

impl PasswordPolicy {
    /// Build from env, falling back to [`Default`] for any unset/garbage var.
    #[must_use]
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            min_len: env_parse("AERO_PASSWORD_MIN_LEN").unwrap_or(d.min_len),
            max_len: env_parse("AERO_PASSWORD_MAX_LEN").unwrap_or(d.max_len),
            required_classes: env_parse::<u8>("AERO_PASSWORD_REQUIRED_CLASSES")
                .unwrap_or(d.required_classes)
                .min(4),
        }
    }

    /// Validate `password` against this policy. Counts by `char`, not bytes, so a
    /// multibyte password is measured by user-perceived length.
    ///
    /// # Errors
    /// [`Error::Invalid`] when too short, too long, or lacking the required
    /// character-class diversity.
    pub fn validate(&self, password: &str) -> Result<()> {
        let len = password.chars().count();
        if len < self.min_len {
            return Err(Error::Invalid(format!(
                "password must be at least {} characters",
                self.min_len
            )));
        }
        if len > self.max_len {
            return Err(Error::Invalid("password is too long".into()));
        }
        if self.required_classes > 0 && char_classes(password) < self.required_classes {
            return Err(Error::Invalid(format!(
                "password must include at least {} of: lowercase, uppercase, digit, symbol",
                self.required_classes
            )));
        }
        Ok(())
    }
}

/// How many of the four character classes appear in `password`.
#[must_use]
fn char_classes(password: &str) -> u8 {
    let (mut lower, mut upper, mut digit, mut symbol) = (false, false, false, false);
    for c in password.chars() {
        if c.is_ascii_lowercase() {
            lower = true;
        } else if c.is_ascii_uppercase() {
            upper = true;
        } else if c.is_ascii_digit() {
            digit = true;
        } else {
            symbol = true;
        }
    }
    u8::from(lower) + u8::from(upper) + u8::from(digit) + u8::from(symbol)
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok()?.trim().parse().ok()
}

/// Validate `password` against the env-configured [`PasswordPolicy`]. The single
/// entry point used by register / change-password / reset-password.
///
/// # Errors
/// Propagates [`PasswordPolicy::validate`].
pub fn validate(password: &str) -> Result<()> {
    PasswordPolicy::from_env().validate(password)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_matches_legacy_behaviour() {
        let p = PasswordPolicy::default();
        assert!(p.validate("short").is_err(), "<8 chars rejected");
        assert!(p.validate("longenough").is_ok(), "8+ chars, single class, accepted by default");
        assert!(p.validate(&"x".repeat(1025)).is_err(), ">1024 rejected");
    }

    #[test]
    fn char_classes_counts_distinct_kinds() {
        assert_eq!(char_classes("abcdef"), 1);
        assert_eq!(char_classes("abcDEF"), 2);
        assert_eq!(char_classes("abcDEF1"), 3);
        assert_eq!(char_classes("abcDEF1!"), 4);
        assert_eq!(char_classes("12345678"), 1);
    }

    #[test]
    fn required_classes_gate() {
        let strict = PasswordPolicy { min_len: 8, max_len: 1024, required_classes: 3 };
        assert!(strict.validate("longenough").is_err(), "single class fails a 3-class policy");
        assert!(strict.validate("password_123").is_ok(), "lower+digit+symbol = 3 classes");
        assert!(strict.validate("pw-Aa123456!").is_ok(), "4 classes");
    }

    #[test]
    fn from_env_default_when_unset() {
        // With the env vars unset (the common case), from_env == default.
        // (Tests run without AERO_PASSWORD_* set.)
        assert_eq!(PasswordPolicy::from_env(), PasswordPolicy::default());
    }
}
