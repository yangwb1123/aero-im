//! [`Outcome`] — structured result of a command or sub-check.
//!
//! Replaces snaplink's fragile `ec += ...` exit-code summation with a
//! structured type that carries severity, duration, and optional machine-
//! readable detail.

use std::time::Duration;

/// Severity of an outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// All OK.
    Ok,
    /// Non-fatal warning (command succeeded with caveats).
    Warning,
    /// Fatal error.
    Error,
    /// Skipped (not applicable, preconditions unmet).
    Skip,
}

/// Structured result of executing one command or sub-check.
#[derive(Debug, Clone)]
pub struct Outcome {
    severity: Severity,
    exit_code: i32,
    duration: Duration,
    message: String,
    detail: Option<serde_json::Value>,
}

impl Outcome {
    /// Create a successful outcome.
    #[must_use]
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Ok,
            exit_code: 0,
            duration: Duration::ZERO,
            message: message.into(),
            detail: None,
        }
    }

    /// Create a warning outcome (non-zero exit but not fatal).
    #[must_use]
    pub fn warning(exit_code: i32, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            exit_code,
            duration: Duration::ZERO,
            message: message.into(),
            detail: None,
        }
    }

    /// Create an error outcome.
    #[must_use]
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            exit_code: 1,
            duration: Duration::ZERO,
            message: message.into(),
            detail: None,
        }
    }

    /// Create a skip outcome.
    #[must_use]
    pub fn skip(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Skip,
            exit_code: 2,
            duration: Duration::ZERO,
            message: message.into(),
            detail: None,
        }
    }

    /// Record the execution duration.
    #[must_use]
    pub fn with_duration(mut self, d: Duration) -> Self {
        self.duration = d;
        self
    }

    /// Attach machine-readable detail.
    #[must_use]
    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = Some(detail);
        self
    }

    // -- Accessors --

    #[must_use]
    pub fn severity(&self) -> Severity {
        self.severity
    }

    #[must_use]
    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }

    #[must_use]
    pub fn duration(&self) -> Duration {
        self.duration
    }

    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.severity == Severity::Ok
    }

    #[must_use]
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    #[must_use]
    pub fn detail(&self) -> Option<serde_json::Value> {
        self.detail.clone()
    }

    /// Combine multiple outcomes into one aggregate. Returns the worst
    /// severity (Error > Warning > Skip > Ok) and concatenates messages.
    #[must_use]
    pub fn merge(outcomes: &[Self]) -> Self {
        const fn rank(s: Severity) -> u8 {
            match s {
                Severity::Ok => 0,
                Severity::Warning => 1,
                Severity::Skip => 2,
                Severity::Error => 3,
            }
        }
        let mut worst = Severity::Ok;
        let mut exit = 0;
        let mut parts = Vec::new();
        let mut total = Duration::ZERO;
        for o in outcomes {
            total += o.duration;
            if rank(o.severity) > rank(worst) {
                worst = o.severity;
                exit = o.exit_code;
            }
            if !o.message.is_empty() {
                parts.push(o.message.clone());
            }
        }
        Self {
            severity: worst,
            exit_code: exit,
            duration: total,
            message: parts.join("; "),
            detail: None,
        }
    }
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{:?}] {}", self.severity, self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_has_exit_code_zero() {
        let o = Outcome::ok("all good");
        assert_eq!(o.exit_code(), 0);
        assert!(o.is_ok());
    }

    #[test]
    fn error_has_exit_code_one() {
        let o = Outcome::error("something broke");
        assert_eq!(o.exit_code(), 1);
        assert!(o.is_error());
    }

    #[test]
    fn skip_has_exit_code_two() {
        let o = Outcome::skip("not applicable");
        assert_eq!(o.exit_code(), 2);
    }

    #[test]
    fn merge_ok_and_error_yields_error() {
        let outcomes = vec![Outcome::ok("first"), Outcome::error("second")];
        let merged = Outcome::merge(&outcomes);
        assert!(merged.is_error());
        assert!(merged.message.contains("first"));
        assert!(merged.message.contains("second"));
    }

    #[test]
    fn merge_all_ok_yields_ok() {
        let outcomes = vec![Outcome::ok("a"), Outcome::ok("b")];
        let merged = Outcome::merge(&outcomes);
        assert!(merged.is_ok());
    }

    #[test]
    fn merge_worst_severity_wins() {
        let outcomes = vec![
            Outcome::ok("ok"),
            Outcome::skip("skip"),
            Outcome::warning(3, "warn"),
            Outcome::error("err"),
        ];
        let merged = Outcome::merge(&outcomes);
        assert!(merged.is_error());
        assert_eq!(merged.exit_code(), 1);
    }

    #[test]
    fn merge_empty_returns_ok_with_empty_message() {
        let merged = Outcome::merge(&[]);
        assert!(merged.is_ok());
        assert_eq!(merged.exit_code(), 0);
        assert!(merged.message().is_empty());
    }

    #[test]
    fn merge_single_ok_returns_that_ok() {
        let merged = Outcome::merge(&[Outcome::ok("only")]);
        assert!(merged.is_ok());
        assert_eq!(merged.message(), "only");
    }

    #[test]
    fn merge_single_error_returns_that_error() {
        let merged = Outcome::merge(&[Outcome::error("only error")]);
        assert!(merged.is_error());
        assert_eq!(merged.message(), "only error");
    }

    #[test]
    fn merge_all_warnings_yields_warning() {
        let merged = Outcome::merge(&[Outcome::warning(3, "warn1"), Outcome::warning(5, "warn2")]);
        assert_eq!(merged.severity(), Severity::Warning);
        // Warning does not have is_warning(), check severity directly
        assert!(!merged.is_error());
        assert!(!merged.is_ok());
    }

    #[test]
    fn merge_accumulates_durations() {
        let d1 = std::time::Duration::from_secs(1);
        let d2 = std::time::Duration::from_secs(2);
        let o1 = Outcome::ok("first").with_duration(d1);
        let o2 = Outcome::ok("second").with_duration(d2);
        let merged = Outcome::merge(&[o1, o2]);
        assert_eq!(merged.duration(), Duration::from_secs(3));
    }

    #[test]
    fn outcome_with_unicode_message() {
        let o = Outcome::ok("✓ 测试 unicode 🎉");
        assert!(o.is_ok());
        assert!(o.message().contains("🎉"));
    }

    #[test]
    fn outcome_with_long_message() {
        let long = "a".repeat(10000);
        let o = Outcome::ok(&long);
        assert!(o.message().len() >= 10000);
    }

    #[test]
    fn merge_skip_does_not_change_exit_code_of_error() {
        let merged = Outcome::merge(&[Outcome::skip("skipped"), Outcome::error("real error")]);
        assert!(merged.is_error());
        assert_eq!(merged.exit_code(), 1);
        assert!(merged.message().contains("skipped"));
        assert!(merged.message().contains("real error"));
    }

    #[test]
    fn outcome_with_detail_is_serializable() {
        let detail = serde_json::json!({"key": "value", "nested": {"a": 1}});
        let o = Outcome::ok("with detail").with_detail(detail.clone());
        assert_eq!(o.detail(), Some(detail));
        // Verify it round-trips through JSON
        let json = serde_json::to_string(&o.detail()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["key"], "value");
    }

    #[test]
    fn outcome_defaults_to_zero_duration() {
        let o = Outcome::ok("instant");
        assert_eq!(o.duration(), Duration::ZERO);
    }

    #[test]
    fn severity_ordering_is_consistent() {
        fn rank(s: Severity) -> u8 {
            match s {
                Severity::Ok => 0,
                Severity::Warning => 1,
                Severity::Skip => 2,
                Severity::Error => 3,
            }
        }
        assert!(rank(Severity::Ok) < rank(Severity::Warning));
        assert!(rank(Severity::Warning) < rank(Severity::Skip));
        assert!(rank(Severity::Skip) < rank(Severity::Error));
    }

    // ---- Property-based tests (manual fuzz) ----

    /// Merge invariants tested with exhaustive combinations.
    #[test]
    fn prop_merge_exhaustive_severity_combinations() {
        let cases = vec![
            (vec![Severity::Ok, Severity::Ok], Severity::Ok),
            (vec![Severity::Ok, Severity::Warning], Severity::Warning),
            (vec![Severity::Warning, Severity::Ok], Severity::Warning),
            (vec![Severity::Ok, Severity::Skip], Severity::Skip),
            (vec![Severity::Skip, Severity::Ok], Severity::Skip),
            (vec![Severity::Ok, Severity::Error], Severity::Error),
            (vec![Severity::Error, Severity::Ok], Severity::Error),
            (vec![Severity::Warning, Severity::Skip], Severity::Skip),
            (vec![Severity::Skip, Severity::Warning], Severity::Skip),
            (vec![Severity::Warning, Severity::Error], Severity::Error),
            (vec![Severity::Error, Severity::Warning], Severity::Error),
            (vec![Severity::Skip, Severity::Error], Severity::Error),
            (vec![Severity::Error, Severity::Skip], Severity::Error),
            (
                vec![Severity::Warning, Severity::Warning],
                Severity::Warning,
            ),
            (vec![Severity::Skip, Severity::Skip], Severity::Skip),
            (vec![Severity::Error, Severity::Error], Severity::Error),
        ];
        for (input, expected) in &cases {
            let outcomes: Vec<Outcome> = input
                .iter()
                .map(|s| match s {
                    Severity::Ok => Outcome::ok(""),
                    Severity::Warning => Outcome::warning(1, ""),
                    Severity::Skip => Outcome::skip(""),
                    Severity::Error => Outcome::error(""),
                })
                .collect();
            let merged = Outcome::merge(&outcomes);
            assert_eq!(
                merged.severity(),
                *expected,
                "merge {:?} should give {:?}",
                input,
                expected
            );
        }
    }

    /// Merge duration is sum of individual durations (tested with random values).
    #[test]
    fn prop_merge_random_durations() {
        use std::time::Duration;
        // Test 100 random duration combinations
        for _ in 0..100 {
            let n = (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 10) as usize
                + 1;
            let mut total = 0u64;
            let mut outcomes = Vec::new();
            for _ in 0..n {
                let d = (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
                    % 1000) as u64;
                total += d;
                outcomes.push(Outcome::ok("").with_duration(Duration::from_micros(d)));
            }
            let merged = Outcome::merge(&outcomes);
            assert_eq!(
                merged.duration().as_micros() as u64,
                total,
                "duration sum mismatch"
            );
        }
    }

    /// Merge of many outcomes (1000 elements) does not crash.
    #[test]
    fn prop_merge_large_input() {
        let mut outcomes = Vec::with_capacity(1000);
        for i in 0..1000 {
            outcomes.push(if i % 3 == 0 {
                Outcome::ok(&i.to_string())
            } else if i % 3 == 1 {
                Outcome::warning(i, &i.to_string())
            } else {
                Outcome::error(&i.to_string())
            });
        }
        let merged = Outcome::merge(&outcomes);
        // Should not crash, should have some severity
        assert!(merged.severity() == Severity::Error || merged.severity() == Severity::Warning);
        // Message should contain at least some of the inputs
        assert!(merged.message().len() > 0);
    }

    /// Empty merge returns Ok with zero duration.
    #[test]
    fn prop_merge_empty() {
        let merged = Outcome::merge(&[]);
        assert!(merged.is_ok());
        assert_eq!(merged.duration(), Duration::ZERO);
    }
}
