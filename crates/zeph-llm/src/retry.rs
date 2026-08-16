// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::future::Future;
use std::time::{Duration, Instant};

use crate::error::LlmError;
use crate::provider::StatusTx;
use crate::usage::UsageTracker;

const BASE_BACKOFF_SECS: u64 = 1;

/// Exponential backoff delay for a given attempt, doubling each retry from `BASE_BACKOFF_SECS`.
///
/// Shared by [`retry_delay`] (used when a `Retry-After` header is unavailable) and by
/// `ollama.rs`'s `send_with_transport_retry`, which retries transient transport-level
/// failures (timeout, connection reset) that occur before any HTTP response is received to
/// read a `Retry-After` header from.
pub(crate) fn exponential_backoff_delay(attempt: u32) -> Duration {
    Duration::from_secs(BASE_BACKOFF_SECS << attempt)
}

/// Parse the `Retry-After` header value as seconds, falling back to exponential backoff.
pub(crate) fn retry_delay(response: &reqwest::Response, attempt: u32) -> Duration {
    if let Some(val) = response.headers().get("retry-after")
        && let Ok(s) = val.to_str()
        && let Ok(secs) = s.parse::<u64>()
    {
        return Duration::from_secs(secs);
    }
    exponential_backoff_delay(attempt)
}

