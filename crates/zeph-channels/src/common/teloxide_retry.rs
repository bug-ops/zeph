// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared 429 retry-with-backoff helper for teloxide `Bot` requests.
//!
//! [`crate::common::http_retry`] provides this same resilience for the raw `reqwest`-based
//! REST clients (Discord, Slack, [`crate::telegram_api_ext::TelegramApiClient`]). `TelegramChannel`'s
//! primary send path instead goes through teloxide's typed `Bot` API (for `MarkdownV2` parsing,
//! message-id tracking, etc.), which surfaces rate-limiting as
//! [`teloxide::RequestError::RetryAfter`] rather than an HTTP 429 status — this module mirrors
//! `http_retry`'s backoff semantics for that error shape instead.

use std::time::Duration;

use teloxide::requests::{Output, Request};

/// Upper bound applied to any `RetryAfter` duration reported by Telegram.
const MAX_RETRY_SECS: u64 = 60;
/// Maximum number of retry attempts before giving up and surfacing the error.
const MAX_RETRIES: u32 = 3;

/// Sends a teloxide request, retrying with backoff on [`teloxide::RequestError::RetryAfter`].
///
/// Uses [`Request::send_ref`] so the same request value is resent on each attempt rather than
/// rebuilding it. On any other error the failure is returned immediately.
///
/// `context` is a short label (e.g. `"telegram"`) attached to the warning logs.
///
/// # Timing
///
/// Under sustained rate-limiting, the worst-case wall-clock across all attempts is
/// approximately `MAX_RETRIES * MAX_RETRY_SECS` (the backoff sleeps between attempts), on the
/// order of minutes with the current constants — the same shape as
/// [`crate::common::http_retry::send_with_retry`]'s documented worst case. This is deliberate for
/// `TelegramChannel::send`/`flush_chunks` (the actual response content): a real reply is worth
/// retrying to completion rather than dropping. The one caller that cannot tolerate this,
/// `Channel::send_status` (an ephemeral status label with no value after a few seconds), is not
/// bounded here — `zeph_core::channel::Channel::send_status_best_effort` wraps the whole
/// `send_status` call (including any retry loop reached through this function) in its own much
/// shorter, separately-configured timeout at the `Channel` trait level instead of this module
/// applying one internally. Callers of `send`/`flush_chunks` (which do not go through
/// `send_status_best_effort`) intentionally have no outer timeout and may legitimately block for
/// the full worst case above.
///
/// # Errors
///
/// Returns the underlying [`teloxide::RequestError`] when a non-`RetryAfter` error occurs or
/// when retries are exhausted.
pub(crate) async fn send_teloxide_with_retry<R>(
    context: &str,
    req: &R,
) -> Result<Output<R>, teloxide::RequestError>
where
    R: Request<Err = teloxide::RequestError>,
{
    let mut attempts = 0u32;
    loop {
        match req.send_ref().await {
            Ok(value) => return Ok(value),
            Err(teloxide::RequestError::RetryAfter(secs)) => {
                let delay = secs.duration().min(Duration::from_secs(MAX_RETRY_SECS));
                attempts += 1;
                if attempts > MAX_RETRIES {
                    tracing::warn!(
                        context,
                        delay_secs = delay.as_secs(),
                        attempts,
                        "telegram rate-limited and retries exhausted"
                    );
                    return req.send_ref().await;
                }
                tracing::warn!(
                    context,
                    delay_secs = delay.as_secs(),
                    attempt = attempts,
                    max = MAX_RETRIES,
                    "telegram rate-limited (429), backing off"
                );
                tokio::time::sleep(delay).await;
            }
            Err(e) => return Err(e),
        }
    }
}
