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

pub struct AgentRegistry {
    client: reqwest::Client,
    cache: RwLock<HashMap<String, CachedCard>>,
    ttl: Duration,
}

impl AgentRegistry {
    #[must_use]
    pub fn new(client: reqwest::Client, ttl: Duration) -> Self {
        Self {
            client,
            cache: RwLock::new(HashMap::new()),
            ttl,
        }
    }

    /// # Errors
    /// Returns `A2aError::Http` on network failure or `A2aError::Discovery` on non-2xx / parse failure.
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

    /// # Errors
    /// Returns `A2aError` if cached card is stale and re-fetch fails.
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

    pub async fn all(&self) -> Vec<AgentCard> {
        let cache = self.cache.read().await;
        cache.values().map(|e| e.card.clone()).collect()
    }

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
        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_secs(300));
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
        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_secs(300));
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
        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_secs(60));
        let result = registry.discover("http://no-server.invalid").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn multiple_registrations() {
        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_secs(300));
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
        let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_secs(300));
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
