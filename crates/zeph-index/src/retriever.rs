// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Hybrid code retrieval: query classification, semantic search, budget packing.
//!
//! # Retrieval strategy
//!
//! [`classify_query`] inspects the free-text query for heuristic signals:
//!
//! | Signal | Examples | Strategy |
//! |--------|----------|----------|
//! | Symbol patterns only | `"fn my_fn"`, `"SkillMatcher::match"`, `"my_snake_func"` | [`RetrievalStrategy::Grep`] |
//! | Conceptual patterns only | `"how does auth work?"`, `"explain the retry logic"` | [`RetrievalStrategy::Semantic`] |
//! | Both | `"where is SkillMatcher used?"` | [`RetrievalStrategy::Hybrid`] |
//!
//! For `Grep` queries, [`CodeRetriever::retrieve`] returns an empty chunk list and
//! the agent falls back to its shell grep tool. For `Semantic` and `Hybrid` queries
//! an embedding round-trip is made and the top-scoring Qdrant results are packed
//! within a token budget.
//!
//! # Token budget
//!
//! [`RetrievalConfig::budget_ratio`] controls what fraction of the caller's available
//! context window is allocated to code chunks. The packing loop stops before adding a
//! chunk that would exceed the budget, so the retrieved set always fits the window.

use std::fmt::Write;
use std::sync::Arc;

use crate::error::Result;
use crate::store::{CodeStore, SearchHit};
use zeph_common::{EmbeddingVector, Unnormalized};
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider;
use zeph_memory::TokenCounter;

/// The retrieval strategy selected by [`classify_query`] for a given query.
///
/// # Examples
///
/// ```
/// use zeph_index::retriever::{RetrievalStrategy, classify_query};
///
/// assert_eq!(classify_query("how does authentication work?"), RetrievalStrategy::Semantic);
/// assert_eq!(classify_query("fn my_handler"), RetrievalStrategy::Grep);
/// assert_eq!(classify_query("where is MyHandler used?"), RetrievalStrategy::Hybrid);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetrievalStrategy {
    /// Vector similarity search for conceptual or descriptive queries.
    ///
    /// The query is embedded and the top-K chunks from Qdrant are returned.
    Semantic,
    /// Exact symbol lookup — the retriever returns an empty chunk list.
    ///
    /// The caller (agent) is expected to use a `grep` or `symbol_definition` tool
    /// instead of the vector store for precise symbol lookups.
    Grep,
    /// Both semantic search **and** a hint that grep may also help.
    ///
    /// Semantic results are still returned, but the caller can additionally
    /// perform a textual search for the identified symbol names.
    Hybrid,
}

/// Configuration for [`CodeRetriever`].
///
/// # Examples
///
/// ```
/// use zeph_index::retriever::RetrievalConfig;
///
/// let cfg = RetrievalConfig::default();
/// assert_eq!(cfg.max_chunks, 12);
/// assert!(cfg.score_threshold > 0.0);
/// assert!(cfg.budget_ratio > 0.0 && cfg.budget_ratio < 1.0);
/// ```
#[derive(Debug, Clone)]
pub struct RetrievalConfig {
    /// Maximum number of chunks to fetch from Qdrant before applying score and budget filters.
    pub max_chunks: usize,
    /// Minimum cosine similarity score to accept (chunks below this are dropped).
    pub score_threshold: f32,
    /// Maximum fraction of `available_tokens` allocated to code chunks (0.0–1.0).
    pub budget_ratio: f32,
    /// Maximum seconds to wait for `provider.embed()` before returning
    /// [`crate::error::IndexError::EmbedTimeout`]. Defaults to `10`.
    pub embed_timeout_secs: u64,
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            max_chunks: 12,
            score_threshold: 0.25,
            budget_ratio: 0.40,
            embed_timeout_secs: 10,
        }
    }
}

/// The result of a single retrieval operation.
///
/// Returned by [`CodeRetriever::retrieve`] and [`CodeRetriever::retrieve_filtered`].
/// Pass to [`format_as_context`] to produce an XML snippet for injection into the
/// agent message.
#[derive(Debug)]
pub struct RetrievedCode {
    /// Ordered list of matching chunks (highest score first, budget-capped).
    pub chunks: Vec<SearchHit>,
    /// Estimated total tokens consumed by `chunks` (including a small per-chunk overhead).
    pub total_tokens: usize,
    /// Strategy that was used to produce this result.
    pub strategy: RetrievalStrategy,
}

/// Budget-aware code retriever with automatic query classification.
///
/// Wraps a [`CodeStore`] and an LLM provider (for embedding) and exposes a single
/// high-level [`CodeRetriever::retrieve`] method.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
/// use zeph_index::retriever::{CodeRetriever, RetrievalConfig, format_as_context};
/// use zeph_index::store::CodeStore;
/// # async fn example() -> zeph_index::Result<()> {
/// # let store: CodeStore = panic!("placeholder");
/// # let provider: Arc<zeph_llm::any::AnyProvider> = panic!("placeholder");
///
/// let retriever = CodeRetriever::new(store, provider, RetrievalConfig::default());
/// let result = retriever.retrieve("explain how authentication works", 8_000).await?;
/// let xml = format_as_context(&result);
/// println!("{xml}");
/// # Ok(())
/// # }
/// ```
pub struct CodeRetriever {
    store: CodeStore,
    provider: Arc<AnyProvider>,
    config: RetrievalConfig,
    token_counter: Arc<TokenCounter>,
}

