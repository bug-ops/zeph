// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! HTTP transport for the Cocoon sidecar.
//!
//! [`CocoonClient`] wraps a `reqwest::Client` and handles health checks,
//! model listing, and raw POST forwarding to the localhost sidecar.
//! All network calls are bounded by the `reqwest::Client` timeout configured at construction.
//!
//! The `access_hash` field is never exposed in `Debug` output or tracing spans.

use std::time::Duration;

use serde::Deserialize;
use tracing::Instrument as _;

use crate::error::LlmError;
use crate::provider::StatusTx;
use crate::retry::send_with_retry;

/// Bounded retry count for transient 429/503 responses from the sidecar,
/// matching the Claude/OpenAI/Gemini `send_with_retry` convention.
const MAX_RETRIES: u32 = 3;

/// Health status parsed from the sidecar `/stats` endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct CocoonHealth {
    /// Whether the sidecar has an active connection to a Cocoon proxy.
    #[serde(default)]
    pub proxy_connected: bool,
    /// Number of TEE workers available through the proxy.
    #[serde(default)]
    pub worker_count: u32,
    /// TON wallet balance in TON units. `None` when the sidecar does not report balance.
    #[serde(default)]
    pub ton_balance: Option<f64>,
}

/// HTTP client for the Cocoon C++ sidecar.
///
/// All methods communicate exclusively with the configured `base_url` (expected
/// to be `localhost`); the sidecar handles RA-TLS attestation, proxy selection,
/// and TON payments transparently.
///
/// The `access_hash` field is intentionally excluded from `Debug` output to
/// prevent accidental secret exposure in logs and traces.
///
/// # Examples
///
/// ```no_run
/// use std::time::Duration;
/// use zeph_llm::cocoon::CocoonClient;
///
/// let client = CocoonClient::new("http://localhost:10000", None, Duration::from_secs(30));
/// ```
pub struct CocoonClient {
    base_url: String,
    /// Optional access hash attached to outgoing requests as `X-Access-Hash`.
    ///
    /// Intentionally not in `Debug` output — never log or trace this value.
    access_hash: Option<String>,
    client: reqwest::Client,
    timeout: Duration,
}

