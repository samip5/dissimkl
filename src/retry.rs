use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tracing::{debug, warn};

/// Configuration for the retry executor.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of attempts (1 = no retries).
    pub max_retries: u32,
    /// Initial delay between attempts.
    pub base_delay: Duration,
    /// Maximum delay between attempts.
    pub max_delay: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 4,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
        }
    }
}

impl RetryConfig {
    #[allow(dead_code)]
    pub fn new(max_retries: u32, base_delay: Duration, max_delay: Duration) -> Self {
        Self {
            max_retries,
            base_delay,
            max_delay,
        }
    }

    /// Calculate the delay for attempt `n` (0-indexed).
    fn delay_for(&self, attempt: u32) -> Duration {
        let multiplier = 1u64 << attempt.min(10); // cap shift to avoid overflow
        let delay = self.base_delay.saturating_mul(multiplier as u32);
        delay.min(self.max_delay)
    }
}

/// Classification of an error for retry purposes.
#[derive(Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// Should be retried (HTTP 429, 5xx, network errors).
    Retryable,
    /// Should not be retried (HTTP 401, 403, 404, parse errors, etc.).
    Fatal,
}

/// Classify a `ureq::Error` to decide if we should retry.
pub fn classify_ureq_error(err: &ureq::Error) -> ErrorKind {
    match err {
        ureq::Error::Status(status, _) => match *status {
            429 | 500..=599 => ErrorKind::Retryable,
            _ => ErrorKind::Fatal,
        },
        ureq::Error::Transport(_) => ErrorKind::Retryable,
    }
}

/// Error wrapper returned by `execute_with_retry`.
#[derive(Debug)]
pub struct RetryError {
    pub attempts: u32,
    pub last_error: anyhow::Error,
}

impl std::fmt::Display for RetryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Failed after {} attempt(s): {}",
            self.attempts, self.last_error
        )
    }
}

impl std::error::Error for RetryError {}

/// Execute a closure with retry logic.
///
/// `f` receives the current attempt number (0-indexed) and returns either
/// `Ok(T)` or `Err((ErrorKind, anyhow::Error))`.
///
/// - `ErrorKind::Retryable` → sleep and retry (up to `config.max_retries` total).
/// - `ErrorKind::Fatal` → return immediately without further retries.
pub fn execute_with_retry<T, F>(config: &RetryConfig, mut f: F) -> Result<T>
where
    F: FnMut(u32) -> Result<T, (ErrorKind, anyhow::Error)>,
{
    let mut last_err = anyhow!("No attempts made");

    for attempt in 0..config.max_retries {
        match f(attempt) {
            Ok(value) => return Ok(value),
            Err((ErrorKind::Fatal, err)) => {
                debug!(attempt, "Fatal error, not retrying: {}", err);
                return Err(anyhow::Error::new(RetryError {
                    attempts: attempt + 1,
                    last_error: err,
                }));
            }
            Err((ErrorKind::Retryable, err)) => {
                let delay = config.delay_for(attempt);
                warn!(
                    attempt,
                    "Retryable error (retry in {:?}): {}", delay, err
                );
                last_err = err;
                if attempt + 1 < config.max_retries {
                    thread::sleep(delay);
                }
            }
        }
    }

    Err(anyhow::Error::new(RetryError {
        attempts: config.max_retries,
        last_error: last_err,
    }))
}

/// Convenience: wrap a `ureq` call so that HTTP status errors are classified
/// and returned in the format `execute_with_retry` expects.
///
/// Usage:
/// ```rust
/// let resp = execute_with_retry(&cfg, |_| {
///     wrap_ureq(|| agent.get(url).call())
/// })?;
/// ```
pub fn wrap_ureq<T, F>(f: F) -> Result<T, (ErrorKind, anyhow::Error)>
where
    F: FnOnce() -> Result<T, ureq::Error>,
{
    f().map_err(|e| {
        let kind = classify_ureq_error(&e);
        (kind, anyhow::Error::new(e))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_delay_doubling() {
        let cfg = RetryConfig {
            max_retries: 5,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(10),
        };
        assert_eq!(cfg.delay_for(0), Duration::from_millis(100));
        assert_eq!(cfg.delay_for(1), Duration::from_millis(200));
        assert_eq!(cfg.delay_for(2), Duration::from_millis(400));
        assert_eq!(cfg.delay_for(3), Duration::from_millis(800));
    }

    #[test]
    fn test_delay_capped() {
        let cfg = RetryConfig {
            max_retries: 10,
            base_delay: Duration::from_millis(1000),
            max_delay: Duration::from_secs(5),
        };
        for attempt in 3..10 {
            assert_eq!(cfg.delay_for(attempt), Duration::from_secs(5));
        }
    }

    #[test]
    fn test_fatal_stops_immediately() {
        let cfg = RetryConfig {
            max_retries: 5,
            ..Default::default()
        };
        let mut call_count = 0u32;
        let result: Result<()> = execute_with_retry(&cfg, |_| {
            call_count += 1;
            Err((ErrorKind::Fatal, anyhow!("not found")))
        });
        assert!(result.is_err());
        assert_eq!(call_count, 1);
    }

    #[test]
    fn test_retryable_retries_up_to_max() {
        let cfg = RetryConfig {
            max_retries: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(10),
        };
        let mut call_count = 0u32;
        let result: Result<()> = execute_with_retry(&cfg, |_| {
            call_count += 1;
            Err((ErrorKind::Retryable, anyhow!("server error")))
        });
        assert!(result.is_err());
        assert_eq!(call_count, 3);
    }
}
