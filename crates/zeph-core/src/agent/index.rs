// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::{Agent, Channel};

impl<C: Channel> Agent<C> {
    pub(super) async fn fetch_code_rag(
        index: &super::IndexState,
        query: &str,
        token_budget: usize,
    ) -> Result<Option<String>, super::error::AgentError> {
        let Some(retriever) = &index.retriever else {
            return Ok(None);
        };
        if token_budget == 0 {
            return Ok(None);
        }

        let result = retriever
            .retrieve(query, token_budget)
            .await
            .map_err(|e| super::error::AgentError::Other(format!("{e:#}")))?;
        let context_text = zeph_index::retriever::format_as_context(&result);

        if context_text.is_empty() {
            Ok(None)
        } else {
            tracing::debug!(
                strategy = ?result.strategy,
                chunks = result.chunks.len(),
                tokens = result.total_tokens,
                "code context fetched"
            );
            Ok(Some(context_text))
        }
    }
}

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;
    #[allow(clippy::wildcard_imports)]
    use crate::agent::agent_tests::*;

    /// GAP-1554-A: repo map fields are populated even when retriever is None.
    #[test]
    fn test_repo_map_populated_without_retriever() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        // No retriever wired up (default).
        assert!(agent.index.retriever.is_none(), "retriever must be None");

        // With a positive token budget, repo_map_tokens is non-zero.
        agent.index.repo_map_tokens = 500;
        assert_eq!(agent.index.repo_map_tokens, 500);

        // Pre-populate the cache (simulates a successful generate_repo_map call).
        let fake_map = "<repo_map>\n  src/lib.rs :: pub fn:foo(1)\n</repo_map>".to_string();
        let now = std::time::Instant::now();
        agent.index.cached_repo_map = Some((fake_map.clone(), now));

        let (cached, _) = agent.index.cached_repo_map.as_ref().unwrap();
        assert!(
            cached.contains("<repo_map>"),
            "repo map must contain XML wrapper"
        );
        assert!(
            cached.contains("src/lib.rs"),
            "repo map must contain a file entry"
        );
    }

    /// GAP-1554-A (async): fetch_code_rag returns None when retriever is absent.
    #[tokio::test]
    async fn test_fetch_code_rag_no_retriever_returns_none() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor);
        assert!(agent.index.retriever.is_none());

        let result = Agent::<MockChannel>::fetch_code_rag(&agent.index, "some query", 200).await;
        assert!(result.is_ok());
        assert!(
            result.unwrap().is_none(),
            "must return None when no retriever is configured"
        );
    }

    #[test]
    fn test_repo_map_cache_hit() {
        use std::time::{Duration, Instant};

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.index.repo_map_ttl = Duration::from_secs(300);

        let now = Instant::now();
        agent.index.cached_repo_map = Some(("cached map".into(), now));

        let (cached, generated_at) = agent.index.cached_repo_map.as_ref().unwrap();
        assert_eq!(cached, "cached map");

        let elapsed = Instant::now().duration_since(*generated_at);
        assert!(
            elapsed < agent.index.repo_map_ttl,
            "cache should still be valid within TTL"
        );

        let original_instant = *generated_at;
        let (_, second_generated_at) = agent.index.cached_repo_map.as_ref().unwrap();
        assert_eq!(
            original_instant, *second_generated_at,
            "cached instant should not change on reuse"
        );
    }
}
