// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Hybrid code retrieval: query classification, semantic search, budget packing.

use std::fmt::Write;
use std::sync::Arc;

use crate::error::Result;
use crate::store::{CodeStore, SearchHit};
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider;
use zeph_memory::TokenCounter;

/// Strategy chosen for a particular query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetrievalStrategy {
    /// Vector similarity search — for conceptual queries.
    Semantic,
    /// Exact symbol lookup — retriever returns empty, agent uses grep.
    Grep,
    /// Both semantic search + hint that grep may also help.
    Hybrid,
}

/// Retrieval configuration.
#[derive(Debug, Clone)]
pub struct RetrievalConfig {
    /// Maximum chunks to fetch from `Qdrant` before budget packing.
    pub max_chunks: usize,
    /// Minimum cosine similarity to accept.
    pub score_threshold: f32,
    /// Maximum fraction of available context for code chunks.
    pub budget_ratio: f32,
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            max_chunks: 12,
            score_threshold: 0.25,
            budget_ratio: 0.40,
        }
    }
}

/// Result of a retrieval operation.
#[derive(Debug)]
pub struct RetrievedCode {
    pub chunks: Vec<SearchHit>,
    pub total_tokens: usize,
    pub strategy: RetrievalStrategy,
}

/// Budget-aware code retriever with query classification.
pub struct CodeRetriever {
    store: CodeStore,
    provider: Arc<AnyProvider>,
    config: RetrievalConfig,
    token_counter: Arc<TokenCounter>,
}

impl CodeRetriever {
    #[must_use]
    pub fn new(store: CodeStore, provider: Arc<AnyProvider>, config: RetrievalConfig) -> Self {
        Self {
            store,
            provider,
            config,
            token_counter: Arc::new(TokenCounter::new()),
        }
    }

    /// Retrieve relevant code for a user query.
    ///
    /// # Errors
    ///
    /// Returns an error if embedding or `Qdrant` search fails.
    pub async fn retrieve(&self, query: &str, available_tokens: usize) -> Result<RetrievedCode> {
        let strategy = classify_query(query);

        let token_budget = budget_tokens(available_tokens, self.config.budget_ratio);

        match strategy {
            RetrievalStrategy::Grep => Ok(RetrievedCode {
                chunks: vec![],
                total_tokens: 0,
                strategy,
            }),
            RetrievalStrategy::Semantic | RetrievalStrategy::Hybrid => {
                let chunks = self
                    .semantic_search(query, token_budget, None::<String>)
                    .await?;
                let total_tokens: usize = chunks
                    .iter()
                    .map(|c| self.token_counter.count_tokens(&c.code) + 20)
                    .sum();
                Ok(RetrievedCode {
                    chunks,
                    total_tokens,
                    strategy,
                })
            }
        }
    }

    /// Retrieve with a language filter.
    ///
    /// # Errors
    ///
    /// Returns an error if embedding or `Qdrant` search fails.
    pub async fn retrieve_filtered(
        &self,
        query: &str,
        available_tokens: usize,
        language: &str,
    ) -> Result<RetrievedCode> {
        let strategy = classify_query(query);

        let token_budget = budget_tokens(available_tokens, self.config.budget_ratio);

        let chunks = self
            .semantic_search(query, token_budget, Some(language.to_string()))
            .await?;
        let total_tokens: usize = chunks
            .iter()
            .map(|c| self.token_counter.count_tokens(&c.code) + 20)
            .sum();

        Ok(RetrievedCode {
            chunks,
            total_tokens,
            strategy,
        })
    }

    async fn semantic_search(
        &self,
        query: &str,
        token_budget: usize,
        language_filter: Option<String>,
    ) -> Result<Vec<SearchHit>> {
        let query_vector = self.provider.embed(query).await?;

        let mut hits = self
            .store
            .search(query_vector, self.config.max_chunks, language_filter)
            .await?;

        hits.retain(|h| h.score >= self.config.score_threshold);

        let mut packed = Vec::new();
        let mut used_tokens = 0;

        for hit in hits {
            let cost = self.token_counter.count_tokens(&hit.code) + 20;
            if used_tokens + cost > token_budget {
                break;
            }
            used_tokens += cost;
            packed.push(hit);
        }

        Ok(packed)
    }
}

