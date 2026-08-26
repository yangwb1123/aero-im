//! Shared retry policy for paid interactive provider calls.
//!
//! Provider wrappers classify an individual attempt (transport/status/parse)
//! while this module owns the bounded exponential schedule. Keeping the policy
//! here makes Voyage, Whisper, and Anthropic agree on retryable status codes and
//! prevents accounting wrappers from seeing each HTTP attempt as a new logical
//! operation.

use std::{future::Future, time::Duration};

use reqwest::StatusCode;

use crate::error::{AiError, Result};

/// One provider attempt's result classification.
pub(crate) struct AttemptError {
    pub(crate) retryable: bool,
    pub(crate) error: AiError,
}

/// Retry rate limits and server failures; client errors fail immediately.
#[must_use]
pub(crate) fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Exponential delay for a one-based failed-attempt number.
#[must_use]
pub(crate) fn backoff_delay(attempt: usize, base: Duration) -> Duration {
    let shift = attempt.saturating_sub(1).min(31);
    base.saturating_mul(1_u32 << shift)
}

/// Run one logical provider operation with bounded exponential backoff.
pub(crate) async fn retry_with_backoff<T, F, Fut>(
    max_attempts: usize,
    base: Duration,
    mut attempt: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, AttemptError>>,
{
    let max_attempts = max_attempts.max(1);
    for number in 1..=max_attempts {
        match attempt().await {
            Ok(value) => return Ok(value),
            Err(failure) if !failure.retryable || number == max_attempts => {
                return Err(failure.error);
            }
            Err(_failure) => {
                let delay = backoff_delay(number, base);
                tracing::debug!(attempt = number, delay = ?delay, "AI provider retry");
                tokio::time::sleep(delay).await;
            }
        }
    }
    unreachable!("max_attempts is clamped to at least one")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_statuses_match_provider_contract() {
        assert!(is_retryable(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_retryable(StatusCode::BAD_GATEWAY));
        assert!(!is_retryable(StatusCode::BAD_REQUEST));
        assert!(!is_retryable(StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn backoff_is_one_two_four_seconds_for_one_second_base() {
        let base = Duration::from_secs(1);
        assert_eq!(backoff_delay(1, base), Duration::from_secs(1));
        assert_eq!(backoff_delay(2, base), Duration::from_secs(2));
        assert_eq!(backoff_delay(3, base), Duration::from_secs(4));
    }

    #[tokio::test]
    async fn retry_helper_returns_last_error_and_stops_on_non_retryable() {
        let mut calls = 0;
        let error = retry_with_backoff(3, Duration::ZERO, || {
            calls += 1;
            let current = calls;
            async move {
                Err::<(), _>(AttemptError {
                    retryable: true,
                    error: AiError::Internal(format!("attempt {current}")),
                })
            }
        })
        .await
        .unwrap_err();
        assert_eq!(calls, 3);
        assert!(error.to_string().contains("attempt 3"));

        calls = 0;
        let _ = retry_with_backoff(3, Duration::ZERO, || {
            calls += 1;
            async {
                Err::<(), _>(AttemptError {
                    retryable: false,
                    error: AiError::Invalid("no retry".into()),
                })
            }
        })
        .await;
        assert_eq!(calls, 1);
    }
}
