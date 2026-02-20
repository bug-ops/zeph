//! Shared HTTP client construction for consistent timeout and TLS configuration.

use std::time::Duration;

/// Create a shared HTTP client with standard Zeph configuration.
///
/// Config: 30s connect timeout, 60s request timeout, rustls TLS,
/// `zeph/{version}` user-agent, redirect limit 10.
#[must_use]
pub fn default_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(60))
        .user_agent(concat!("zeph/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .expect("default HTTP client construction must not fail")
}
