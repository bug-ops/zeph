// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Dynamic tool schema filtering based on query-tool embedding similarity (#2020).
//!
//! Filters the set of tool definitions sent to the LLM on each turn, selecting
//! only the most relevant tools based on cosine similarity between the user query
//! embedding and pre-computed tool description embeddings.

use std::collections::HashSet;

use zeph_common::math::cosine_similarity;

/// Cached embedding for a tool definition.
#[derive(Debug, Clone)]
pub struct ToolEmbedding {
    pub tool_id: String,
    pub embedding: Vec<f32>,
}

/// Reason a tool was included in the filtered set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InclusionReason {
    /// Tool is in the always-on config list.
    AlwaysOn,
    /// Tool name was explicitly mentioned in the user query.
    NameMentioned,
    /// Tool scored within the top-K by similarity rank.
    SimilarityRank,
    /// MCP tool with too-short description to filter reliably.
    ShortDescription,
    /// Tool has no cached embedding (e.g. added after startup via MCP).
    NoEmbedding,
}

/// Result of filtering tool schemas against a query.
#[derive(Debug, Clone)]
pub struct ToolFilterResult {
    /// Tool IDs that passed the filter.
    pub included: HashSet<String>,
    /// Tool IDs that were filtered out.
    pub excluded: Vec<String>,
    /// Per-tool similarity scores for filterable tools (sorted descending).
    pub scores: Vec<(String, f32)>,
    /// Reason each included tool was included.
    pub inclusion_reasons: Vec<(String, InclusionReason)>,
}

/// Core filter holding cached tool embeddings and config.
pub struct ToolSchemaFilter {
    always_on: HashSet<String>,
    top_k: usize,
    min_description_words: usize,
    embeddings: Vec<ToolEmbedding>,
    version: u64,
}

impl ToolSchemaFilter {
    /// Create a new filter with pre-computed tool embeddings.
    #[must_use]
    pub fn new(
        always_on: Vec<String>,
        top_k: usize,
        min_description_words: usize,
        embeddings: Vec<ToolEmbedding>,
    ) -> Self {
        Self {
            always_on: always_on.into_iter().collect(),
            top_k,
            min_description_words,
            embeddings,
            version: 0,
        }
    }

    /// Current version counter. Incremented on recompute.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Number of cached tool embeddings.
    #[must_use]
    pub fn embedding_count(&self) -> usize {
        self.embeddings.len()
    }

    /// Replace tool embeddings (e.g. after MCP tool changes) and bump version.
    pub fn recompute(&mut self, embeddings: Vec<ToolEmbedding>) {
        self.embeddings = embeddings;
        self.version += 1;
    }

    /// Filter tools for a given user query embedding.
    ///
    /// `all_tool_ids` is the full set of tool IDs currently available.
    /// `tool_descriptions` maps tool ID to its description (for short-description check).
    /// `query_embedding` is the embedded user query.
    #[must_use]
    pub fn filter(
        &self,
        all_tool_ids: &[&str],
        tool_descriptions: &[(&str, &str)],
        query: &str,
        query_embedding: &[f32],
    ) -> ToolFilterResult {
        let mut included = HashSet::new();
        let mut inclusion_reasons = Vec::new();

        // 1. Always-on tools.
        for id in all_tool_ids {
            if self.always_on.contains(*id) {
                included.insert((*id).to_owned());
                inclusion_reasons.push(((*id).to_owned(), InclusionReason::AlwaysOn));
            }
        }

        // 2. Name-mentioned tools.
        let mentioned = find_mentioned_tool_ids(query, all_tool_ids);
        for id in &mentioned {
            if included.insert(id.clone()) {
                inclusion_reasons.push((id.clone(), InclusionReason::NameMentioned));
            }
        }

        // 3. Short-description MCP tools.
        for &(id, desc) in tool_descriptions {
            let word_count = desc.split_whitespace().count();
            if word_count < self.min_description_words && included.insert(id.to_owned()) {
                inclusion_reasons.push((id.to_owned(), InclusionReason::ShortDescription));
            }
        }

        // 4. Similarity-ranked filterable tools.
        let mut scores: Vec<(String, f32)> = self
            .embeddings
            .iter()
            .filter(|e| !included.contains(&e.tool_id))
            .map(|e| {
                let score = cosine_similarity(query_embedding, &e.embedding);
                (e.tool_id.clone(), score)
            })
            .collect();

        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let take = if self.top_k == 0 {
            scores.len()
        } else {
            self.top_k.min(scores.len())
        };

        for (id, _score) in scores.iter().take(take) {
            if included.insert(id.clone()) {
                inclusion_reasons.push((id.clone(), InclusionReason::SimilarityRank));
            }
        }

        // 5. Auto-include tools without embeddings (e.g. new MCP tools added after startup).
        let embedded_ids: HashSet<&str> =
            self.embeddings.iter().map(|e| e.tool_id.as_str()).collect();
        for id in all_tool_ids {
            if !included.contains(*id) && !embedded_ids.contains(*id) {
                included.insert((*id).to_owned());
                inclusion_reasons.push(((*id).to_owned(), InclusionReason::NoEmbedding));
            }
        }

        // Build excluded list.
        let excluded: Vec<String> = all_tool_ids
            .iter()
            .filter(|id| !included.contains(**id))
            .map(|id| (*id).to_owned())
            .collect();

        ToolFilterResult {
            included,
            excluded,
            scores,
            inclusion_reasons,
        }
    }
}

