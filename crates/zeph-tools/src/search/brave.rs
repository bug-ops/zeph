// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Brave Search API backend (v1 default).

use serde::Deserialize;
use url::Url;
use zeph_common::secret::Secret;
use zeph_config::tools::SearchConfig;

use super::provider::{SearchError, SearchProvider, SearchResult};

/// HTTP header Brave uses for the API key.
const SUBSCRIPTION_TOKEN_HEADER: &str = "X-Subscription-Token";

/// Brave Search API backend.
///
/// Issues `GET {endpoint}?q={query}&count={limit}` with the API key in the
/// `X-Subscription-Token` header, and parses the `web.results[]` array of the JSON
/// response into [`SearchResult`]s.
#[derive(Debug)]
pub struct BraveSearchProvider {
    endpoint: Url,
    api_key: Secret,
    /// Response-body size cap, from `[tools.scrape].max_body_bytes` (the search tool has
    /// no independent body-size config). Mirrors `scrape.rs`'s `max_body_bytes` guard.
    max_body_bytes: usize,
}

impl BraveSearchProvider {
    /// Build a provider from config, a response-body size cap, and a resolved API key.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::MissingApiKey`] when `api_key` is `None`, or
    /// [`SearchError::Provider`] when `cfg.endpoint` is not a valid URL.
    pub fn new(
        cfg: &SearchConfig,
        max_body_bytes: usize,
        api_key: Option<Secret>,
    ) -> Result<Self, SearchError> {
        let api_key = api_key.ok_or(SearchError::MissingApiKey { backend: "brave" })?;
        let endpoint = Url::parse(&cfg.endpoint)
            .map_err(|e| SearchError::Provider(format!("invalid search endpoint: {e}")))?;
        Ok(Self {
            endpoint,
            api_key,
            max_body_bytes,
        })
    }
}

impl SearchProvider for BraveSearchProvider {
    async fn search(
        &self,
        client: &reqwest::Client,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, SearchError> {
        // Query parameters are appended manually (rather than via `RequestBuilder::query`)
        // to avoid pulling in reqwest's `query` cargo feature for a single call site.
        let mut request_url = self.endpoint.clone();
        request_url
            .query_pairs_mut()
            .append_pair("q", query)
            .append_pair("count", &limit.to_string());

        let response = client
            .get(request_url)
            .header(SUBSCRIPTION_TOKEN_HEADER, self.api_key.expose())
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    SearchError::Timeout
                } else {
                    SearchError::Provider(e.to_string())
                }
            })?;

        let status = response.status();
        // M1: HTTP 429 (quota exhaustion) is a permanent, non-retryable block — never
        // treated as a transient error, to avoid hammering an already-exhausted quota.
        if status.as_u16() == 429 {
            return Err(SearchError::Blocked {
                reason: "rate limited".to_owned(),
            });
        }
        if !status.is_success() {
            return Err(SearchError::Http {
                status: status.as_u16(),
                message: status.canonical_reason().unwrap_or("unknown").to_owned(),
            });
        }

        let bytes = response
            .bytes()
            .await
            .map_err(|e| SearchError::Provider(e.to_string()))?;
        if bytes.len() > self.max_body_bytes {
            return Err(SearchError::Provider(format!(
                "response too large: {} bytes (max: {})",
                bytes.len(),
                self.max_body_bytes,
            )));
        }
        let body: BraveSearchResponse =
            serde_json::from_slice(&bytes).map_err(|e| SearchError::Parse(e.to_string()))?;

        Ok(body
            .web
            .map(|w| w.results)
            .unwrap_or_default()
            .into_iter()
            .take(limit)
            .map(|r| SearchResult {
                title: r.title,
                url: r.url,
                snippet: r.description.unwrap_or_default(),
            })
            .collect())
    }

    fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    fn name(&self) -> &'static str {
        "brave"
    }
}

#[derive(Debug, Deserialize)]
struct BraveSearchResponse {
    #[serde(default)]
    web: Option<BraveWebResults>,
}

#[derive(Debug, Deserialize)]
struct BraveWebResults {
    #[serde(default)]
    results: Vec<BraveResult>,
}

#[derive(Debug, Deserialize)]
struct BraveResult {
    title: String,
    url: String,
    #[serde(default)]
    description: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_without_key_errors() {
        let cfg = SearchConfig::default();
        let err = BraveSearchProvider::new(&cfg, 1_048_576, None).unwrap_err();
        assert!(matches!(
            err,
            SearchError::MissingApiKey { backend: "brave" }
        ));
    }

