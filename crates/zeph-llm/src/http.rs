// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared HTTP client construction for consistent timeout and TLS configuration.

use std::time::Duration;

/// Create an HTTP client for LLM inference providers.
///
/// Connect timeout is fixed at 30s. `request_timeout_secs` is a hard backstop
/// for the full HTTP round-trip; it should be set larger than the agent-level
/// `TimeoutConfig.llm_seconds` so the tokio-layer fires first in normal
/// operation and this only catches runaway requests.
///
/// # Panics
///
/// Panics if the underlying TLS configuration cannot be initialized, which
/// should never happen in a correctly compiled binary.
#[must_use]
pub fn llm_client(request_timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(request_timeout_secs))
        .user_agent(concat!("zeph/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .expect("LLM HTTP client construction must not fail")
}
