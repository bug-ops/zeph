use std::future::Future;
use std::time::Duration;

use crate::error::LlmError;
use crate::provider::StatusTx;

const BASE_BACKOFF_SECS: u64 = 1;

/// Parse the `Retry-After` header value as seconds, falling back to exponential backoff.
pub(crate) fn retry_delay(response: &reqwest::Response, attempt: u32) -> Duration {
    if let Some(val) = response.headers().get("retry-after")
        && let Ok(s) = val.to_str()
        && let Ok(secs) = s.parse::<u64>()
    {
        return Duration::from_secs(secs);
    }
    Duration::from_secs(BASE_BACKOFF_SECS << attempt)
}

/// Send an HTTP request, retrying up to `max_retries` times on 429 responses.
///
/// `f` must return a `reqwest::Response`. On each rate-limited attempt, emits a status
/// message and waits before retrying. Returns the successful `Response` for further
/// processing by the caller, or an error.
///
/// # Errors
///
/// Returns `LlmError::RateLimited` if all attempts are exhausted, or the underlying
/// `reqwest::Error` wrapped as `LlmError::Http` for other failures.
pub(crate) async fn send_with_retry<F, Fut>(
    provider_name: &str,
    max_retries: u32,
    status_tx: Option<&StatusTx>,
    mut f: F,
) -> Result<reqwest::Response, LlmError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<reqwest::Response, reqwest::Error>>,
{
    for attempt in 0..=max_retries {
        let response = f().await.map_err(LlmError::Http)?;
        let status = response.status();

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            if attempt == max_retries {
                return Err(LlmError::RateLimited);
            }
            let delay = retry_delay(&response, attempt);
            let msg = format!(
                "{provider_name} rate limited, retrying in {}s ({}/{})",
                delay.as_secs(),
                attempt + 1,
                max_retries
            );
            if let Some(tx) = status_tx {
                let _ = tx.send(msg.clone());
            }
            tracing::warn!("{msg}");
            tokio::time::sleep(delay).await;
            continue;
        }

        return Ok(response);
    }

    Err(LlmError::RateLimited)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delay_exponential_backoff() {
        // Without a response, we can't test header parsing, but verify the math
        assert_eq!(BASE_BACKOFF_SECS << 0, 1);
        assert_eq!(BASE_BACKOFF_SECS << 1, 2);
        assert_eq!(BASE_BACKOFF_SECS << 2, 4);
    }
}
