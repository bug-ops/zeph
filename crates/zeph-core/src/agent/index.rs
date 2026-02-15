use super::{Agent, Channel, LlmProvider, ToolExecutor};

impl<P: LlmProvider + Clone + 'static, C: Channel, T: ToolExecutor> Agent<P, C, T> {
    pub(super) async fn inject_code_rag(
        &mut self,
        query: &str,
        token_budget: usize,
    ) -> Result<(), super::error::AgentError> {
        let Some(retriever) = &self.index.retriever else {
            return Ok(());
        };
        if token_budget == 0 {
            return Ok(());
        }

        let result = retriever
            .retrieve(query, token_budget)
            .await
            .map_err(|e| super::error::AgentError::Other(format!("{e:#}")))?;
        let context_text = zeph_index::retriever::format_as_context(&result);

        if !context_text.is_empty() {
            self.inject_code_context(&context_text);
            tracing::debug!(
                strategy = ?result.strategy,
                chunks = result.chunks.len(),
                tokens = result.total_tokens,
                "code context injected"
            );
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;
    #[allow(clippy::wildcard_imports)]
    use crate::agent::agent_tests::*;

    #[test]
    fn test_repo_map_cache_hit() {
        use std::time::{Duration, Instant};

        let provider = MockProvider::new(vec![]);
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
