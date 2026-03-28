// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! In-memory embedding index for MCP tool selection (#2321).
//!
//! [`SemanticToolIndex`] embeds tool descriptions once at connect time and
//! retrieves the top-K most relevant tools per query using brute-force cosine
//! similarity.  It is a cheaper, faster alternative to the LLM-based
//! `prune_tools()` path.
//!
//! # Crate dependency
//!
//! Like `PruningParams`, this struct lives in `zeph-mcp` (not `zeph-config`)
//! to avoid a circular dependency (`zeph-config` → `zeph-mcp`).
//! The config-side mirror is `ToolDiscoveryConfig` in `zeph-config`.
//! Callers in `zeph-core` convert between the two.

use futures::stream::{self, StreamExt};

use crate::tool::McpTool;

// ── Errors ────────────────────────────────────────────────────────────────────

/// Errors produced by [`SemanticToolIndex::build`].
#[derive(Debug, thiserror::Error)]
pub enum SemanticIndexError {
    /// Every tool in the input list failed to embed.
    #[error("all {count} tool embeddings failed during index build")]
    AllEmbeddingsFailed { count: usize },
}

// ── SemanticToolIndex ─────────────────────────────────────────────────────────

struct ToolEntry {
    tool: McpTool,
    embedding: Vec<f32>,
}

/// In-memory embedding index for a set of MCP tools.
///
/// Built once at connect time (or on tool list change) by calling
/// [`SemanticToolIndex::build`].  Stores pre-computed embeddings alongside tool
/// metadata for O(N) cosine similarity retrieval per query.
///
/// # Thread safety
///
/// Not thread-safe.  Must be owned exclusively by the agent turn loop
/// (`&mut Agent`).  Do NOT wrap in `Arc<Mutex<>>` or share across tasks.
///
/// # Invariants
///
/// The `embed_fn` used at [`build`](Self::build) time MUST produce vectors of
/// the same dimension as the query embedding supplied to [`select`](Self::select).
/// [`select`] checks the dimension on every call and skips mismatched entries.
#[derive(Debug)]
pub struct SemanticToolIndex {
    entries: Vec<ToolEntry>,
    /// All tools supplied to `build()`, including those that failed to embed.
    /// Used to resolve `always_include` lookups (critic finding #1).
    all_tools: Vec<McpTool>,
}

impl std::fmt::Debug for ToolEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolEntry")
            .field("tool_name", &self.tool.name)
            .field("embedding_dim", &self.embedding.len())
            .finish()
    }
}

impl SemanticToolIndex {
    /// Build an embedding index from `tools`.
    ///
    /// Calls `embed_fn` once per tool using the concatenated `"name: description"` text
    /// (name carries strong semantic signal).  Tools whose embedding fails are logged at
    /// `WARN` level and excluded from cosine similarity retrieval, but remain available
    /// via the `always_include` path in [`select`](Self::select).
    ///
    /// Embeddings are requested concurrently with a concurrency cap of 8 to avoid
    /// overwhelming the provider while still being significantly faster than sequential
    /// per-tool calls.
    ///
    /// # Errors
    ///
    /// Returns [`SemanticIndexError::AllEmbeddingsFailed`] when every tool's embedding
    /// fails.  The caller should fall back to the `None` strategy (all tools).
    pub async fn build<F>(tools: &[McpTool], embed_fn: &F) -> Result<Self, SemanticIndexError>
    where
        F: Fn(&str) -> zeph_llm::provider::EmbedFuture + Send + Sync,
    {
        if tools.is_empty() {
            return Ok(Self {
                entries: Vec::new(),
                all_tools: Vec::new(),
            });
        }

        // Embed up to 8 tools concurrently.
        // Sanitize description before embedding: strip control chars, cap at 200 chars.
        // Mirrors the sanitization applied to the LLM pruning prompt to prevent
        // embedding poisoning via keyword-stuffed tool descriptions.
        let results: Vec<(usize, Result<Vec<f32>, _>)> = stream::iter(tools.iter().enumerate())
            .map(|(idx, tool)| {
                let sanitized_desc: String = tool
                    .description
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(200)
                    .collect();
                let text = format!("{}: {}", tool.name, sanitized_desc);
                let fut = embed_fn(&text);
                async move { (idx, fut.await) }
            })
            .buffer_unordered(8)
            .collect()
            .await;

        let mut entries = Vec::with_capacity(tools.len());
        let mut failed = 0usize;

        for (idx, result) in results {
            match result {
                Ok(embedding) => entries.push(ToolEntry {
                    tool: tools[idx].clone(),
                    embedding,
                }),
                Err(e) => {
                    failed += 1;
                    tracing::warn!(
                        tool_name = %tools[idx].name,
                        server_id = %tools[idx].server_id,
                        "semantic index: embedding failed for tool, excluded from similarity ranking: {e:#}"
                    );
                }
            }
        }

        if entries.is_empty() {
            return Err(SemanticIndexError::AllEmbeddingsFailed { count: failed });
        }

        if failed > 0 {
            tracing::warn!(
                total = tools.len(),
                failed,
                indexed = entries.len(),
                "semantic index: some tools failed to embed"
            );
        }

        Ok(Self {
            entries,
            all_tools: tools.to_vec(),
        })
    }

