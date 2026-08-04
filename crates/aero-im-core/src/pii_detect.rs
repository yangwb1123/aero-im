//! PII detection on the message send path (ROADMAP5 方向五).
//!
//! The content [`Moderator`](crate::moderator::Moderator) classifies banned
//! WORDS; it does not catch personally identifiable information a user pastes
//! into a message — an SSN, a credit-card number, an email, a phone number —
//! which would then silently flow into the FTS index, AI embeddings, and
//! exports, potentially across tenants. This pure, dependency-free scanner
//! fingerprints the common high-signal PII shapes so the send path can block (or
//! flag) them. Hand-rolled with no `regex` dependency, matching the codebase
//! style (see [`crate::spam_guard`] and `aero-server`'s `content_sniff`).
//!
//! The matchers favour precision over recall — they fingerprint *structurally
//! plausible* PII (a Luhn-valid 13–19 digit run, an `###-##-####` SSN with a
//! valid area, a `local@domain.tld` email) rather than every conceivable form,
//! so a chat full of order numbers and version strings does not trip the guard.

// The matchers are hand-rolled byte scanners; `bytes`/`len`/index locals (b/n/i/j)
// are the idiomatic spelling for this kind of code (cf. `content_sniff`).
#![allow(clippy::many_single_char_names)]

/// A class of personally identifiable information detected in message text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PiiKind {
    /// US Social Security Number (`###-##-####`, structurally valid area/group).
    Ssn,
    /// A 13–19 digit run that passes the Luhn checksum (credit / debit card).
    CreditCard,
    /// An email address (`local@domain.tld`).
    Email,
    /// A phone number in a plausible NANP grouping (off by default — noisiest).
    Phone,
}

impl PiiKind {
    /// Stable lowercase tag for logging / audit detail.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            PiiKind::Ssn => "ssn",
            PiiKind::CreditCard => "credit_card",
            PiiKind::Email => "email",
            PiiKind::Phone => "phone",
        }
    }
}

/// Which PII classes to scan for. `phone` defaults OFF: NANP grouping overlaps
/// with many benign number formats, so it is opt-in to keep false positives low.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PiiConfig {
    pub ssn: bool,
    pub credit_card: bool,
    pub email: bool,
    pub phone: bool,
}

impl Default for PiiConfig {
    fn default() -> Self {
        Self {
            ssn: true,
            credit_card: true,
            email: true,
            phone: false,
        }
    }
}

/// Detect which [`PiiKind`]s appear in `text`. Pure and allocation-light; returns
/// each matched kind at most once, in [`PiiKind`] order. Empty ⇒ no PII found.
#[must_use]
pub fn detect(text: &str, cfg: &PiiConfig) -> Vec<PiiKind> {
    let mut found = Vec::new();
    if cfg.ssn && contains_ssn(text) {
        found.push(PiiKind::Ssn);
    }
    if cfg.credit_card && contains_credit_card(text) {
        found.push(PiiKind::CreditCard);
    }
    if cfg.email && contains_email(text) {
        found.push(PiiKind::Email);
    }
    if cfg.phone && contains_phone(text) {
        found.push(PiiKind::Phone);
    }
    found
}

// ---------------------------------------------------------------- SSN

