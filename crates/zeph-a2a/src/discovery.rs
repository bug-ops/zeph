// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Agent discovery via `/.well-known/agent.json` with TTL-based caching.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

use crate::error::A2aError;
use crate::types::AgentCard;

const WELL_KNOWN_PATH: &str = "/.well-known/agent.json";

struct CachedCard {
    card: AgentCard,
    fetched_at: Instant,
}

/// In-memory registry of peer agent capability cards with TTL-based cache invalidation.
///
/// `AgentRegistry` fetches [`AgentCard`] documents from `{base_url}/.well-known/agent.json`
/// and caches them for up to `ttl`. It supports three usage patterns:
///
/// 1. **Auto-discovery** via [`discover`](Self::discover): always fetches from the network.
/// 2. **Cache-first** via [`get_or_discover`](Self::get_or_discover): returns the cached card
///    if it is younger than `ttl`, otherwise re-fetches.
/// 3. **Manual registration** via [`register`](Self::register): populates the cache directly
///    without a network call (useful for known peers or test fixtures).
///
/// # Examples
///
/// ```rust,no_run
/// use zeph_a2a::AgentRegistry;
/// use std::time::Duration;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(5));
///
/// // Discover and cache a peer agent.
/// let card = registry.discover("http://peer.example.com").await?;
/// println!("Peer agent: {}", card.name);
///
/// // Next call returns the cached version (no network request).
/// let card = registry.get_or_discover("http://peer.example.com").await?;
/// # Ok(())
/// # }
/// ```
pub struct AgentRegistry {
    client: reqwest::Client,
    cache: RwLock<HashMap<String, CachedCard>>,
    ttl: Duration,
}

impl AgentRegistry {
    /// Create a new registry with the given HTTP client and cache TTL.
    ///
    /// All discovered or registered cards are evicted from the cache after `ttl` elapses.
    #[must_use]
    pub fn new(client: reqwest::Client, ttl: Duration) -> Self {
        Self {
            client,
            cache: RwLock::new(HashMap::new()),
            ttl,
        }
    }

    /// Fetch the [`AgentCard`] from `{base_url}/.well-known/agent.json` and update the cache.
    ///
    /// Always performs a network request regardless of the current cache state. The result
    /// is stored under `base_url` so subsequent [`get_or_discover`](Self::get_or_discover)
    /// calls can serve it without re-fetching until the TTL expires.
    ///
    /// # Errors
    ///
    /// Returns [`A2aError`] wrapping an HTTP transport failure, or a [`A2aError`] discovery
    /// variant on non-2xx HTTP status or JSON parse failure.
    pub async fn discover(&self, base_url: &str) -> Result<AgentCard, A2aError> {
        let url = format!("{}{WELL_KNOWN_PATH}", base_url.trim_end_matches('/'));
        let resp = self.client.get(&url).send().await?;

        if !resp.status().is_success() {
            return Err(A2aError::Discovery {
                url,
                reason: format!("HTTP {}", resp.status()),
            });
        }

        let card: AgentCard = resp.json().await.map_err(|e| A2aError::Discovery {
            url,
            reason: e.to_string(),
        })?;

        let mut cache = self.cache.write().await;
        cache.insert(
            base_url.to_owned(),
            CachedCard {
                card: card.clone(),
                fetched_at: Instant::now(),
            },
        );

        Ok(card)
    }

    /// Return a cached [`AgentCard`] if it is still within the TTL, otherwise re-fetch.
    ///
    /// This is the preferred call for high-frequency routing decisions — it avoids a
    /// network round-trip on every call while still refreshing stale cards automatically.
    ///
    /// # Errors
    ///
    /// Returns [`A2aError`] if the cached entry is expired and the re-fetch via
    /// [`discover`](Self::discover) fails.
    pub async fn get_or_discover(&self, base_url: &str) -> Result<AgentCard, A2aError> {
        {
            let cache = self.cache.read().await;
            if let Some(entry) = cache.get(base_url)
                && entry.fetched_at.elapsed() < self.ttl
            {
                return Ok(entry.card.clone());
            }
        }
        self.discover(base_url).await
    }

    /// Manually register an [`AgentCard`] under `base_url`, bypassing the network.
    ///
    /// Overwrites any existing entry for the same URL. The card is treated as freshly
    /// fetched and will not expire until `ttl` has elapsed from the time of this call.
    ///
    /// Useful when the card is already known (e.g., loaded from config) or in tests.
    pub async fn register(&self, base_url: String, card: AgentCard) {
        let mut cache = self.cache.write().await;
        cache.insert(
            base_url,
            CachedCard {
                card,
                fetched_at: Instant::now(),
            },
        );
    }