    /// Retrieve top-K tools by cosine similarity to `query_embedding`.
    ///
    /// Returns tools sorted by descending similarity, filtered by `min_similarity`.
    /// Tools in `always_include` are prepended unconditionally and do NOT count toward
    /// the `top_k` cap (same semantics as `PruningParams::always_include`).
    ///
    /// `always_include` matches bare tool `name` (not `server_id:name`).  When two MCP
    /// servers expose tools with the same name, both are included — consistent with
    /// `PruningParams` semantics.
    ///
    /// Always-include lookups are resolved against the **original** tool list supplied to
    /// [`build`](Self::build), so a tool that failed to embed can still be pinned.
    ///
    /// If `query_embedding` is empty or `top_k` is 0, returns only always-include tools.
    pub fn select(
        &self,
        query_embedding: &[f32],
        top_k: usize,
        min_similarity: f32,
        always_include: &[String],
    ) -> Vec<McpTool> {
        // Collect always-include tools from the original list (includes failed-to-embed tools).
        let mut pinned: Vec<McpTool> = self
            .all_tools
            .iter()
            .filter(|t| always_include.iter().any(|a| a == &t.name))
            .cloned()
            .collect();

        if query_embedding.is_empty() || top_k == 0 {
            return pinned;
        }

        let query_dim = query_embedding.len();

        // Score all indexed entries, skipping dimension mismatches.
        let mut scored: Vec<(f32, &McpTool)> = self
            .entries
            .iter()
            .filter(|e| {
                if e.embedding.len() == query_dim {
                    true
                } else {
                    tracing::warn!(
                        tool_name = %e.tool.name,
                        entry_dim = e.embedding.len(),
                        query_dim,
                        "semantic index: dimension mismatch, skipping tool"
                    );
                    false
                }
            })
            // Skip tools already pinned by always_include to avoid duplicates.
            .filter(|e| !always_include.iter().any(|a| a == &e.tool.name))
            .map(|e| (cosine_similarity(query_embedding, &e.embedding), &e.tool))
            .filter(|(score, _)| *score >= min_similarity)
            .collect();

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top_k);

        for (score, tool) in &scored {
            tracing::debug!(tool_name = %tool.name, score, "semantic tool selection score");
        }

        pinned.extend(scored.into_iter().map(|(_, t)| t.clone()));
        pinned
    }

    /// Number of indexed (successfully embedded) tools.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the index has no successfully embedded tools.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ── Tool discovery strategy ───────────────────────────────────────────────────

/// MCP tool discovery strategy.
///
/// Mirrors `ToolDiscoveryStrategyConfig` in `zeph-config` but lives in `zeph-mcp`
/// to avoid a circular crate dependency.  Callers in `zeph-core` convert between
/// the two representations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolDiscoveryStrategy {
    /// Embedding-based cosine similarity retrieval.  Fast, no LLM call per turn.
    Embedding,
    /// LLM-based pruning via `prune_tools_cached`.  Existing behavior.
    Llm,
    /// No filtering — all tools are passed through.  This is the default.
    #[default]
    None,
}