    #[test]
    fn new_with_key_succeeds() {
        let cfg = SearchConfig::default();
        let provider = BraveSearchProvider::new(&cfg, 1_048_576, Some(Secret::new("k"))).unwrap();
        assert_eq!(provider.name(), "brave");
        assert_eq!(provider.endpoint().host_str(), Some("api.search.brave.com"));
    }

    #[test]
    fn new_invalid_endpoint_errors() {
        let cfg = SearchConfig {
            endpoint: "not a url".to_owned(),
            ..SearchConfig::default()
        };
        let err = BraveSearchProvider::new(&cfg, 1_048_576, Some(Secret::new("k"))).unwrap_err();
        assert!(matches!(err, SearchError::Provider(_)));
    }

    #[test]
    fn parse_response_with_results() {
        let json = r#"{"web":{"results":[
            {"title":"A","url":"https://a.example","description":"desc a"},
            {"title":"B","url":"https://b.example"}
        ]}}"#;
        let parsed: BraveSearchResponse = serde_json::from_str(json).unwrap();
        let results = parsed.web.unwrap().results;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "A");
        assert_eq!(results[0].description.as_deref(), Some("desc a"));
        assert_eq!(results[1].description, None);
    }

    #[test]
    fn parse_response_missing_web_key() {
        let parsed: BraveSearchResponse = serde_json::from_str("{}").unwrap();
        assert!(parsed.web.is_none());
    }

    // --- BraveSearchProvider::search: wiremock HTTP server tests ---
    //
    // Mirrors `scrape.rs`'s `mock_server_executor`/`server_host_and_addr` pattern: the
    // provider's `endpoint` is pointed at a local wiremock server so `search()` can be
    // exercised end-to-end without SSRF concerns (SSRF/addr-pinning is the executor's
    // responsibility, tested separately in `search/mod.rs`).

    fn provider_for(server: &wiremock::MockServer, max_body_bytes: usize) -> BraveSearchProvider {
        let cfg = SearchConfig {
            endpoint: format!("{}/search", server.uri()),
            ..SearchConfig::default()
        };
        BraveSearchProvider::new(&cfg, max_body_bytes, Some(Secret::new("test-key"))).unwrap()
    }

    #[tokio::test]
    async fn search_golden_path_returns_parsed_results() {
        use wiremock::matchers::{header, method, path, query_param};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search"))
            .and(query_param("q", "rust async"))
            .and(query_param("count", "5"))
            .and(header("X-Subscription-Token", "test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"web":{"results":[
                    {"title":"Rust","url":"https://rust-lang.org","description":"A systems language"}
                ]}}"#,
            ))
            .mount(&server)
            .await;

        let provider = provider_for(&server, 1_048_576);
        let client = reqwest::Client::new();
        let results = provider.search(&client, "rust async", 5).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Rust");
        assert_eq!(results[0].url, "https://rust-lang.org");
        assert_eq!(results[0].snippet, "A systems language");
    }

    #[tokio::test]
    async fn search_429_maps_to_blocked() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let provider = provider_for(&server, 1_048_576);
        let client = reqwest::Client::new();
        let err = provider.search(&client, "quota test", 5).await.unwrap_err();
        assert!(matches!(err, SearchError::Blocked { .. }));
    }

    #[tokio::test]
    async fn search_non_2xx_maps_to_http_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let provider = provider_for(&server, 1_048_576);
        let client = reqwest::Client::new();
        let err = provider.search(&client, "test", 5).await.unwrap_err();
        assert!(matches!(err, SearchError::Http { status: 503, .. }));
    }

    #[tokio::test]
    async fn search_oversized_body_rejected() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        // Body well-formed JSON but larger than the tiny cap below.
        let big_snippet = "x".repeat(200);
        let body = format!(
            r#"{{"web":{{"results":[{{"title":"A","url":"https://a.example","description":"{big_snippet}"}}]}}}}"#
        );
        Mock::given(method("GET"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let provider = provider_for(&server, 32); // cap well below the actual body size
        let client = reqwest::Client::new();
        let err = provider.search(&client, "test", 5).await.unwrap_err();
        assert!(matches!(err, SearchError::Provider(msg) if msg.contains("too large")));
    }

    #[tokio::test]
    async fn search_malformed_json_maps_to_parse_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let provider = provider_for(&server, 1_048_576);
        let client = reqwest::Client::new();
        let err = provider.search(&client, "test", 5).await.unwrap_err();
        assert!(matches!(err, SearchError::Parse(_)));
    }
}