impl CodeRetriever {
    /// Create a new `CodeRetriever`.
    ///
    /// `store` must have its Qdrant collection already created (see
    /// [`CodeStore::ensure_collection`]).
    #[must_use]
    pub fn new(store: CodeStore, provider: Arc<AnyProvider>, config: RetrievalConfig) -> Self {
        Self {
            store,
            provider,
            config,
            token_counter: Arc::new(TokenCounter::new()),
        }
    }

    /// Retrieve relevant code chunks for a free-text query.
    ///
    /// Classifies `query` via [`classify_query`], then:
    ///
    /// * For [`RetrievalStrategy::Grep`] queries — returns an empty [`RetrievedCode`]
    ///   so the agent falls back to its shell `grep` or `symbol_definition` tools.
    /// * For [`RetrievalStrategy::Semantic`] / [`RetrievalStrategy::Hybrid`] — embeds
    ///   the query, searches Qdrant, applies the score threshold, and packs results
    ///   within `available_tokens * budget_ratio`.
    ///
    /// # Errors
    ///
    /// Returns an error if the embedding call or Qdrant search fails.
    #[tracing::instrument(name = "index.retriever.retrieve", skip(self), fields(%query, available_tokens))]
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

    /// Retrieve relevant code, restricting results to a single language.
    ///
    /// Behaves like [`CodeRetriever::retrieve`] but adds a Qdrant payload filter so
    /// only chunks whose `language` field matches `language` are returned.
    ///
    /// Useful when the user or agent has already established the relevant language
    /// (e.g. "show me the Python error handling" should not return Rust results).
    ///
    /// # Arguments
    ///
    /// * `language` — the language identifier as returned by [`crate::languages::Lang::id`]
    ///   (e.g. `"rust"`, `"python"`).
    ///
    /// # Errors
    ///
    /// Returns an error if embedding or Qdrant search fails.
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

    #[tracing::instrument(name = "index.retriever.semantic_search", skip(self), fields(%query, token_budget))]
    async fn semantic_search(
        &self,
        query: &str,
        token_budget: usize,
        language_filter: Option<String>,
    ) -> Result<Vec<SearchHit>> {
        let timeout = std::time::Duration::from_secs(self.config.embed_timeout_secs);
        let raw_vector = tokio::time::timeout(timeout, self.provider.embed(query))
            .await
            .map_err(|_| {
                tracing::warn!(
                    embed_timeout_secs = self.config.embed_timeout_secs,
                    "embedding timed out"
                );
                crate::error::IndexError::EmbedTimeout(self.config.embed_timeout_secs)
            })??;

        // Normalize to unit length so Qdrant gRPC cosine search returns correct scores.
        // Qdrant gRPC silently returns near-zero scores for unnormalized vectors (#3421).
        let query_vector = EmbeddingVector::<Unnormalized>::new(raw_vector).normalize();

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

/// Format retrieved code chunks as an XML `<code_context>` block.
///
/// The output is suitable for direct injection into the agent's user or assistant
/// message. Each chunk is wrapped in a `<chunk>` element with `file`, `lines`,
/// `name`, and `score` attributes.
///
/// Returns an empty string when `result.chunks` is empty so callers can append
/// without adding unnecessary whitespace.
///
/// # Examples
///
/// ```
/// use zeph_index::retriever::{RetrievedCode, RetrievalStrategy, format_as_context};
/// use zeph_index::store::SearchHit;
///
/// let result = RetrievedCode {
///     chunks: vec![SearchHit {
///         code: "fn hello() {}".to_string(),
///         file_path: "src/lib.rs".to_string(),
///         line_range: (1, 1),
///         score: 0.9,
///         node_type: "function_item".to_string(),
///         entity_name: Some("hello".to_string()),
///         scope_chain: String::new(),
///     }],
///     total_tokens: 10,
///     strategy: RetrievalStrategy::Semantic,
/// };
///
/// let xml = format_as_context(&result);
/// assert!(xml.starts_with("<code_context>"));
/// assert!(xml.contains("file=\"src/lib.rs\""));
/// assert!(xml.ends_with("</code_context>"));
/// ```
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

/// Classify a free-text query to select the best retrieval strategy.
///
/// The heuristic looks for symbol-like patterns (Rust path syntax, `fn`/`struct`/`impl`
/// keywords, `CamelCase` type names, `snake_case` identifiers) and conceptual signal
/// words (`"how"`, `"explain"`, `"where"`, …).
///
/// | Signals present | Returned strategy |
/// |-----------------|-------------------|
/// | Symbol only | [`RetrievalStrategy::Grep`] |
/// | Conceptual only | [`RetrievalStrategy::Semantic`] |
/// | Both | [`RetrievalStrategy::Hybrid`] |
/// | Neither | [`RetrievalStrategy::Semantic`] |
///
/// # Examples
///
/// ```
/// use zeph_index::retriever::{RetrievalStrategy, classify_query};
///
/// assert_eq!(classify_query("how does retry logic work?"), RetrievalStrategy::Semantic);
/// assert_eq!(classify_query("fn handle_request"), RetrievalStrategy::Grep);
/// assert_eq!(classify_query("where is MyRouter defined?"), RetrievalStrategy::Hybrid);
/// ```
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
