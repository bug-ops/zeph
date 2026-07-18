// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Search backend contract and dispatch.

use zeph_common::secret::Secret;
use zeph_config::tools::SearchConfig;

use super::brave::BraveSearchProvider;

/// One ranked search result.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchResult {
    /// Result page title, as returned by the search backend.
    pub title: String,
    /// Result page URL. Text only — never auto-fetched by the search tool itself.
    pub url: String,
    /// Short excerpt/snippet from the result page.
    pub snippet: String,
}

/// Search-backend error taxonomy — mapped to [`crate::executor::ToolError`] at the
/// executor boundary; never surfaced raw to the LLM.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SearchError {
    /// No API key configured for a backend that requires one.
    #[error("no API key configured for backend {backend}")]
    MissingApiKey {
        /// Name of the backend that requires a key (see [`SearchProvider::name`]).
        backend: &'static str,
    },
    /// The backend returned a non-success HTTP status.
    #[error("HTTP error {status}: {message}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// Human-readable status/error message.
        message: String,
    },
    /// The request did not complete within the configured timeout.
    #[error("request timed out")]
    Timeout,
    /// The request was blocked by policy (SSRF, denylist, or backend rate limiting).
    #[error("blocked: {reason}")]
    Blocked {
        /// Human-readable block reason.
        reason: String,
        /// HTTP status code that triggered the block, when applicable (e.g. `429` for
        /// backend rate-limiting). `None` for non-HTTP blocks (SSRF, denylist).
        status: Option<u16>,
    },
    /// The backend response body could not be parsed.
    #[error("failed to parse response: {0}")]
    Parse(String),
    /// Catch-all for backend-specific failures not covered by the other variants.
    #[error("provider error: {0}")]
    Provider(String),
}

/// Contract for a query-based web search backend.
///
/// Implementors guarantee they issue at most one HTTPS call per [`search`](Self::search)
/// invocation, to the exact URL returned by [`endpoint`](Self::endpoint), using the
/// `client` handed in by the caller rather than building their own.
///
/// # Design note: caller-supplied client
///
/// [`WebSearchExecutor`](crate::search::WebSearchExecutor) — not the provider — owns SSRF
/// validation (`zeph_common::net::resolve_and_validate`) and addr-pinning
/// (`reqwest::ClientBuilder::resolve_to_addrs`), mirroring `WebScrapeExecutor`. The
/// resulting pinned [`reqwest::Client`] is passed into `search()` so a provider must use it
/// as-is rather than building its own unpinned client (INVARIANT-2, spec 006-1-web-search
/// §3.4/§4) — this is a contract implementors must uphold, not a compile-time guarantee.
///
/// # Examples
///
/// ```rust,no_run
/// use zeph_tools::search::SearchBackend;
/// use zeph_tools::SearchConfig;
///
/// let backend = SearchBackend::from_config(&SearchConfig::default(), 1_048_576, None);
/// assert!(backend.is_err()); // Brave requires an API key
/// ```
pub trait SearchProvider: Send + Sync {
    /// Issue a single search query via `client` and return ranked results.
    ///
    /// `client` is pre-validated and addr-pinned by the caller (see the trait-level
    /// design note) — implementors must use it as-is, never build their own client.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError`] on missing key, HTTP failure, timeout, or response parse
    /// failure.
    fn search(
        &self,
        client: &reqwest::Client,
        query: &str,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<SearchResult>, SearchError>> + Send;

    /// The fixed, operator-configured endpoint this provider calls.
    ///
    /// Used by [`crate::search::WebSearchExecutor`] to run SSRF/denylist validation and
    /// addr-pinning *before* the request is issued.
    fn endpoint(&self) -> &url::Url;

    /// Stable provider name for audit/egress records and error messages (e.g. `"brave"`).
    fn name(&self) -> &'static str;
}

/// Concrete backend dispatch.
///
/// An enum (not `Box<dyn ErasedSearchProvider>`) keeps [`WebSearchExecutor`](crate::search::WebSearchExecutor)
/// non-generic and avoids erased-async-trait boilerplate for a small, closed backend set.
/// New backends are new variants.
#[derive(Debug)]
#[non_exhaustive]
pub enum SearchBackend {
    /// Brave Search API (v1 default backend).
    Brave(BraveSearchProvider),
}

impl SearchBackend {
    /// Resolve a backend from config plus an optional pre-fetched API key.
    ///
    /// `max_body_bytes` caps the response body size a provider will read (from
    /// `[tools.scrape].max_body_bytes` — the search tool has no independent body-size
    /// config, mirroring its `denied_domains`/`ipi_filter_threshold` inheritance).
    ///
    /// Returns `Err` for a keyed backend (e.g. Brave) with no key. A future keyless
    /// backend (e.g. `SearXNG`) must be constructible with `api_key: None` — callers gate
    /// tool availability on this function's success, not on "key present" (see FR-002).
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::MissingApiKey`] when the selected backend requires a key
    /// and none was supplied, or [`SearchError::Provider`] when `cfg.endpoint` is not a
    /// valid URL or `cfg.backend` names an unknown backend.
    pub fn from_config(
        cfg: &SearchConfig,
        max_body_bytes: usize,
        api_key: Option<Secret>,
    ) -> Result<Self, SearchError> {
        match cfg.backend.as_str() {
            "brave" => Ok(Self::Brave(BraveSearchProvider::new(
                cfg,
                max_body_bytes,
                api_key,
            )?)),
            other => Err(SearchError::Provider(format!(
                "unknown search backend: {other}"
            ))),
        }
    }

    /// See [`SearchProvider::search`].
    ///
    /// # Errors
    ///
    /// Returns [`SearchError`] on missing key, HTTP failure, timeout, or response parse
    /// failure.
    pub async fn search(
        &self,
        client: &reqwest::Client,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, SearchError> {
        match self {
            Self::Brave(provider) => provider.search(client, query, limit).await,
        }
    }

    /// See [`SearchProvider::endpoint`].
    #[must_use]
    pub fn endpoint(&self) -> &url::Url {
        match self {
            Self::Brave(provider) => provider.endpoint(),
        }
    }

    /// See [`SearchProvider::name`].
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Brave(provider) => provider.name(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_config_unknown_backend_errors() {
        let cfg = SearchConfig {
            backend: "unknown".to_owned(),
            ..SearchConfig::default()
        };
        let err = SearchBackend::from_config(&cfg, 1_048_576, None).unwrap_err();
        assert!(matches!(err, SearchError::Provider(_)));
    }

    #[test]
    fn from_config_brave_without_key_errors() {
        let cfg = SearchConfig::default();
        let err = SearchBackend::from_config(&cfg, 1_048_576, None).unwrap_err();
        assert!(matches!(
            err,
            SearchError::MissingApiKey { backend: "brave" }
        ));
    }

    #[test]
    fn from_config_brave_with_key_succeeds() {
        let cfg = SearchConfig::default();
        let backend =
            SearchBackend::from_config(&cfg, 1_048_576, Some(Secret::new("test-key"))).unwrap();
        assert_eq!(backend.name(), "brave");
    }
}