/// Send an HTTP request, retrying up to `max_retries` times on 429 or 503 responses.
///
/// `f` must return a `reqwest::Response`. On each rate-limited or unavailable attempt,
/// emits a status message and waits before retrying. Returns the successful `Response`
/// for further processing by the caller, or an error.
///
/// When `usage` is `Some`, records a time-to-first-byte sample (milliseconds, measured
/// around each attempt's `f().await`) into it on every attempt. Because retried attempts
/// overwrite the previous sample, the value left behind when this function returns always
/// belongs to the attempt whose response was actually used — never inflated by the
/// backoff sleeps or failed attempts that preceded it (issue #6549, D-S2: measuring from
/// outside this loop would collapse `ttft_ms` to `latency_ms` on any retried call).
///
/// # Errors
///
/// If all attempts are exhausted, returns `LlmError::RateLimited` when the last response
/// was `429 Too Many Requests`, or `LlmError::Unavailable` when it was `503 Service
/// Unavailable`. The underlying `reqwest::Error` is wrapped as `LlmError::Http` for other
/// failures.
pub(crate) async fn send_with_retry<F, Fut>(
    provider_name: &str,
    max_retries: u32,
    status_tx: Option<&StatusTx>,
    usage: Option<&UsageTracker>,
    mut f: F,
) -> Result<reqwest::Response, LlmError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<reqwest::Response, reqwest::Error>>,
{
    for attempt in 0..=max_retries {
        let send_start = Instant::now();
        let response = f().await.map_err(LlmError::Http)?;
        if let Some(tracker) = usage {
            let ms = u64::try_from(send_start.elapsed().as_millis()).unwrap_or(u64::MAX);
            tracker.record_ttft(ms);
        }
        let status = response.status();

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
        {
            if attempt == max_retries {
                return Err(if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
                    LlmError::Unavailable
                } else {
                    LlmError::RateLimited
                });
            }
            let delay = retry_delay(&response, attempt);
            let msg = format!(
                "{provider_name} rate limited or unavailable, retrying in {}s ({}/{})",
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
        assert_eq!(BASE_BACKOFF_SECS, 1);
        assert_eq!(BASE_BACKOFF_SECS << 1, 2);
        assert_eq!(BASE_BACKOFF_SECS << 2, 4);
    }

    /// Spawn a minimal HTTP server that returns a fixed response for each connection.
    /// Returns `(port, join_handle)`.
    async fn spawn_mock_server(responses: Vec<&'static str>) -> (u16, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server_loop = async move {
            for resp in responses {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let conn = async move {
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
                };
                tokio::spawn(conn); // EXEMPT: test-only per-connection handler inside mock server
            }
        };
        let handle = tokio::spawn(server_loop); // EXEMPT: test-only mock server; handle returned to caller

        (port, handle)
    }

    #[tokio::test]
    async fn send_with_retry_success_on_first_attempt() {
        let ok_response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
        let (port, _handle) = spawn_mock_server(vec![ok_response]).await;

        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/test");

        let result = send_with_retry("test", 3, None, None, || {
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
        let result = send_with_retry("test", 1, None, None, || {
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
    async fn send_with_retry_exhausts_retries_returns_unavailable() {
        // All responses are 503 with Retry-After: 0 to not slow down the test
        let unavailable_response =
            "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nContent-Length: 0\r\n\r\n";
        let (port, _handle) =
            spawn_mock_server(vec![unavailable_response, unavailable_response]).await;

        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/test");

        // max_retries=1 means: attempt 0 (503 → retry), attempt 1 (503 → fail)
        let result = send_with_retry("test", 1, None, None, || {
            let req = client.get(&url).build().unwrap();
            let c = client.clone();
            async move { c.execute(req).await }
        })
        .await;

        assert!(
            matches!(result, Err(LlmError::Unavailable)),
            "expected Unavailable, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn send_with_retry_mixed_429_then_503_returns_unavailable() {
        // attempt 0: 429 (retry), attempt 1: 503 (retry), attempt 2: 503 (exhausted).
        // The LAST status seen (503), not the first (429), must decide the error variant.
        let rate_limit_response =
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 0\r\n\r\n";
        let unavailable_response =
            "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nContent-Length: 0\r\n\r\n";
        let (port, _handle) = spawn_mock_server(vec![
            rate_limit_response,
            unavailable_response,
            unavailable_response,
        ])
        .await;

        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/test");

        let result = send_with_retry("test", 2, None, None, || {
            let req = client.get(&url).build().unwrap();
            let c = client.clone();
            async move { c.execute(req).await }
        })
        .await;

        assert!(
            matches!(result, Err(LlmError::Unavailable)),
            "last status (503) must win over the earlier 429, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn send_with_retry_mixed_503_then_429_returns_rate_limited() {
        // attempt 0: 503 (retry), attempt 1: 429 (retry), attempt 2: 429 (exhausted).
        // The LAST status seen (429), not the first (503), must decide the error variant.
        let rate_limit_response =
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 0\r\n\r\n";
        let unavailable_response =
            "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nContent-Length: 0\r\n\r\n";
        let (port, _handle) = spawn_mock_server(vec![
            unavailable_response,
            rate_limit_response,
            rate_limit_response,
        ])
        .await;

        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/test");

        let result = send_with_retry("test", 2, None, None, || {
            let req = client.get(&url).build().unwrap();
            let c = client.clone();
            async move { c.execute(req).await }
        })
        .await;

        assert!(
            matches!(result, Err(LlmError::RateLimited)),
            "last status (429) must win over the earlier 503, got: {result:?}"
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

        let result = send_with_retry("test", 2, None, None, || {
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

    /// D-S2 (issue #6549): a retried call's `ttft_ms` must reflect only the successful
    /// attempt's send time, never the earlier failed attempt plus the backoff sleep between
    /// them. Uses a 1-second `Retry-After` so a caller-side (outside-the-loop) measurement
    /// would be forced to at least ~1000ms; the fix must stay far below that.
    ///
    /// #6737: NOT converted to `start_paused` — `send_start` in `send_with_retry` (this file,
    /// above) is `std::time::Instant`, which does not track the paused virtual clock. Under a
    /// paused clock the 1s backoff costs ~0 real time regardless of whether ttft is measured
    /// inside or outside the retry loop, so a reintroduction of the #6549 bug would still read
    /// `ttft < 500` and this assertion would pass either way — vacuous. Left on real time.
    #[tokio::test]
    async fn send_with_retry_ttft_reflects_final_attempt_not_backoff_delay() {
        let rate_limit_response =
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nContent-Length: 0\r\n\r\n";
        let ok_response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";

        let (port, _handle) = spawn_mock_server(vec![rate_limit_response, ok_response]).await;

        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/test");
        let usage = UsageTracker::default();

        let result = send_with_retry("test", 1, None, Some(&usage), || {
            let req = client.get(&url).build().unwrap();
            let c = client.clone();
            async move { c.execute(req).await }
        })
        .await;

        assert!(
            result.is_ok(),
            "expected Ok after one retry, got: {result:?}"
        );
        let ttft = usage
            .last_ttft_ms()
            .expect("send_with_retry must record a ttft sample when usage is Some");
        assert!(
            ttft < 500,
            "ttft_ms={ttft} must reflect only the final attempt's send time, not the \
             1000ms Retry-After backoff sleep between attempts"
        );
    }

    /// D-S2: every attempt overwrites the tracker, so a call that fails on attempt 0 and
    /// succeeds on attempt 1 must still leave a `Some` value (never lost, never stale-`None`).
    #[tokio::test]
    async fn send_with_retry_records_ttft_on_first_attempt_success_too() {
        let ok_response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
        let (port, _handle) = spawn_mock_server(vec![ok_response]).await;

        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/test");
        let usage = UsageTracker::default();

        assert!(usage.last_ttft_ms().is_none());
        let result = send_with_retry("test", 3, None, Some(&usage), || {
            let req = client.get(&url).build().unwrap();
            let c = client.clone();
            async move { c.execute(req).await }
        })
        .await;

        assert!(result.is_ok());
        assert!(usage.last_ttft_ms().is_some());
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