/// Format retrieved chunks as XML for injection into messages.
#[must_use]
pub fn format_as_context(result: &RetrievedCode) -> String {
    if result.chunks.is_empty() {
        return String::new();
    }

    let mut out = String::from("<code_context>\n");

    for chunk in &result.chunks {
        let name = chunk.entity_name.as_deref().unwrap_or(&chunk.node_type);
        let _ = writeln!(
            out,
            "  <chunk file=\"{}\" lines=\"{}-{}\" name=\"{}\" score=\"{:.2}\">",
            chunk.file_path, chunk.line_range.0, chunk.line_range.1, name, chunk.score,
        );
        out.push_str(&chunk.code);
        out.push_str("\n  </chunk>\n");
    }

    out.push_str("</code_context>");
    out
}

/// Classify user query to pick retrieval strategy.
#[must_use]
pub fn classify_query(query: &str) -> RetrievalStrategy {
    let has_symbol_pattern = query.contains("::")
        || query.contains("fn ")
        || query.contains("struct ")
        || query.contains("impl ")
        || query.contains("trait ")
        || query.contains("mod ")
        || query.contains("class ")
        || query.contains("def ")
        || has_camel_case(query)
        || has_snake_case_identifier(query);

    let has_conceptual = query.contains("how")
        || query.contains("where")
        || query.contains("why")
        || query.contains("find all")
        || query.contains("explain")
        || query.contains("what does")
        || query.contains("show me");

    match (has_symbol_pattern, has_conceptual) {
        (true, true) => RetrievalStrategy::Hybrid,
        (true, false) => RetrievalStrategy::Grep,
        (false, _) => RetrievalStrategy::Semantic,
    }
}

fn has_camel_case(text: &str) -> bool {
    text.split_whitespace().any(|word| {
        let chars: Vec<char> = word.chars().collect();
        chars.len() >= 3
            && chars[0].is_uppercase()
            && chars.iter().any(|c| c.is_lowercase())
            && chars.iter().skip(1).any(|c| c.is_uppercase())
    })
}

fn has_snake_case_identifier(text: &str) -> bool {
    text.split_whitespace().any(|word| {
        word.len() >= 3
            && word.contains('_')
            && word.chars().all(|c| c.is_alphanumeric() || c == '_')
            && word.starts_with(|c: char| c.is_lowercase())
    })
}

