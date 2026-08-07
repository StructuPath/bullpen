//! Shared transient-failure retry policy for provider transports.

use std::time::Duration;

pub const MAX_ATTEMPTS: u32 = 5;
const BASE_DELAY_MS: u64 = 500;
const MAX_DELAY_MS: u64 = 16_000;

/// Whether an HTTP status is worth retrying. 429 is rate limiting, 529 is
/// Anthropic's overloaded status, 5xx is transient server failure.
pub fn retryable_status(status: u16) -> bool {
    status == 429 || status == 529 || (500..600).contains(&status)
}

/// Exponential backoff with a cap. `retry_after` (from a Retry-After header)
/// wins over the computed delay when present.
pub fn delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
    if let Some(d) = retry_after {
        return d.min(Duration::from_millis(MAX_DELAY_MS * 4));
    }
    let exp = BASE_DELAY_MS.saturating_mul(1u64 << attempt.min(10));
    Duration::from_millis(exp.min(MAX_DELAY_MS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses() {
        assert!(retryable_status(429));
        assert!(retryable_status(529));
        assert!(retryable_status(500));
        assert!(retryable_status(503));
        assert!(!retryable_status(400));
        assert!(!retryable_status(401));
        assert!(!retryable_status(404));
    }

    #[test]
    fn backoff_caps() {
        assert_eq!(delay(0, None), Duration::from_millis(500));
        assert_eq!(delay(1, None), Duration::from_millis(1000));
        assert_eq!(delay(10, None), Duration::from_millis(16_000));
        assert_eq!(
            delay(0, Some(Duration::from_secs(3))),
            Duration::from_secs(3)
        );
    }
}