/// Parameters for embedding-based tool discovery.
///
/// Mirrors `ToolDiscoveryConfig` in `zeph-config`.  Callers in `zeph-core`
/// convert from `ToolDiscoveryConfig` before wiring into `McpState`.
#[derive(Debug, Clone)]
pub struct DiscoveryParams {
    /// Number of top-scoring tools to include per turn.
    pub top_k: usize,
    /// Minimum cosine similarity for a tool to be included.
    pub min_similarity: f32,
    /// Minimum tool count below which discovery is skipped (all tools passed through).
    pub min_tools_to_filter: usize,
    /// Tool names always included regardless of similarity score.
    pub always_include: Vec<String>,
    /// When `true`, treat any embedding failure as a hard error instead of silently
    /// falling back to all tools.  Default: `false`.
    pub strict: bool,
}

impl Default for DiscoveryParams {
    fn default() -> Self {
        Self {
            top_k: 10,
            min_similarity: 0.2,
            min_tools_to_filter: 10,
            always_include: Vec::new(),
            strict: false,
        }
    }
}

// ── Cosine similarity ─────────────────────────────────────────────────────────

/// Compute cosine similarity between two equal-length vectors.
///
/// Returns 0.0 if either vector has zero magnitude (avoids NaN).
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "cosine_similarity: dimension mismatch");

    let mut dot = 0.0f32;
    let mut mag_a = 0.0f32;
    let mut mag_b = 0.0f32;

    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        mag_a += x * x;
        mag_b += y * y;
    }

    let denom = mag_a.sqrt() * mag_b.sqrt();
    if denom == 0.0 { 0.0 } else { dot / denom }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use zeph_llm::provider::EmbedFn;

    use super::*;

    fn make_tool(name: &str, desc: &str) -> McpTool {
        McpTool {
            server_id: "srv".into(),
            name: name.into(),
            description: desc.into(),
            input_schema: serde_json::Value::Null,
        }
    }

    /// Embedding function that returns a fixed vector based on the tool name hash.
    /// First char code determines the embedding direction to give distinct similarity values.
    fn fixed_embed() -> EmbedFn {
        Box::new(|text: &str| -> zeph_llm::provider::EmbedFuture {
            let first = f32::from(text.chars().next().unwrap_or('a') as u8);
            // 3-dim vector with first element = first char value, rest = 1.0
            let v = vec![first / 100.0, 1.0, 1.0];
            Box::pin(async move { Ok(v) })
        })
    }

    fn failing_embed() -> EmbedFn {
        Box::new(|_text: &str| -> zeph_llm::provider::EmbedFuture {
            Box::pin(async move { Err(zeph_llm::LlmError::Other("forced failure".into())) })
        })
    }

    #[tokio::test]
    async fn build_empty_tools_returns_empty_index() {
        let embed = fixed_embed();
        let idx = SemanticToolIndex::build(&[], &embed).await.unwrap();
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
    }

    #[tokio::test]
    async fn build_all_fail_returns_error() {
        let tools = vec![make_tool("a", "desc")];
        let embed = failing_embed();
        let err = SemanticToolIndex::build(&tools, &embed).await.unwrap_err();
        assert!(matches!(
            err,
            SemanticIndexError::AllEmbeddingsFailed { count: 1 }
        ));
    }

    #[tokio::test]
    async fn build_partial_failure_returns_partial_index() {
        let tools = vec![make_tool("aaa", "desc a"), make_tool("bbb", "desc b")];
        // Fail only the second embedding by index; use a counter.
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cc = call_count.clone();
        let embed: EmbedFn = Box::new(move |_text: &str| -> zeph_llm::provider::EmbedFuture {
            let n = cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    Ok(vec![1.0, 0.0, 0.0])
                } else {
                    Err(zeph_llm::LlmError::Other("fail".into()))
                }
            })
        });
        let idx = SemanticToolIndex::build(&tools, &embed).await.unwrap();
        assert_eq!(
            idx.len(),
            1,
            "one tool indexed despite second embedding failure"
        );
    }

    #[tokio::test]
    async fn select_returns_top_k() {
        let tools: Vec<McpTool> = (0..5).map(|i| make_tool(&format!("t{i}"), "d")).collect();
        let embed: EmbedFn = Box::new(|text: &str| -> zeph_llm::provider::EmbedFuture {
            // Return distinct vectors based on text length mod 5.
            #[allow(clippy::cast_precision_loss)]
            let v = vec![text.len() as f32 / 10.0, 1.0];
            Box::pin(async move { Ok(v) })
        });
        let idx = SemanticToolIndex::build(&tools, &embed).await.unwrap();
        let query = vec![1.0, 1.0];
        let result = idx.select(&query, 3, 0.0, &[]);
        assert_eq!(result.len(), 3);
    }

    #[tokio::test]
    async fn select_always_include_from_failed_tools() {
        // Tool "pinned" fails to embed but should appear via always_include.
        let tools = vec![
            make_tool("pinned", "always here"),
            make_tool("normal", "normal desc"),
        ];
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cc = call_count.clone();
        let embed: EmbedFn = Box::new(move |_text: &str| -> zeph_llm::provider::EmbedFuture {
            let n = cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    // "pinned" fails
                    Err(zeph_llm::LlmError::Other("fail".into()))
                } else {
                    Ok(vec![1.0, 0.0])
                }
            })
        });
        let idx = SemanticToolIndex::build(&tools, &embed).await.unwrap();
        let query = vec![1.0, 0.0];
        let result = idx.select(&query, 10, 0.0, &["pinned".to_string()]);
        assert!(
            result.iter().any(|t| t.name == "pinned"),
            "always_include must include failed-to-embed tools"
        );
    }

    #[tokio::test]
    async fn select_min_similarity_filters_low_scores() {
        let tools = vec![make_tool("t0", "x"), make_tool("t1", "y")];
        // t0 embedding is parallel to query → similarity ~1.0
        // t1 embedding is orthogonal → similarity ~0.0
        let embed: EmbedFn = Box::new(|text: &str| -> zeph_llm::provider::EmbedFuture {
            let v = if text.starts_with("t0") {
                vec![1.0_f32, 0.0]
            } else {
                vec![0.0_f32, 1.0]
            };
            Box::pin(async move { Ok(v) })
        });
        let idx = SemanticToolIndex::build(&tools, &embed).await.unwrap();
        let query = vec![1.0_f32, 0.0];
        // min_similarity=0.5 should exclude the orthogonal tool.
        let result = idx.select(&query, 10, 0.5, &[]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "t0");
    }

    #[tokio::test]
    async fn select_dimension_mismatch_skips_entry() {
        let tools = vec![make_tool("t0", "d")];
        let embed: EmbedFn = Box::new(|_text: &str| -> zeph_llm::provider::EmbedFuture {
            Box::pin(async move { Ok(vec![1.0, 0.0, 0.0]) }) // 3-dim
        });
        let idx = SemanticToolIndex::build(&tools, &embed).await.unwrap();
        // Query with 2-dim vector — dimension mismatch, tool must be skipped.
        let result = idx.select(&[1.0, 0.0], 10, 0.0, &[]);
        assert!(result.is_empty(), "dimension mismatch must skip entry");
    }

    #[test]
    fn cosine_similarity_identical_vectors() {
        let v = vec![1.0, 2.0, 3.0];
        let s = cosine_similarity(&v, &v);
        assert!((s - 1.0).abs() < 1e-5);
    }

    #[test]
    fn cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let s = cosine_similarity(&a, &b);
        assert!(s.abs() < 1e-5);
    }

    #[test]
    fn cosine_similarity_zero_vector_returns_zero() {
        let a = vec![0.0, 0.0];
        let b = vec![1.0, 2.0];
        let s = cosine_similarity(&a, &b);
        assert!(s.abs() < f32::EPSILON);
    }

    // top_k larger than the number of available tools: must return all tools, not panic.
    #[tokio::test]
    async fn select_top_k_exceeds_available_tools() {
        let tools: Vec<McpTool> = (0..3).map(|i| make_tool(&format!("t{i}"), "d")).collect();
        let embed: EmbedFn = Box::new(|_text: &str| -> zeph_llm::provider::EmbedFuture {
            Box::pin(async move { Ok(vec![1.0, 0.0]) })
        });
        let idx = SemanticToolIndex::build(&tools, &embed).await.unwrap();
        // top_k=100 > 3 indexed tools; must return all 3 without panic.
        let result = idx.select(&[1.0, 0.0], 100, 0.0, &[]);
        assert_eq!(
            result.len(),
            3,
            "top_k > available tools must return all tools"
        );
    }

    // top_k = 0 with no always_include: returns empty.
    #[tokio::test]
    async fn select_top_k_zero_returns_empty() {
        let tools: Vec<McpTool> = (0..3).map(|i| make_tool(&format!("t{i}"), "d")).collect();
        let embed: EmbedFn = Box::new(|_text: &str| -> zeph_llm::provider::EmbedFuture {
            Box::pin(async move { Ok(vec![1.0, 0.0]) })
        });
        let idx = SemanticToolIndex::build(&tools, &embed).await.unwrap();
        let result = idx.select(&[1.0, 0.0], 0, 0.0, &[]);
        assert!(
            result.is_empty(),
            "top_k=0 with no always_include must return empty"
        );
    }

    // top_k = 0 with always_include: pinned tools are returned regardless.
    #[tokio::test]
    async fn select_top_k_zero_returns_pinned() {
        let tools = vec![make_tool("pinned", "always"), make_tool("other", "other")];
        let embed: EmbedFn = Box::new(|_text: &str| -> zeph_llm::provider::EmbedFuture {
            Box::pin(async move { Ok(vec![1.0, 0.0]) })
        });
        let idx = SemanticToolIndex::build(&tools, &embed).await.unwrap();
        let result = idx.select(&[1.0, 0.0], 0, 0.0, &["pinned".to_string()]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "pinned");
    }

    // empty query embedding: returns only always_include tools.
    #[tokio::test]
    async fn select_empty_query_returns_only_pinned() {
        let tools = vec![make_tool("pinned", "always"), make_tool("other", "other")];
        let embed: EmbedFn = Box::new(|_text: &str| -> zeph_llm::provider::EmbedFuture {
            Box::pin(async move { Ok(vec![1.0, 0.0]) })
        });
        let idx = SemanticToolIndex::build(&tools, &embed).await.unwrap();
        let result = idx.select(&[], 10, 0.0, &["pinned".to_string()]);
        assert_eq!(
            result.len(),
            1,
            "empty query must return only always_include tools"
        );
        assert_eq!(result[0].name, "pinned");
    }

    // always_include tool must not appear twice in the result even when it also scores highly.
    #[tokio::test]
    async fn select_always_include_no_duplicate() {
        let tools = vec![make_tool("pinned", "always"), make_tool("other", "other")];
        let embed: EmbedFn = Box::new(|_text: &str| -> zeph_llm::provider::EmbedFuture {
            Box::pin(async move { Ok(vec![1.0, 0.0]) })
        });
        let idx = SemanticToolIndex::build(&tools, &embed).await.unwrap();
        // Query that would match "pinned" with high score; it is also in always_include.
        let result = idx.select(&[1.0, 0.0], 10, 0.0, &["pinned".to_string()]);
        let pinned_count = result.iter().filter(|t| t.name == "pinned").count();
        assert_eq!(
            pinned_count, 1,
            "always_include tool must not be duplicated in result"
        );
    }

    // ToolDiscoveryStrategy::None variant exists and has correct default.
    #[test]
    fn strategy_none_variant_exists() {
        let s = ToolDiscoveryStrategy::None;
        assert_ne!(s, ToolDiscoveryStrategy::Embedding);
        assert_ne!(s, ToolDiscoveryStrategy::Llm);
    }

    // ToolDiscoveryStrategy default is None (safe default: all tools, no filtering).
    #[test]
    fn strategy_default_is_none() {
        assert_eq!(
            ToolDiscoveryStrategy::default(),
            ToolDiscoveryStrategy::None
        );
    }
}
