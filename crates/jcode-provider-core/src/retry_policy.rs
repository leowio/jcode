//! Request retry budgets shared by streaming providers.
//!
//! Zero means keep retrying transient failures until the consumer cancels.
//! An explicit positive budget still bounds the total number of attempts.

use anyhow::Result;
use jcode_message_types::StreamEvent;
use std::future::Future;
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    max_attempts: u32,
    backoff_cap: Duration,
}

impl RetryPolicy {
    pub fn new(max_attempts: u32, backoff_cap_secs: u64) -> Self {
        Self {
            max_attempts,
            backoff_cap: Duration::from_secs(backoff_cap_secs.max(1)),
        }
    }

    /// Zero-based attempt indices. The unlimited counter saturates instead of
    /// overflowing, without terminating an otherwise retryable request.
    pub fn attempts(self) -> impl Iterator<Item = u32> {
        std::iter::successors(Some(0_u32), |attempt| Some(attempt.saturating_add(1)))
            .take_while(move |attempt| self.max_attempts == 0 || *attempt < self.max_attempts)
    }

    pub fn can_retry(self, attempt: u32) -> bool {
        self.max_attempts == 0 || attempt < self.max_attempts.saturating_sub(1)
    }

    /// Retry-After remains authoritative; the configured cap bounds the usual
    /// exponential backoff, rather than shortening a provider's requested wait.
    pub fn delay(self, attempt: u32, base_ms: u64, server_hint: Option<Duration>) -> Duration {
        server_hint.unwrap_or_else(|| {
            crate::attempt_tracker::retry_backoff_delay(attempt, base_ms).min(self.backoff_cap)
        })
    }
}

/// Stop an in-flight attempt or its backoff as soon as its stream is dropped.
pub async fn while_consumer_open<T>(
    tx: &mpsc::Sender<Result<StreamEvent>>,
    future: impl Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        _ = tx.closed() => None,
        result = future => Some(result),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_policy_keeps_retrying_beyond_previous_budgets() {
        let policy = RetryPolicy::new(0, 30);
        assert_eq!(policy.attempts().take(1000).last(), Some(999));
        assert!(policy.can_retry(u32::MAX));
        assert!(policy.delay(u32::MAX, 1000, None) <= Duration::from_secs(30));
    }

    #[test]
    fn explicit_budget_includes_initial_attempt() {
        let policy = RetryPolicy::new(3, 30);
        assert_eq!(policy.attempts().collect::<Vec<_>>(), [0, 1, 2]);
        assert!(policy.can_retry(1));
        assert!(!policy.can_retry(2));
        assert!(!RetryPolicy::new(1, 30).can_retry(0));
    }

    #[test]
    fn backoff_cap_does_not_shorten_retry_after() {
        let policy = RetryPolicy::new(0, 30);
        assert_eq!(
            policy.delay(1000, 1000, Some(Duration::from_secs(60))),
            Duration::from_secs(60)
        );
        assert!(policy.delay(1000, 1000, None) <= Duration::from_secs(30));
    }

    #[tokio::test]
    async fn cancellation_drops_pending_attempt_without_waiting_for_timeout() {
        let (tx, rx) = mpsc::channel(1);
        let wait = while_consumer_open(&tx, std::future::pending::<()>());
        drop(rx);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), wait)
                .await
                .unwrap()
                .is_none()
        );
    }
}
