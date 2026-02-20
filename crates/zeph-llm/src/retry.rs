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

    /// Spawn a minimal HTTP server that returns a fixed response for each connection.
    /// Returns (port, join_handle).
    async fn spawn_mock_server(responses: Vec<&'static str>) -> (u16, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = tokio::spawn(async move {
            for resp in responses {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let (reader, mut writer) = stream.split();
                    let mut buf_reader = BufReader::new(reader);
                    // Drain headers
                    let mut line = String::new();
                    loop {
                        line.clear();
                        buf_reader.read_line(&mut line).await.unwrap_or(0);
                        if line == "\r\n" || line == "\n" || line.is_empty() {
                            break;
                        }
                    }
                    writer.write_all(resp.as_bytes()).await.ok();
                });
            }
        });

        (port, handle)
    }

    #[tokio::test]
    async fn send_with_retry_success_on_first_attempt() {
        let ok_response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
        let (port, _handle) = spawn_mock_server(vec![ok_response]).await;

        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/test");

        let result = send_with_retry("test", 3, None, || {
            let req = client.get(&url).build().unwrap();
            let c = client.clone();
            async move { c.execute(req).await }
        })
        .await;

        assert!(result.is_ok(), "expected Ok, got: {result:?}");
        assert_eq!(result.unwrap().status(), 200);
    }

    #[tokio::test]
    async fn send_with_retry_exhausts_retries_returns_rate_limited() {
        // All responses are 429 with Retry-After: 0 to not slow down the test
        let rate_limit_response =
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 0\r\n\r\n";
        let (port, _handle) =
            spawn_mock_server(vec![rate_limit_response, rate_limit_response]).await;

        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/test");

        // max_retries=1 means: attempt 0 (429 → retry), attempt 1 (429 → fail)
        let result = send_with_retry("test", 1, None, || {
            let req = client.get(&url).build().unwrap();
            let c = client.clone();
            async move { c.execute(req).await }
        })
        .await;

        assert!(
            matches!(result, Err(LlmError::RateLimited)),
            "expected RateLimited, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn send_with_retry_succeeds_after_one_429() {
        let rate_limit_response =
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 0\r\n\r\n";
        let ok_response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";

        let (port, _handle) = spawn_mock_server(vec![rate_limit_response, ok_response]).await;

        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/test");

        let result = send_with_retry("test", 2, None, || {
            let req = client.get(&url).build().unwrap();
            let c = client.clone();
            async move { c.execute(req).await }
        })
        .await;

        assert!(
            result.is_ok(),
            "expected Ok after one retry, got: {result:?}"
        );
        assert_eq!(result.unwrap().status(), 200);
    }

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn retry_delay_range_always_valid(attempt in 0u32..63) {
            // Verify exponential backoff stays within u64 range for all valid shift amounts.
            // attempt < 63 guarantees BASE_BACKOFF_SECS << attempt fits in u64.
            let delay = Duration::from_secs(BASE_BACKOFF_SECS << attempt);
            assert!(delay.as_secs() >= BASE_BACKOFF_SECS, "delay must be at least base backoff");
            // Exponential growth: each step doubles
            if attempt > 0 {
                let prev = Duration::from_secs(BASE_BACKOFF_SECS << (attempt - 1));
                assert_eq!(delay.as_secs(), prev.as_secs() * 2);
            }
        }
    }
}