/// Find tool IDs explicitly mentioned in the query (case-insensitive, word-boundary aware).
///
/// Uses word-boundary checking: the character before and after the match must not be
/// alphanumeric or underscore. This prevents false positives like "read" matching "thread".
#[must_use]
pub fn find_mentioned_tool_ids(query: &str, all_tool_ids: &[&str]) -> Vec<String> {
    let query_lower = query.to_lowercase();
    all_tool_ids
        .iter()
        .filter(|id| {
            let id_lower = id.to_lowercase();
            let mut start = 0;
            while let Some(pos) = query_lower[start..].find(&id_lower) {
                let abs_pos = start + pos;
                let end_pos = abs_pos + id_lower.len();
                let before_ok = abs_pos == 0
                    || !query_lower.as_bytes()[abs_pos - 1].is_ascii_alphanumeric()
                        && query_lower.as_bytes()[abs_pos - 1] != b'_';
                let after_ok = end_pos >= query_lower.len()
                    || !query_lower.as_bytes()[end_pos].is_ascii_alphanumeric()
                        && query_lower.as_bytes()[end_pos] != b'_';
                if before_ok && after_ok {
                    return true;
                }
                start = abs_pos + 1;
            }
            false
        })
        .map(|id| (*id).to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_filter(always_on: Vec<&str>, top_k: usize) -> ToolSchemaFilter {
        ToolSchemaFilter::new(
            always_on.into_iter().map(String::from).collect(),
            top_k,
            5,
            vec![
                ToolEmbedding {
                    tool_id: "grep".into(),
                    embedding: vec![0.9, 0.1, 0.0],
                },
                ToolEmbedding {
                    tool_id: "write".into(),
                    embedding: vec![0.1, 0.9, 0.0],
                },
                ToolEmbedding {
                    tool_id: "find_path".into(),
                    embedding: vec![0.5, 0.5, 0.0],
                },
                ToolEmbedding {
                    tool_id: "web_scrape".into(),
                    embedding: vec![0.0, 0.0, 1.0],
                },
                ToolEmbedding {
                    tool_id: "diagnostics".into(),
                    embedding: vec![0.0, 0.1, 0.9],
                },
            ],
        )
    }

    #[test]
    fn top_k_ranking_selects_most_similar() {
        let filter = make_filter(vec!["bash"], 2);
        let all_ids: Vec<&str> = vec![
            "bash",
            "grep",
            "write",
            "find_path",
            "web_scrape",
            "diagnostics",
        ];
        let query_emb = vec![0.8, 0.2, 0.0]; // close to grep
        let result = filter.filter(&all_ids, &[], "search for pattern", &query_emb);

        assert!(result.included.contains("bash")); // always-on
        assert!(result.included.contains("grep")); // top similarity
        assert!(result.included.contains("find_path")); // 2nd top
        // web_scrape and diagnostics should be excluded
        assert!(!result.included.contains("web_scrape"));
        assert!(!result.included.contains("diagnostics"));
    }

    #[test]
    fn always_on_tools_always_included() {
        let filter = make_filter(vec!["bash", "read"], 1);
        let all_ids: Vec<&str> = vec!["bash", "read", "grep", "write"];
        let query_emb = vec![0.0, 1.0, 0.0]; // close to write
        let result = filter.filter(&all_ids, &[], "test query", &query_emb);

        assert!(result.included.contains("bash"));
        assert!(result.included.contains("read"));
        assert!(result.included.contains("write")); // top-1
        assert!(!result.included.contains("grep"));
    }

    #[test]
    fn name_mention_force_includes() {
        let filter = make_filter(vec!["bash"], 1);
        let all_ids: Vec<&str> = vec!["bash", "grep", "web_scrape", "write"];
        let query_emb = vec![0.0, 1.0, 0.0]; // close to write
        let result = filter.filter(&all_ids, &[], "use web_scrape to fetch", &query_emb);

        assert!(result.included.contains("web_scrape")); // name match
        assert!(result.included.contains("write")); // top-1
        assert!(result.included.contains("bash")); // always-on
    }

    #[test]
    fn short_mcp_description_auto_included() {
        let filter = make_filter(vec!["bash"], 1);
        let all_ids: Vec<&str> = vec!["bash", "grep", "mcp_query"];
        let descriptions: Vec<(&str, &str)> = vec![
            ("mcp_query", "Run query"),
            ("grep", "Search file contents recursively"),
        ];
        let query_emb = vec![0.9, 0.1, 0.0];
        let result = filter.filter(&all_ids, &descriptions, "test", &query_emb);

        assert!(result.included.contains("mcp_query")); // short desc (2 words)
    }

    #[test]
    fn empty_embeddings_includes_all_via_no_embedding_fallback() {
        let filter = ToolSchemaFilter::new(vec!["bash".into()], 6, 5, vec![]);
        let all_ids: Vec<&str> = vec!["bash", "grep", "write"];
        let query_emb = vec![0.5, 0.5, 0.0];
        let result = filter.filter(&all_ids, &[], "test", &query_emb);

        // All tools included: bash (always-on), grep+write (NoEmbedding fallback)
        assert!(result.included.contains("bash"));
        assert!(result.included.contains("grep"));
        assert!(result.included.contains("write"));
        assert!(result.excluded.is_empty());
    }

    #[test]
    fn top_k_zero_includes_all_filterable() {
        let filter = make_filter(vec!["bash"], 0);
        let all_ids: Vec<&str> = vec![
            "bash",
            "grep",
            "write",
            "find_path",
            "web_scrape",
            "diagnostics",
        ];
        let query_emb = vec![0.1, 0.1, 0.1];
        let result = filter.filter(&all_ids, &[], "test", &query_emb);

        assert_eq!(result.included.len(), 6); // all included
        assert!(result.excluded.is_empty());
    }

    #[test]
    fn top_k_exceeds_filterable_count_includes_all() {
        let filter = make_filter(vec!["bash"], 100);
        let all_ids: Vec<&str> = vec![
            "bash",
            "grep",
            "write",
            "find_path",
            "web_scrape",
            "diagnostics",
        ];
        let query_emb = vec![0.1, 0.1, 0.1];
        let result = filter.filter(&all_ids, &[], "test", &query_emb);

        assert_eq!(result.included.len(), 6);
    }

    #[test]
    fn version_counter_incremented_on_recompute() {
        let mut filter = make_filter(vec![], 3);
        assert_eq!(filter.version(), 0);
        filter.recompute(vec![]);
        assert_eq!(filter.version(), 1);
        filter.recompute(vec![]);
        assert_eq!(filter.version(), 2);
    }

    #[test]
    fn inclusion_reason_correctness() {
        let filter = make_filter(vec!["bash"], 1);
        let all_ids: Vec<&str> = vec!["bash", "grep", "web_scrape", "write"];
        let descriptions: Vec<(&str, &str)> = vec![("web_scrape", "Scrape")]; // 1 word
        let query_emb = vec![0.1, 0.9, 0.0]; // close to write
        let result = filter.filter(&all_ids, &descriptions, "test query", &query_emb);

        let reasons: std::collections::HashMap<String, InclusionReason> =
            result.inclusion_reasons.into_iter().collect();
        assert_eq!(reasons.get("bash"), Some(&InclusionReason::AlwaysOn));
        assert_eq!(
            reasons.get("web_scrape"),
            Some(&InclusionReason::ShortDescription)
        );
        assert_eq!(reasons.get("write"), Some(&InclusionReason::SimilarityRank));
    }

    #[test]
    fn cosine_similarity_identical_vectors() {
        let v = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&v, &v);
        assert!((sim - 1.0).abs() < 1e-5);
    }

    #[test]
    fn cosine_similarity_orthogonal_vectors() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-5);
    }

    #[test]
    fn cosine_similarity_empty_returns_zero() {
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
    }

    #[test]
    fn cosine_similarity_mismatched_length_returns_zero() {
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 2.0]), 0.0);
    }

    #[test]
    fn find_mentioned_tool_ids_case_insensitive() {
        let ids = vec!["web_scrape", "grep", "Bash"];
        let found = find_mentioned_tool_ids("use WEB_SCRAPE and BASH", &ids);
        assert!(found.contains(&"web_scrape".to_owned()));
        assert!(found.contains(&"Bash".to_owned()));
        assert!(!found.contains(&"grep".to_owned()));
    }

    #[test]
    fn find_mentioned_tool_ids_word_boundary_no_false_positives() {
        let ids = vec!["read", "edit", "fetch", "grep"];
        // "read" should NOT match inside "thread" or "breadcrumb"
        let found = find_mentioned_tool_ids("thread breadcrumb", &ids);
        assert!(found.is_empty());
    }

    #[test]
    fn find_mentioned_tool_ids_word_boundary_matches_standalone() {
        let ids = vec!["read", "edit"];
        let found = find_mentioned_tool_ids("please read and edit the file", &ids);
        assert!(found.contains(&"read".to_owned()));
        assert!(found.contains(&"edit".to_owned()));
    }
}