    /// Return all currently cached [`AgentCard`]s, including stale entries.
    ///
    /// This does not trigger any eviction or re-fetch. Call [`evict_stale`](Self::evict_stale)
    /// first if you only want cards that are still within their TTL.
    pub async fn all(&self) -> Vec<AgentCard> {
        let cache = self.cache.read().await;
        cache.values().map(|e| e.card.clone()).collect()
    }

    /// Remove all cache entries whose TTL has expired.
    ///
    /// Intended for periodic background cleanup. The A2A server does not call this
    /// automatically — callers should schedule it as needed (e.g., via a periodic task).
    pub async fn evict_stale(&self) {
        let mut cache = self.cache.write().await;
        cache.retain(|_, entry| entry.fetched_at.elapsed() < self.ttl);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::AgentCardBuilder;

    fn test_card(name: &str) -> AgentCard {
        AgentCardBuilder::new(name, "http://localhost", "0.1.0")
            .description("test")
            .build()
    }

    #[tokio::test]
    async fn register_and_retrieve() {
        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(5));
        let card = test_card("agent-1");
        registry
            .register("http://localhost:8080".into(), card.clone())
            .await;

        let all = registry.all().await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "agent-1");
    }

    #[tokio::test]
    async fn get_or_discover_returns_cached() {
        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(5));
        let card = test_card("cached");
        registry.register("http://example.com".into(), card).await;

        let result = registry.get_or_discover("http://example.com").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().name, "cached");
    }

    #[tokio::test]
    async fn evict_stale_removes_expired() {
        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_millis(1));
        let card = test_card("stale");
        registry
            .register("http://stale.example.com".into(), card)
            .await;

        tokio::time::sleep(Duration::from_millis(10)).await;
        registry.evict_stale().await;

        let all = registry.all().await;
        assert!(all.is_empty());
    }

    #[tokio::test]
    async fn get_or_discover_refetches_after_ttl_expiry() {
        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_millis(1));
        let card = test_card("expiring");
        registry
            .register("http://no-server.invalid".into(), card)
            .await;

        tokio::time::sleep(Duration::from_millis(10)).await;

        let result = registry.get_or_discover("http://no-server.invalid").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn discover_invalid_url_returns_error() {
        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(1));
        let result = registry.discover("http://no-server.invalid").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn multiple_registrations() {
        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(5));
        registry
            .register("http://a.example.com".into(), test_card("a"))
            .await;
        registry
            .register("http://b.example.com".into(), test_card("b"))
            .await;

        let all = registry.all().await;
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn register_overwrites_existing() {
        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(5));
        registry
            .register("http://a.example.com".into(), test_card("v1"))
            .await;
        registry
            .register("http://a.example.com".into(), test_card("v2"))
            .await;

        let all = registry.all().await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "v2");
    }
}

#[cfg(test)]
mod wiremock_tests {
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::discovery::AgentRegistry;
    use crate::error::A2aError;
    use crate::testing::agent_card_response;

    #[tokio::test]
    async fn discover_success() {
        let server = MockServer::start().await;
        let base_url = server.uri();
        Mock::given(method("GET"))
            .and(path("/.well-known/agent.json"))
            .respond_with(agent_card_response("mock-agent", &base_url))
            .mount(&server)
            .await;

        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(1));
        let card = registry.discover(&base_url).await.unwrap();
        assert_eq!(card.name, "mock-agent");
    }

    #[tokio::test]
    async fn discover_404_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/agent.json"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(1));
        let result = registry.discover(&server.uri()).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), A2aError::Discovery { .. }));
    }

    #[tokio::test]
    async fn discover_invalid_json_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/agent.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not-json"))
            .mount(&server)
            .await;

        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_mins(1));
        let result = registry.discover(&server.uri()).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), A2aError::Discovery { .. }));
    }

    #[tokio::test]
    async fn get_or_discover_refetches_after_ttl() {
        let server = MockServer::start().await;
        let base_url = server.uri();
        Mock::given(method("GET"))
            .and(path("/.well-known/agent.json"))
            .respond_with(agent_card_response("fresh-agent", &base_url))
            .mount(&server)
            .await;

        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_millis(1));
        // Register stale card
        let stale = crate::card::AgentCardBuilder::new("stale", &base_url, "0.0.1").build();
        registry.register(base_url.clone(), stale).await;
        // Wait for TTL to expire
        tokio::time::sleep(Duration::from_millis(10)).await;
        // Should re-fetch from mock server
        let card = registry.get_or_discover(&base_url).await.unwrap();
        assert_eq!(card.name, "fresh-agent");
    }
}