/// `###-##-####` (or space-separated) with a structurally valid area (not 000,
/// 666, or 900–999), group (not 00) and serial (not 0000), not embedded in a
/// longer digit run.
fn contains_ssn(text: &str) -> bool {
    let b = text.as_bytes();
    let n = b.len();
    let mut i = 0;
    while i + 11 <= n {
        // Boundary before: the area's first digit must not extend a digit run.
        if i > 0 && b[i - 1].is_ascii_digit() {
            i += 1;
            continue;
        }
        if let Some(end) = match_ssn_at(b, i) {
            // Boundary after: must not run straight into more digits.
            if end >= n || !b[end].is_ascii_digit() {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Try to match an SSN starting exactly at `i`. Returns the end index (exclusive)
/// on success. The two separators must be identical and one of `-` or space.
fn match_ssn_at(b: &[u8], i: usize) -> Option<usize> {
    // area(3) sep group(2) sep serial(4)
    if i + 11 > b.len() {
        return None;
    }
    let d = |x: u8| x.is_ascii_digit();
    if !(d(b[i]) && d(b[i + 1]) && d(b[i + 2])) {
        return None;
    }
    let sep = b[i + 3];
    if sep != b'-' && sep != b' ' {
        return None;
    }
    if !(d(b[i + 4]) && d(b[i + 5])) || b[i + 6] != sep {
        return None;
    }
    if !(d(b[i + 7]) && d(b[i + 8]) && d(b[i + 9]) && d(b[i + 10])) {
        return None;
    }
    let area =
        u16::from(b[i] - b'0') * 100 + u16::from(b[i + 1] - b'0') * 10 + u16::from(b[i + 2] - b'0');
    let group = (b[i + 4] - b'0') * 10 + (b[i + 5] - b'0');
    let serial = &b[i + 7..i + 11];
    // Invalid area codes per SSA: 000, 666, 900-999. Group 00 and serial 0000
    // are never issued.
    if area == 0 || area == 666 || area >= 900 || group == 0 || serial.iter().all(|&x| x == b'0') {
        return None;
    }
    Some(i + 11)
}

// ---------------------------------------------------------------- Credit card

/// A maximal `digit (sep? digit)*` run of 13–19 digits (separators `-`/space)
/// that passes the Luhn checksum.
fn contains_credit_card(text: &str) -> bool {
    let b = text.as_bytes();
    let n = b.len();
    let mut i = 0;
    while i < n {
        // Start a candidate only at a digit not preceded by another digit — a
        // leading separator (space/dash) is fine, but a preceding digit means
        // we're mid-number and already scanning it.
        if b[i].is_ascii_digit() && (i == 0 || !b[i - 1].is_ascii_digit()) {
            let mut j = i;
            let mut digits: Vec<u8> = Vec::new();
            while j < n {
                let c = b[j];
                if c.is_ascii_digit() {
                    digits.push(c - b'0');
                    j += 1;
                } else if (c == b' ' || c == b'-')
                    && !digits.is_empty()
                    && j + 1 < n
                    && b[j + 1].is_ascii_digit()
                {
                    j += 1; // separator inside the candidate number
                } else {
                    break;
                }
                if digits.len() > 19 {
                    break; // too long to be a card; stop collecting
                }
            }
            if (13..=19).contains(&digits.len()) && luhn_ok(&digits) {
                return true;
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    false
}

/// Luhn (mod-10) checksum over individual decimal digits.
fn luhn_ok(digits: &[u8]) -> bool {
    let mut sum = 0u32;
    let mut double = false;
    for &d in digits.iter().rev() {
        let mut x = u32::from(d);
        if double {
            x *= 2;
            if x > 9 {
                x -= 9;
            }
        }
        sum += x;
        double = !double;
    }
    sum % 10 == 0
}

// ---------------------------------------------------------------- Email

/// `local@domain.tld` with a ≥2-letter alphabetic TLD.
fn contains_email(text: &str) -> bool {
    let b = text.as_bytes();
    let n = b.len();
    for (idx, &c) in b.iter().enumerate() {
        if c != b'@' {
            continue;
        }
        // Local part: at least one valid char immediately before the `@`.
        if idx == 0 || !is_local_byte(b[idx - 1]) {
            continue;
        }
        // Domain: labels of [alnum-.] ending in `.tld`, tld ≥2 alpha.
        let dom_start = idx + 1;
        let mut j = dom_start;
        let mut last_dot: Option<usize> = None;
        while j < n && is_domain_byte(b[j]) {
            if b[j] == b'.' {
                last_dot = Some(j);
            }
            j += 1;
        }
        if let Some(dot) = last_dot {
            let tld = &b[dot + 1..j];
            if dot > dom_start && tld.len() >= 2 && tld.iter().all(u8::is_ascii_alphabetic) {
                return true;
            }
        }
    }
    false
}

fn is_local_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'%' | b'+' | b'-')
}

fn is_domain_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'.' || b == b'-'
}

// ---------------------------------------------------------------- Phone

/// A NANP-plausible phone number: optional `+1`/`1` country code, then 10 digits
/// grouped with separators (`-`, `.`, space, parentheses). Requires at least one
/// separator so a bare 10-digit id is not mistaken for a phone number.
fn contains_phone(text: &str) -> bool {
    let b = text.as_bytes();
    let n = b.len();
    let mut i = 0;
    while i < n {
        if !is_phone_start(b, i) {
            i += 1;
            continue;
        }
        let mut j = i;
        let mut digits = 0usize;
        let mut seps = 0usize;
        let mut leading_one = false;
        // optional + and a leading country-code 1
        if b[j] == b'+' {
            j += 1;
        }
        while j < n {
            let c = b[j];
            if c.is_ascii_digit() {
                if digits == 0 && c == b'1' {
                    leading_one = true;
                }
                digits += 1;
                j += 1;
            } else if matches!(c, b' ' | b'-' | b'.' | b'(' | b')') {
                seps += 1;
                j += 1;
            } else {
                break;
            }
            if digits > 12 {
                break;
            }
        }
        let core = if leading_one {
            digits.saturating_sub(1)
        } else {
            digits
        };
        if core == 10 && seps >= 1 {
            return true;
        }
        i = (i + 1).max(j);
    }
    false
}

fn is_phone_start(b: &[u8], i: usize) -> bool {
    let here = b[i];
    let starts = here == b'+' || here == b'(' || here.is_ascii_digit();
    if !starts {
        return false;
    }
    // Boundary: don't start mid-number.
    i == 0 || !b[i - 1].is_ascii_digit()
}

// ---------------------------------------------------------------- Gated wrapper

/// Process-wide PII detector. Stateless (no per-sender history), so it is just a
/// config holder; the send path calls [`PiiDetector::scan`] and decides whether
/// to block or flag.
#[derive(Debug, Clone)]
pub struct PiiDetector {
    cfg: PiiConfig,
}

impl PiiDetector {
    #[must_use]
    pub fn new(cfg: PiiConfig) -> Self {
        Self { cfg }
    }

    /// Build from env, or `None` when the guard is off. Enabled by
    /// `AERO_PII_GUARD=1|true`. `AERO_PII_GUARD_PHONE=1|true` additionally turns
    /// on the (noisier, off-by-default) phone matcher.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let truthy = |v: String| v == "1" || v.eq_ignore_ascii_case("true");
        let on = std::env::var("AERO_PII_GUARD").map(truthy).unwrap_or(false);
        if !on {
            return None;
        }
        let mut cfg = PiiConfig::default();
        if let Ok(v) = std::env::var("AERO_PII_GUARD_PHONE") {
            cfg.phone = truthy(v);
        }
        Some(Self::new(cfg))
    }

    /// Scan `text` for PII, returning the kinds found (empty ⇒ clean).
    #[must_use]
    pub fn scan(&self, text: &str) -> Vec<PiiKind> {
        detect(text, &self.cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: PiiConfig = PiiConfig {
        ssn: true,
        credit_card: true,
        email: true,
        phone: true,
    };

    #[test]
    fn luhn_matches_known_test_cards() {
        // Classic Luhn-valid test PANs.
        for pan in [
            "4111111111111111",
            "5500005555555559",
            "340000000000009",
            "6011000000000004",
        ] {
            let digits: Vec<u8> = pan.bytes().map(|b| b - b'0').collect();
            assert!(luhn_ok(&digits), "{pan} should be Luhn-valid");
        }
        // A transposed digit breaks the checksum.
        let bad: Vec<u8> = "4111111111111121".bytes().map(|b| b - b'0').collect();
        assert!(!luhn_ok(&bad), "transposed digit should fail Luhn");
    }

    #[test]
    fn detects_credit_card_with_and_without_separators() {
        assert_eq!(
            detect("pay to 4111111111111111 now", &ALL),
            vec![PiiKind::CreditCard]
        );
        assert_eq!(
            detect("card 4111 1111 1111 1111", &ALL),
            vec![PiiKind::CreditCard]
        );
        assert_eq!(
            detect("card 4111-1111-1111-1111", &ALL),
            vec![PiiKind::CreditCard]
        );
    }

    #[test]
    fn does_not_flag_non_luhn_or_short_digit_runs() {
        // 16 digits but not Luhn-valid.
        assert!(detect("id 1234567890123456", &ALL).is_empty());
        // Order/version numbers — too short to be a card.
        assert!(detect("order 123456 v1.2.3", &ALL).is_empty());
        // A long but non-card numeric (Luhn-invalid).
        assert!(detect("12345678901234567890", &ALL).is_empty());
    }

    #[test]
    fn detects_valid_ssn_shapes() {
        assert_eq!(detect("ssn 123-45-6789", &ALL), vec![PiiKind::Ssn]);
        assert_eq!(detect("123 45 6789", &ALL), vec![PiiKind::Ssn]);
    }

    #[test]
    fn rejects_structurally_invalid_or_embedded_ssn() {
        // Invalid areas / group / serial.
        assert!(detect("000-12-3456", &ALL).is_empty(), "area 000");
        assert!(detect("666-12-3456", &ALL).is_empty(), "area 666");
        assert!(detect("900-12-3456", &ALL).is_empty(), "area >= 900");
        assert!(detect("123-00-6789", &ALL).is_empty(), "group 00");
        assert!(detect("123-45-0000", &ALL).is_empty(), "serial 0000");
        // Mixed separators must not match.
        assert!(detect("123-45 6789", &ALL).is_empty(), "separators differ");
        // Embedded in a longer digit run.
        assert!(detect("9123-45-67890", &ALL).is_empty(), "digit boundary");
    }

    #[test]
    fn detects_email() {
        assert_eq!(
            detect("reach me at jane.doe+tag@example.co.uk", &ALL),
            vec![PiiKind::Email]
        );
        assert!(detect("not_an_email@", &ALL).is_empty(), "no domain");
        assert!(detect("@handle hello", &ALL).is_empty(), "no local part");
        assert!(detect("a@b.c", &ALL).is_empty(), "tld too short");
    }

    #[test]
    fn detects_phone_only_when_grouped() {
        assert_eq!(detect("call (415) 555-2671", &ALL), vec![PiiKind::Phone]);
        assert_eq!(detect("+1 415-555-2671", &ALL), vec![PiiKind::Phone]);
        assert_eq!(detect("415.555.2671", &ALL), vec![PiiKind::Phone]);
        // A bare 10-digit run is NOT treated as a phone number.
        assert!(detect("4155552671", &ALL).is_empty(), "no separators");
    }

    #[test]
    fn phone_off_by_default() {
        // Default config has phone disabled.
        assert!(detect("call (415) 555-2671", &PiiConfig::default()).is_empty());
    }

    #[test]
    fn clean_text_yields_nothing() {
        assert!(detect("let's ship the release at 3pm, room 1402", &ALL).is_empty());
    }

    #[test]
    fn multiple_kinds_in_one_message() {
        let found = detect("ssn 123-45-6789 card 4111111111111111 mail a@b.com", &ALL);
        assert!(found.contains(&PiiKind::Ssn));
        assert!(found.contains(&PiiKind::CreditCard));
        assert!(found.contains(&PiiKind::Email));
    }
}
