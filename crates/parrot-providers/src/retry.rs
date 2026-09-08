//! Provider 指数退避 + 抖动重试。
//!
//! 包住面向网络的 provider 调用（`AnthropicProvider::send_request`），让
//! 瞬时失败（限流、超时、5xx、网络重置）以递增延迟重试。策略表见设计
//! 文档 §4.8。
//!
//! 流式阶段错误（SSE 已开始）不在这里重试——daemon 一旦开始向客户端
//! 发 delta，重启请求会导致输出重复。流错误以
//! `ProviderError::StreamError` 原样上报客户端。

use parrot_core::error::ProviderError;
use rand::Rng;
use std::time::Duration;

const MAX_RETRIES: u32 = 3;
const BASE_DELAY_MS: u64 = 500;
const MAX_DELAY_MS: u64 = 30_000;

/// `ProviderError` 是否值得重试。4xx（429 除外）与 `StreamError` 不重试
/// ——请求本身有问题，或流已经过半。
fn is_retryable(err: &ProviderError) -> bool {
    match err {
        ProviderError::RateLimited { .. } => true,
        ProviderError::Timeout(_) => true,
        ProviderError::Network(_) => true,
        ProviderError::Api { status, .. } => *status >= 500,
        ProviderError::StreamError(_) => false,
    }
}

/// 带重试/退避地执行 `op`。`op` 每次尝试都新调用一次（返回借用 future
/// 的闭包没问题——如 `|| self.send_request_once(req)`）。
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
                // 429 尊重服务端的 retry-after；否则指数退避：
                // BASE * 2^attempt。
                let base = match &e {
                    ProviderError::RateLimited { retry_after_ms } => *retry_after_ms,
                    _ => BASE_DELAY_MS.saturating_mul(2u64.saturating_pow(attempt)),
                };
                let capped = base.min(MAX_DELAY_MS);
                // ±25% 抖动，避免一队客户端同步重试。
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
        // 首次尝试 + MAX_RETRIES（3）次重试 = 共 4 次调用。
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
        // 不可重试：恰好调用一次。
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
