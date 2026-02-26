// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared HTTP client construction for consistent timeout and TLS configuration.

use std::time::Duration;

/// Create an HTTP client for LLM inference providers.
///
/// Uses only a connect timeout — no request timeout — because LLM responses
/// can exceed any fixed wall-clock limit (large context, tool use, streaming).
#[must_use]
pub fn llm_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .user_agent(concat!("zeph/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .expect("LLM HTTP client construction must not fail")
}
