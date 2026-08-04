use super::*;

#[test]
fn backoff_doubles_from_base() {
    assert_eq!(backoff_delay(1), Duration::seconds(30));
    assert_eq!(backoff_delay(2), Duration::seconds(60));
    assert_eq!(backoff_delay(3), Duration::seconds(120));
    assert_eq!(backoff_delay(4), Duration::seconds(240));
    assert_eq!(backoff_delay(5), Duration::seconds(480));
    assert_eq!(backoff_delay(6), Duration::seconds(960));
}

#[test]
fn backoff_is_clamped_and_monotonic() {
    let mut prev = Duration::ZERO;
    for n in 1..=40 {
        let delay = backoff_delay(n);
        assert!(delay >= prev, "non-decreasing at n={n}");
        assert!(
            delay <= Duration::seconds(MAX_DELAY_SECS),
            "clamped at n={n}"
        );
        prev = delay;
    }
    assert_eq!(backoff_delay(i32::MAX), Duration::seconds(MAX_DELAY_SECS));
}

#[test]
fn backoff_defends_non_positive_attempts() {
    assert_eq!(backoff_delay(0), Duration::seconds(30));
    assert_eq!(backoff_delay(-5), Duration::seconds(30));
}

#[test]
fn next_attempt_at_adds_backoff_to_now() {
    let now = OffsetDateTime::UNIX_EPOCH;
    assert_eq!(next_attempt_at(now, 1), now + Duration::seconds(30));
    assert_eq!(next_attempt_at(now, 3), now + Duration::seconds(120));
}

#[test]
fn is_dead_only_at_or_past_cap() {
    assert!(!is_dead_at(1));
    assert!(!is_dead_at(MAX_ATTEMPTS - 1));
    assert!(is_dead_at(MAX_ATTEMPTS));
    assert!(is_dead_at(MAX_ATTEMPTS + 1));
    assert_eq!(MAX_ATTEMPTS, 6);
}

#[test]
fn clamp_limit_defaults_and_bounds() {
    assert_eq!(clamp_limit(None), MAX_PAGE);
    assert_eq!(clamp_limit(Some(0)), MAX_PAGE);
    assert_eq!(clamp_limit(Some(-3)), MAX_PAGE);
    assert_eq!(clamp_limit(Some(1)), 1);
    assert_eq!(clamp_limit(Some(50)), 50);
    assert_eq!(clamp_limit(Some(10_000)), MAX_PAGE);
}
