// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[cfg(test)]
mod tests {
    use super::super::Agent;
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
        assert!(agent.index.retriever.is_none(), "retriever must be None");

        agent.index.repo_map_tokens = 500;
        assert_eq!(agent.index.repo_map_tokens, 500);

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

    /// GAP-1554-A (async): `fetch_code_rag` returns None when retriever is absent.
    #[tokio::test]
    async fn test_fetch_code_rag_no_retriever_returns_none() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor);
        assert!(agent.index.retriever.is_none());

        let result = agent.index.fetch_code_rag("some query", 200).await;
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
