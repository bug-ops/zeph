// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared HTTP client construction for consistent timeout and TLS configuration.

use std::time::Duration;

/// Create an HTTP client for LLM inference providers.
///
/// Connect timeout: 30s.
/// Request timeout: 600s — acts as a hard backstop only. The agent-level
/// `TimeoutConfig.llm_seconds` (default 120s) fires first under normal
/// operation; the 600s limit catches runaway requests if that layer is
/// misconfigured or bypassed.
#[must_use]
pub fn llm_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(600))
        .user_agent(concat!("zeph/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .expect("LLM HTTP client construction must not fail")
}
