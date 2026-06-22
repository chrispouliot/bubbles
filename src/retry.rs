//! Async retry helper for absorbing transient send errors.
//!
//! The [`retry`] function runs an async-producing closure up to N times,
//! sleeping with backoff between attempts, returning the first success
//! or the last error.
//!
//! Used by [`crate::protocol::rustpush_backend::send_text`] to absorb
//! transient `SendTimedOut` errors from the underlying iMessage send.
//! The backoff duration parameter lets callers control total latency.

use std::future::Future;
use std::time::Duration;

/// Runs the async-producing closure `f` up to `attempts` times, sleeping
/// `backoff` between attempts.
///
/// Returns the first `Ok` result, or the last `Err` if all attempts fail.
pub async fn retry<F, T, E>(
    attempts: usize,
    backoff: Duration,
    mut f: impl FnMut() -> F,
) -> Result<T, E>
where
    F: Future<Output = Result<T, E>>,
{
    let mut last_error = None;
    for _ in 0..attempts {
        let result = f().await;
        match result {
            Ok(value) => return Ok(value),
            Err(e) => last_error = Some(e),
        }
        tokio::time::sleep(backoff).await;
    }
    Err(last_error.expect("retry requires attempts > 0"))
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn retry_succeeds_after_two_failures() {
        let attempts = std::cell::Cell::new(0u32);
        let result = super::retry(3, std::time::Duration::ZERO, || {
            let n = attempts.get();
            attempts.set(n + 1);
            async move {
                if n < 2 {
                    Err::<(), &str>("not yet")
                } else {
                    Ok(())
                }
            }
        })
        .await;

        assert!(result.is_ok(), "expected Ok after two failures, got {result:?}");
        assert_eq!(
            attempts.get(),
            3,
            "expected exactly 3 attempts when succeeding on the third"
        );
    }

    #[tokio::test]
    async fn retry_all_fail() {
        let attempts = std::cell::Cell::new(0u32);
        let result = super::retry(3, std::time::Duration::ZERO, || {
            let n = attempts.get();
            attempts.set(n + 1);
            async move { Err::<(), &str>("always fail") }
        })
        .await;

        assert!(result.is_err(), "expected Err when all attempts fail");
        assert_eq!(
            attempts.get(),
            3,
            "expected exactly 3 attempts when all fail"
        );
    }

    #[tokio::test]
    async fn retry_succeeds_first_try() {
        let attempts = std::cell::Cell::new(0u32);
        let result = super::retry(3, std::time::Duration::ZERO, || {
            attempts.set(attempts.get() + 1);
            async move { Ok::<_, &str>("first try") }
        })
        .await;

        assert_eq!(result, Ok("first try"), "expected Ok on first attempt");
        assert_eq!(
            attempts.get(),
            1,
            "expected exactly 1 attempt when succeeding first try"
        );
    }
}
