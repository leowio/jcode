//! A request budget shared by provider transports. `u32::MAX` is an explicit
//! unlimited setting, not a very large finite range that eventually overflows.

use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    max_attempts: u32,
    backoff_cap: Duration,
}

impl RetryPolicy {
    pub fn new(max_attempts: u32, backoff_cap: Duration) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            backoff_cap: backoff_cap.max(Duration::from_millis(1)),
        }
    }

    pub fn max_attempts(self) -> u32 {
        self.max_attempts
    }

    pub fn is_unlimited(self) -> bool {
        self.max_attempts == u32::MAX
    }

    pub fn allows_retry_after(self, attempt: u32) -> bool {
        self.is_unlimited() || attempt.saturating_add(1) < self.max_attempts
    }

    pub fn attempts(self) -> RetryAttempts {
        RetryAttempts {
            policy: self,
            next: 0,
        }
    }

    pub fn retry_after_from_error(self, error: &anyhow::Error) -> Option<Duration> {
        if self.is_unlimited() {
            crate::retry_after::retry_after_unbounded_from_error(error)
        } else {
            crate::retry_after::retry_after_from_error(error)
        }
    }

    /// A validated provider hint takes precedence over the exponential cap:
    /// retrying before Retry-After elapses would only prolong a rate limit.
    pub fn delay(self, attempt: u32, server_hint: Option<Duration>, base_ms: u64) -> Duration {
        server_hint.unwrap_or_else(|| {
            crate::attempt_tracker::retry_backoff_delay(attempt, base_ms).min(self.backoff_cap)
        })
    }
}

pub struct RetryAttempts {
    policy: RetryPolicy,
    next: u32,
}

impl Iterator for RetryAttempts {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.policy.is_unlimited() && self.next >= self.policy.max_attempts {
            return None;
        }
        let attempt = self.next;
        self.next = self.next.saturating_add(1);
        Some(attempt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finite_budget_includes_the_initial_attempt() {
        let policy = RetryPolicy::new(3, Duration::from_secs(30));
        assert_eq!(policy.attempts().collect::<Vec<_>>(), vec![0, 1, 2]);
        assert!(policy.allows_retry_after(1));
        assert!(!policy.allows_retry_after(2));
        assert!(!policy.allows_retry_after(u32::MAX));
        assert_eq!(RetryPolicy::new(0, Duration::ZERO).attempts().count(), 1);
    }

    #[test]
    fn unlimited_budget_survives_counter_saturation() {
        let policy = RetryPolicy::new(u32::MAX, Duration::from_secs(30));
        assert_eq!(policy.attempts().take(100).count(), 100);
        let mut attempts = RetryAttempts {
            policy,
            next: u32::MAX - 1,
        };
        assert_eq!(attempts.next(), Some(u32::MAX - 1));
        assert_eq!(attempts.next(), Some(u32::MAX));
        assert_eq!(attempts.next(), Some(u32::MAX));
        assert!(policy.allows_retry_after(u32::MAX));
    }

    #[test]
    fn backoff_is_capped_but_server_retry_after_is_honored() {
        let policy = RetryPolicy::new(u32::MAX, Duration::from_secs(30));
        for attempt in [1, 8, 100, u32::MAX] {
            assert!(policy.delay(attempt, None, 1000) <= Duration::from_secs(30));
        }
        assert_eq!(
            policy.delay(1, Some(Duration::from_secs(120)), 1000),
            Duration::from_secs(120)
        );
        assert_eq!(policy.delay(1, Some(Duration::ZERO), 1000), Duration::ZERO);
    }

    #[test]
    fn unlimited_budget_preserves_long_http_and_stream_retry_hints() {
        let unlimited = RetryPolicy::new(u32::MAX, Duration::from_secs(30));
        let finite = RetryPolicy::new(3, Duration::from_secs(30));
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "3600".parse().unwrap());
        for hint in [
            crate::retry_after::retry_after(&headers).unwrap(),
            crate::retry_after::retry_after_body(r#"{"retry_after_seconds":3600}"#).unwrap(),
            crate::retry_after::RetryAfter::from_delay(Duration::from_secs(3600)),
        ] {
            let error = crate::retry_after::error_with_retry_after("rate limit".into(), Some(hint));
            assert!(unlimited.retry_after_from_error(&error).unwrap() > Duration::from_secs(3590));
            assert!(
                finite.retry_after_from_error(&error).unwrap()
                    <= crate::retry_after::MAX_RETRY_AFTER
            );
        }
        // Malformed huge durations cannot overflow the monotonic clock.
        let hint = crate::retry_after::RetryAfter::from_delay(Duration::MAX);
        assert!(hint.remaining_unbounded() <= crate::retry_after::MAX_RETRY_AFTER);
    }
}