impl std::fmt::Debug for CocoonClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CocoonClient")
            .field("base_url", &self.base_url)
            .field(
                "access_hash",
                &self.access_hash.as_ref().map(|_| "<redacted>"),
            )
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl CocoonClient {
    /// Construct a new client. Does not perform I/O.
    ///
    /// - `base_url` — sidecar HTTP address (e.g. `"http://localhost:10000"`).
    ///   Trailing slashes are stripped.
    /// - `access_hash` — optional access hash resolved from the age vault.
    ///   Attached as `X-Access-Hash` header when `Some`.
    /// - `timeout` — per-request deadline wrapping every outbound `.await`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::time::Duration;
    /// use zeph_llm::cocoon::CocoonClient;
    ///
    /// let client = CocoonClient::new("http://localhost:10000", None, Duration::from_secs(30));
    /// ```
    #[must_use]
    pub fn new(
        base_url: impl Into<String>,
        access_hash: Option<String>,
        timeout: Duration,
    ) -> Self {
        let mut url = base_url.into();
        while url.ends_with('/') {
            url.pop();
        }
        // Use exact timeout; reqwest's built-in timeout covers the full request lifecycle
        // (connect + send + body read), so no separate tokio::time::timeout wrapping is needed.
        let client = crate::http::llm_client(timeout.as_secs());
        Self {
            base_url: url,
            access_hash,
            client,
            timeout,
        }
    }

    /// Query `GET /stats` and return sidecar health status.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Unavailable`] if the sidecar is unreachable or the request times out.
    pub async fn health_check(&self) -> Result<CocoonHealth, LlmError> {
        async {
            let url = format!("{}/stats", self.base_url);
            let response = self.client.get(&url).send().await.map_err(|e| {
                tracing::warn!(error = %e, "cocoon sidecar unreachable");
                LlmError::Unavailable
            })?;

            let text = response.text().await.map_err(LlmError::Http)?;
            let health: CocoonHealth = serde_json::from_str(&text)?;
            tracing::debug!(
                proxy_connected = health.proxy_connected,
                worker_count = health.worker_count,
                "done"
            );
            Ok(health)
        }
        .instrument(tracing::info_span!("llm.cocoon.health"))
        .await
    }

    /// Query `GET /v1/models` and return the list of model ID strings.
    ///
    /// Parses the OpenAI-format `/v1/models` response: `{ "data": [{"id": "..."}] }`.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Unavailable`] if the sidecar is unreachable or the request times out.
    pub async fn list_models(&self) -> Result<Vec<String>, LlmError> {
        async {
            let url = format!("{}/v1/models", self.base_url);
            let response = self
                .client
                .get(&url)
                .send()
                .await
                .map_err(|_| LlmError::Unavailable)?;

            let text = response.text().await.map_err(LlmError::Http)?;
            let parsed: ModelsResponse = serde_json::from_str(&text)?;
            Ok(parsed.data.into_iter().map(|m| m.id).collect())
        }
        .instrument(tracing::info_span!("llm.cocoon.models"))
        .await
    }

    /// POST a multipart form to `{base_url}{path}`, retrying transient 429/503 responses.
    ///
    /// Used for audio transcription (`/v1/audio/transcriptions`). Attaches
    /// `X-Access-Hash` header when `access_hash` is `Some`. The request is
    /// bounded by the same LLM timeout configured at construction.
    ///
    /// [`reqwest::multipart::Form`] cannot be cloned or reused across retries, so
    /// `build_form` is invoked once per attempt (up to `MAX_RETRIES` retries) to
    /// construct a fresh form.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Http`] on connection failure, or once retries against repeated
    /// 429/503 responses are exhausted: [`LlmError::RateLimited`] if the last response was
    /// 429, or [`LlmError::Unavailable`] if it was 503.
    pub async fn post_multipart<F>(
        &self,
        path: &str,
        status_tx: Option<&StatusTx>,
        mut build_form: F,
    ) -> Result<reqwest::Response, LlmError>
    where
        F: FnMut() -> Result<reqwest::multipart::Form, reqwest::Error>,
    {
        let span = tracing::info_span!("llm.cocoon.request", path);
        async {
            let url = format!("{}{path}", self.base_url);
            send_with_retry("cocoon", MAX_RETRIES, status_tx, || {
                // Build synchronously so the `FnMut` borrow of `build_form` doesn't
                // need to escape into the returned future.
                let form_result = build_form();
                async {
                    let form = form_result?;
                    let mut req = self.client.post(&url).multipart(form);
                    if let Some(ref hash) = self.access_hash {
                        req = req.header("X-Access-Hash", hash.as_str());
                    }
                    req.send().await
                }
            })
            .await
            .inspect_err(|e| {
                tracing::warn!(error = %e, "cocoon multipart HTTP error");
            })
        }
        .instrument(span)
        .await
    }

    /// POST `body` to `{base_url}{path}`, retrying transient 429/503 responses.
    ///
    /// Attaches `X-Access-Hash` header when `access_hash` is `Some`.
    /// The full request lifecycle (connect + send + body read) is bounded by the
    /// `reqwest::Client` timeout configured at construction time.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Http`] on connection failure, or once retries against repeated
    /// 429/503 responses are exhausted: [`LlmError::RateLimited`] if the last response was
    /// 429, or [`LlmError::Unavailable`] if it was 503.
    pub async fn post(
        &self,
        path: &str,
        body: &[u8],
        status_tx: Option<&StatusTx>,
    ) -> Result<reqwest::Response, LlmError> {
        let span = tracing::info_span!("llm.cocoon.request", path);
        async {
            let url = format!("{}{path}", self.base_url);

            send_with_retry("cocoon", MAX_RETRIES, status_tx, || {
                let mut req = self
                    .client
                    .post(&url)
                    .header("Content-Type", "application/json")
                    .body(body.to_vec());

                if let Some(ref hash) = self.access_hash {
                    req = req.header("X-Access-Hash", hash.as_str());
                }

                req.send()
            })
            .await
            .inspect_err(|e| {
                // MINOR-1: log SSE/mid-stream drops as cocoon-specific so they are
                // distinguishable from OpenAI failures in traces.
                tracing::warn!(error = %e, "cocoon HTTP error (may be mid-stream drop)");
            })
        }
        .instrument(span)
        .await
    }
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}