fn budget_tokens(available: usize, ratio: f32) -> usize {
    // Scale to per-mille to stay in integer arithmetic.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let per_mille = (ratio * 1000.0) as usize;
    available.saturating_mul(per_mille) / 1000
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SearchHit;

    #[test]
    fn classify_symbol_query_rust() {
        assert_eq!(
            classify_query("find SkillMatcher::match_skills"),
            RetrievalStrategy::Grep
        );
    }

    #[test]
    fn classify_conceptual_query() {
        assert_eq!(
            classify_query("how does skill matching work?"),
            RetrievalStrategy::Semantic
        );
    }

    #[test]
    fn classify_mixed_query() {
        assert_eq!(
            classify_query("where is SkillMatcher used?"),
            RetrievalStrategy::Hybrid
        );
    }

    #[test]
    fn classify_default_is_semantic() {
        assert_eq!(classify_query("help"), RetrievalStrategy::Semantic);
    }

    #[test]
    fn classify_snake_case_identifier() {
        assert_eq!(classify_query("my_function"), RetrievalStrategy::Grep);
    }

    #[test]
    fn camel_case_detection() {
        assert!(has_camel_case("HttpClient"));
        assert!(has_camel_case("find MyStruct"));
        assert!(!has_camel_case("simple word"));
        assert!(!has_camel_case("HTTP"));
        assert!(!has_camel_case("ab"));
    }

    #[test]
    fn snake_case_detection() {
        assert!(has_snake_case_identifier("my_function"));
        assert!(has_snake_case_identifier("call some_method here"));
        assert!(!has_snake_case_identifier("NoSnake"));
        assert!(has_snake_case_identifier("a_b"));
    }

    #[test]
    fn format_as_context_empty() {
        let result = RetrievedCode {
            chunks: vec![],
            total_tokens: 0,
            strategy: RetrievalStrategy::Semantic,
        };
        assert_eq!(format_as_context(&result), "");
    }

    #[test]
    fn format_as_context_xml() {
        let result = RetrievedCode {
            chunks: vec![SearchHit {
                code: "fn hello() {}".to_string(),
                file_path: "src/lib.rs".to_string(),
                line_range: (1, 3),
                score: 0.85,
                node_type: "function_item".to_string(),
                entity_name: Some("hello".to_string()),
                scope_chain: String::new(),
            }],
            total_tokens: 10,
            strategy: RetrievalStrategy::Semantic,
        };
        let xml = format_as_context(&result);
        assert!(xml.contains("<code_context>"));
        assert!(xml.contains("</code_context>"));
        assert!(xml.contains("file=\"src/lib.rs\""));
        assert!(xml.contains("name=\"hello\""));
        assert!(xml.contains("score=\"0.85\""));
        assert!(xml.contains("fn hello() {}"));
    }

    #[test]
    fn snake_case_a_b_three_chars_passes() {
        assert!(has_snake_case_identifier("a_b"));
    }

    #[test]
    fn budget_tokens_ratio_zero() {
        assert_eq!(budget_tokens(10_000, 0.0), 0);
    }

    #[test]
    fn budget_tokens_ratio_one() {
        assert_eq!(budget_tokens(10_000, 1.0), 10_000);
    }

    #[test]
    fn budget_tokens_ratio_half() {
        assert_eq!(budget_tokens(8_000, 0.5), 4_000);
    }

    #[test]
    fn budget_tokens_zero_available() {
        assert_eq!(budget_tokens(0, 0.4), 0);
    }

    #[test]
    fn format_as_context_uses_node_type_when_no_entity_name() {
        let result = RetrievedCode {
            chunks: vec![SearchHit {
                code: "struct Foo {}".to_string(),
                file_path: "src/foo.rs".to_string(),
                line_range: (1, 2),
                score: 0.75,
                node_type: "struct_item".to_string(),
                entity_name: None,
                scope_chain: String::new(),
            }],
            total_tokens: 5,
            strategy: RetrievalStrategy::Semantic,
        };
        let xml = format_as_context(&result);
        assert!(xml.contains("name=\"struct_item\""));
    }

    #[test]
    fn classify_fn_keyword_is_grep() {
        assert_eq!(classify_query("fn my_func"), RetrievalStrategy::Grep);
    }

    #[test]
    fn classify_struct_keyword_is_grep() {
        assert_eq!(classify_query("struct MyType"), RetrievalStrategy::Grep);
    }

    #[test]
    fn classify_explain_conceptual_is_semantic() {
        assert_eq!(
            classify_query("explain the architecture"),
            RetrievalStrategy::Semantic
        );
    }

    #[test]
    fn retrieval_strategy_debug() {
        assert_eq!(format!("{:?}", RetrievalStrategy::Semantic), "Semantic");
        assert_eq!(format!("{:?}", RetrievalStrategy::Grep), "Grep");
        assert_eq!(format!("{:?}", RetrievalStrategy::Hybrid), "Hybrid");
    }

    #[test]
    fn retrieval_config_defaults() {
        let cfg = RetrievalConfig::default();
        assert_eq!(cfg.max_chunks, 12);
        assert!(cfg.score_threshold > 0.0);
        assert!(cfg.budget_ratio > 0.0 && cfg.budget_ratio < 1.0);
    }
}
