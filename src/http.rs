use std::time::Duration;

use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::{RequestBuilder, Response, StatusCode};
use tracing::warn;

const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

/// Retry configuration for outbound HTTP calls (sources and LLM providers).
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_retries: usize,
    pub base_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_secs(1),
        }
    }
}

pub fn is_retryable_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::REQUEST_TIMEOUT
        || status.is_server_error()
}

pub fn is_retryable_error(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.is_connect() || err.is_request() || err.is_body()
}

pub fn backoff_delay(base_delay: Duration, attempt: usize) -> Duration {
    let shift = u32::try_from(attempt).unwrap_or(u32::MAX);
    let factor = 1u32.checked_shl(shift).unwrap_or(u32::MAX);
    base_delay.saturating_mul(factor).min(MAX_RETRY_DELAY)
}

pub fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let raw = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    let secs: u64 = raw.parse().ok()?;
    Some(Duration::from_secs(secs).min(MAX_RETRY_DELAY))
}

/// Execute an HTTP request with exponential backoff retries on 429/408/5xx and transient transport errors.
pub async fn send_with_retry<F>(
    label: &str,
    policy: RetryPolicy,
    mut build_request: F,
) -> std::result::Result<Response, reqwest::Error>
where
    F: FnMut() -> RequestBuilder,
{
    let mut attempt = 0usize;
    loop {
        match build_request().send().await {
            Ok(resp) => {
                let status = resp.status();
                if is_retryable_status(status) && attempt < policy.max_retries {
                    let delay = parse_retry_after(resp.headers())
                        .unwrap_or_else(|| backoff_delay(policy.base_delay, attempt));
                    warn!(
                        target_label = %label,
                        status = %status,
                        attempt = attempt + 1,
                        max_retries = policy.max_retries,
                        delay_ms = delay.as_millis() as u64,
                        "transient HTTP status; retrying"
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                    continue;
                }
                return Ok(resp);
            }
            Err(err) => {
                if is_retryable_error(&err) && attempt < policy.max_retries {
                    let delay = backoff_delay(policy.base_delay, attempt);
                    warn!(
                        target_label = %label,
                        error = %err,
                        attempt = attempt + 1,
                        max_retries = policy.max_retries,
                        delay_ms = delay.as_millis() as u64,
                        "transient HTTP transport error; retrying"
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                    continue;
                }
                return Err(err);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn classifies_retryable_status_codes() {
        assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(StatusCode::REQUEST_TIMEOUT));
        assert!(is_retryable_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_retryable_status(StatusCode::BAD_GATEWAY));
        assert!(is_retryable_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(!is_retryable_status(StatusCode::OK));
        assert!(!is_retryable_status(StatusCode::BAD_REQUEST));
        assert!(!is_retryable_status(StatusCode::UNAUTHORIZED));
        assert!(!is_retryable_status(StatusCode::NOT_FOUND));
    }

    #[test]
    fn computes_capped_exponential_backoff_and_retry_after() {
        let base = Duration::from_millis(200);
        assert_eq!(backoff_delay(base, 0), Duration::from_millis(200));
        assert_eq!(backoff_delay(base, 1), Duration::from_millis(400));
        assert_eq!(backoff_delay(base, 2), Duration::from_millis(800));
        assert_eq!(backoff_delay(Duration::from_secs(20), 2), MAX_RETRY_DELAY);

        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("5"));
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(5)));

        headers.insert(RETRY_AFTER, HeaderValue::from_static("120"));
        assert_eq!(parse_retry_after(&headers), Some(MAX_RETRY_DELAY));
    }
}
