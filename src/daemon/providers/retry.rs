//! Provider retry with exponential backoff + jitter.
//!
//! Wraps network-facing provider calls (`AnthropicProvider::send_request`) so
//! transient failures (rate limits, timeouts, 5xx, network resets) get
//! retried with growing delays. See design doc §4.8 for the policy table.
//!
//! Stream-phase errors (SSE already started) are NOT retried here — once the
//! daemon has begun emitting deltas to the client, restarting the request
//! would duplicate output. Stream errors surface as `ProviderError::StreamError`
//! and are reported to the client as-is.

use parrot_core::error::ProviderError;
use rand::Rng;
use std::time::Duration;

const MAX_RETRIES: u32 = 3;
const BASE_DELAY_MS: u64 = 500;
const MAX_DELAY_MS: u64 = 30_000;

/// Whether a `ProviderError` is worth retrying. 4xx (other than 429) and
/// `StreamError` are not — the request itself is bad, or the stream is
/// already mid-flight.
pub fn is_retryable(err: &ProviderError) -> bool {
    match err {
        ProviderError::RateLimited { .. } => true,
        ProviderError::Timeout(_) => true,
        ProviderError::Network(_) => true,
        ProviderError::Api { status, .. } => *status >= 500,
        ProviderError::StreamError(_) => false,
    }
}

/// Run `op` with retry/backoff. `op` is called fresh on each attempt
/// (closures returning a borrowing future work fine — e.g.
/// `|| self.send_request_once(req)`).
pub async fn with_retry<F, Fut, T>(mut op: F) -> Result<T, ProviderError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ProviderError>>,
{
    let mut attempt = 0u32;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) if !is_retryable(&e) => return Err(e),
            Err(e) if attempt >= MAX_RETRIES => {
                tracing::warn!(attempt, error = %e, "retry budget exhausted, giving up");
                return Err(e);
            }
            Err(e) => {
                // For 429, honor the server's retry-after if present.
                // Otherwise exponential backoff: BASE * 2^attempt.
                let base = match &e {
                    ProviderError::RateLimited { retry_after_ms } => *retry_after_ms,
                    _ => BASE_DELAY_MS.saturating_mul(2u64.saturating_pow(attempt)),
                };
                let capped = base.min(MAX_DELAY_MS);
                // ±25% jitter so a fleet of clients doesn't synchronize retries.
                let jitter = if capped > 0 {
                    rand::rng().random_range(0..capped / 4 + 1)
                } else {
                    0
                };
                let total = capped + jitter;
                tracing::warn!(
                    attempt,
                    delay_ms = total,
                    error = %e,
                    "retrying provider call after backoff"
                );
                tokio::time::sleep(Duration::from_millis(total)).await;
                attempt += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_classification() {
        assert!(is_retryable(&ProviderError::RateLimited {
            retry_after_ms: 100
        }));
        assert!(is_retryable(&ProviderError::Timeout(500)));
        assert!(is_retryable(&ProviderError::Network("conn reset".into())));
        assert!(is_retryable(&ProviderError::Api {
            status: 500,
            body: "boom".into()
        }));
        assert!(is_retryable(&ProviderError::Api {
            status: 503,
            body: "down".into()
        }));

        assert!(!is_retryable(&ProviderError::Api {
            status: 400,
            body: "bad".into()
        }));
        assert!(!is_retryable(&ProviderError::Api {
            status: 401,
            body: "auth".into()
        }));
        assert!(!is_retryable(&ProviderError::StreamError(
            "mid-stream".into()
        )));
    }

    #[tokio::test]
    async fn with_retry_succeeds_on_second_attempt() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = AtomicU32::new(0);
        let result: Result<u32, ProviderError> = with_retry(|| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 0 {
                    Err(ProviderError::Network("transient".into()))
                } else {
                    Ok(42)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn with_retry_gives_up_after_max_retries() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = AtomicU32::new(0);
        let result: Result<(), ProviderError> = with_retry(|| {
            calls.fetch_add(1, Ordering::SeqCst);
            async move { Err(ProviderError::Network("always fails".into())) }
        })
        .await;
        assert!(result.is_err());
        // 1 initial attempt + MAX_RETRIES (3) retries = 4 total calls.
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn with_retry_does_not_retry_non_retryable() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = AtomicU32::new(0);
        let result: Result<(), ProviderError> = with_retry(|| {
            calls.fetch_add(1, Ordering::SeqCst);
            async move {
                Err(ProviderError::Api {
                    status: 400,
                    body: "bad request".into(),
                })
            }
        })
        .await;
        assert!(matches!(
            result,
            Err(ProviderError::Api { status: 400, .. })
        ));
        // Non-retryable: exactly one call.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
