// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Dynamic tool schema filtering based on query-tool embedding similarity (#2020).
//!
//! Filters the set of tool definitions sent to the LLM on each turn, selecting
//! only the most relevant tools based on cosine similarity between the user query
//! embedding and pre-computed tool description embeddings.

use std::collections::{HashMap, HashSet};

use zeph_common::math::cosine_similarity;

use crate::config::ToolDependency;

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
    /// Tool included because its hard requirements (`requires`) are all satisfied.
    DependencyMet,
    /// Tool received a similarity boost from satisfied soft prerequisites (`prefers`).
    PreferenceBoost,
}

/// Exclusion reason for a tool that was blocked by the dependency gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyExclusion {
    pub tool_id: String,
    /// IDs of `requires` entries that are not yet satisfied.
    pub unmet_requires: Vec<String>,
}

/// Result of filtering tool schemas against a query.
#[derive(Debug, Clone)]
pub struct ToolFilterResult {
    /// Tool IDs that passed the filter.
    pub included: HashSet<String>,
    /// Tool IDs that were filtered out by similarity/embedding.
    pub excluded: Vec<String>,
    /// Per-tool similarity scores for filterable tools (sorted descending).
    pub scores: Vec<(String, f32)>,
    /// Reason each included tool was included.
    pub inclusion_reasons: Vec<(String, InclusionReason)>,
    /// Tools excluded specifically due to unmet hard dependencies.
    pub dependency_exclusions: Vec<DependencyExclusion>,
}

/// Dependency graph for sequential tool availability (issue #2024).
///
/// Built once from `DependencyConfig` at agent start, reused across turns.
/// Implements cycle detection via DFS topological sort: any tool in a detected
/// cycle has all its `requires` removed (made unconditionally available) so it
/// can never be permanently blocked by a dependency loop.
///
/// # Deadlock fallback
///
/// If all non-always-on tools would be blocked (either by config cycles or
/// unreachable `requires` chains), `apply()` detects this at filter time and
/// disables hard gates for that turn, logging a warning.
#[derive(Debug, Clone, Default)]
pub struct ToolDependencyGraph {
    /// Map from `tool_id` -> its dependency spec.
    /// Tools in cycles have their `requires` cleared at construction time.
    deps: HashMap<String, ToolDependency>,
}

impl ToolDependencyGraph {
    /// Build a dependency graph from a map of tool rules.
    ///
    /// Performs DFS-based cycle detection. All tools participating in any cycle
    /// have their `requires` entries removed so they are always available.
    #[must_use]
    pub fn new(deps: HashMap<String, ToolDependency>) -> Self {
        if deps.is_empty() {
            return Self { deps };
        }
        let cycled = detect_cycles(&deps);
        if !cycled.is_empty() {
            tracing::warn!(
                tools = ?cycled,
                "tool dependency graph: cycles detected, removing requires for cycle participants"
            );
        }
        let mut resolved = deps;
        for tool_id in &cycled {
            if let Some(dep) = resolved.get_mut(tool_id) {
                dep.requires.clear();
            }
        }
        Self { deps: resolved }
    }

    /// Returns true if no dependency rules are configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.deps.is_empty()
    }

    /// Check if a tool's hard requirements are all satisfied.
    ///
    /// Returns `true` if the tool has no `requires` entries, or if all entries
    /// are present in `completed`. Returns `true` for unconfigured tools.
    #[must_use]
    pub fn requirements_met(&self, tool_id: &str, completed: &HashSet<String>) -> bool {
        self.deps
            .get(tool_id)
            .is_none_or(|d| d.requires.iter().all(|r| completed.contains(r)))
    }

    /// Returns the unmet `requires` entries for a tool, if any.
    #[must_use]
    pub fn unmet_requires<'a>(
        &'a self,
        tool_id: &str,
        completed: &HashSet<String>,
    ) -> Vec<&'a str> {
        self.deps.get(tool_id).map_or_else(Vec::new, |d| {
            d.requires
                .iter()
                .filter(|r| !completed.contains(r.as_str()))
                .map(String::as_str)
                .collect()
        })
    }

    /// Calculate similarity boost for soft prerequisites.
    ///
    /// Returns `min(boost_per_dep * met_count, max_total_boost)`.
    #[must_use]
    pub fn preference_boost(
        &self,
        tool_id: &str,
        completed: &HashSet<String>,
        boost_per_dep: f32,
        max_total_boost: f32,
    ) -> f32 {
        self.deps.get(tool_id).map_or(0.0, |d| {
            let met = d
                .prefers
                .iter()
                .filter(|p| completed.contains(p.as_str()))
                .count();
            #[allow(clippy::cast_precision_loss)]
            let boost = met as f32 * boost_per_dep;
            boost.min(max_total_boost)
        })
    }

    /// Apply hard dependency gates and preference boosts to a `ToolFilterResult`.
    ///
    /// Called after `ToolSchemaFilter::filter()` returns so the filter signature
    /// remains unchanged (HIGH-03 fix). Dependency gates are applied AFTER TAFC
    /// augmentation to prevent re-adding gated tools through augmentation (MED-04 fix).
    ///
    /// # Deadlock fallback (CRIT-01)
    ///
    /// If applying hard gates would remove ALL non-always-on included tools, the
    /// gates are disabled for this turn and a warning is logged.
    pub fn apply(
        &self,
        result: &mut ToolFilterResult,
        completed: &HashSet<String>,
        boost_per_dep: f32,
        max_total_boost: f32,
        always_on: &HashSet<String>,
    ) {
        if self.deps.is_empty() {
            return;
        }

        // Collect tools to gate (those not already excluded and not always-on / name-mentioned).
        // Always-on and name-mentioned tools bypass the hard gate per design.
        let bypassed: HashSet<&str> = result
            .inclusion_reasons
            .iter()
            .filter(|(_, r)| {
                matches!(
                    r,
                    InclusionReason::AlwaysOn | InclusionReason::NameMentioned
                )
            })
            .map(|(id, _)| id.as_str())
            .collect();

        let mut to_exclude: Vec<DependencyExclusion> = Vec::new();
        for tool_id in &result.included {
            if bypassed.contains(tool_id.as_str()) {
                continue;
            }
            let unmet: Vec<String> = self
                .unmet_requires(tool_id, completed)
                .into_iter()
                .map(str::to_owned)
                .collect();
            if !unmet.is_empty() {
                to_exclude.push(DependencyExclusion {
                    tool_id: tool_id.clone(),
                    unmet_requires: unmet,
                });
            }
        }

        // CRIT-01: deadlock fallback — if gating would leave no non-always-on tools,
        // skip hard gates for this turn.
        let non_always_on_included: usize = result
            .included
            .iter()
            .filter(|id| !always_on.contains(id.as_str()))
            .count();
        if !to_exclude.is_empty() && to_exclude.len() >= non_always_on_included {
            tracing::warn!(
                gated = to_exclude.len(),
                non_always_on = non_always_on_included,
                "tool dependency graph: all non-always-on tools would be blocked; \
                 disabling hard gates for this turn"
            );
            to_exclude.clear();
        }

        // Apply hard gates.
        for excl in &to_exclude {
            result.included.remove(&excl.tool_id);
            result.excluded.push(excl.tool_id.clone());
            tracing::debug!(
                tool_id = %excl.tool_id,
                unmet = ?excl.unmet_requires,
                "tool dependency gate: excluded (requires not met)"
            );
        }
        result.dependency_exclusions = to_exclude;

        // Apply preference boosts: adjust scores for tools with satisfied prefers deps.
        for (tool_id, score) in &mut result.scores {
            if !result.included.contains(tool_id) {
                continue;
            }
            let boost = self.preference_boost(tool_id, completed, boost_per_dep, max_total_boost);
            if boost > 0.0 {
                *score += boost;
                // Record reason if not already recorded with a higher-priority reason.
                let already_recorded = result.inclusion_reasons.iter().any(|(id, _)| id == tool_id);
                if !already_recorded {
                    result
                        .inclusion_reasons
                        .push((tool_id.clone(), InclusionReason::PreferenceBoost));
                }
                tracing::debug!(
                    tool_id = %tool_id,
                    boost,
                    "tool dependency: preference boost applied"
                );
            }
        }
        // Re-sort scores after boosts.
        result
            .scores
            .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    }

    /// Filter a slice of tool IDs to those whose hard requirements are met.
    ///
    /// Used on iterations 1+ in the native tool loop via the agent helper
    /// `apply_hard_dependency_gate_to_names`. Returns only the IDs that pass.
    #[must_use]
    pub fn filter_tool_names<'a>(
        &self,
        names: &[&'a str],
        completed: &HashSet<String>,
        always_on: &HashSet<String>,
    ) -> Vec<&'a str> {
        names
            .iter()
            .copied()
            .filter(|n| always_on.contains(*n) || self.requirements_met(n, completed))
            .collect()
    }
}

/// DFS-based cycle detection for tool dependency graphs.
///
/// Returns the set of tool IDs that participate in any cycle.
/// Algorithm: standard DFS with three states (unvisited/in-progress/done).
/// When a back-edge is found (visiting an in-progress node), all nodes in the
/// current DFS path that form part of the cycle are collected.
fn detect_cycles(deps: &HashMap<String, ToolDependency>) -> HashSet<String> {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Unvisited,
        InProgress,
        Done,
    }

    let mut state: HashMap<&str, State> = HashMap::new();
    let mut cycled: HashSet<String> = HashSet::new();

    for start in deps.keys() {
        if state
            .get(start.as_str())
            .copied()
            .unwrap_or(State::Unvisited)
            != State::Unvisited
        {
            continue;
        }
        let mut stack: Vec<(&str, usize)> = vec![(start.as_str(), 0)];
        state.insert(start.as_str(), State::InProgress);

        while let Some((node, child_idx)) = stack.last_mut() {
            let node = *node;
            let requires = deps
                .get(node)
                .map_or(&[] as &[String], |d| d.requires.as_slice());

            if *child_idx >= requires.len() {
                state.insert(node, State::Done);
                stack.pop();
                continue;
            }

            let child = requires[*child_idx].as_str();
            *child_idx += 1;

            match state.get(child).copied().unwrap_or(State::Unvisited) {
                State::InProgress => {
                    // Back-edge: mark all nodes currently in the DFS path as cycled.
                    for (path_node, _) in &stack {
                        cycled.insert((*path_node).to_owned());
                    }
                    cycled.insert(child.to_owned());
                }
                State::Unvisited => {
                    state.insert(child, State::InProgress);
                    stack.push((child, 0));
                }
                State::Done => {}
            }
        }
    }

    cycled
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

    /// Configured top-K limit for similarity ranking.
    #[must_use]
    pub fn top_k(&self) -> usize {
        self.top_k
    }

    /// Number of always-on tools in the filter config.
    #[must_use]
    pub fn always_on_count(&self) -> usize {
        self.always_on.len()
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
            dependency_exclusions: Vec::new(),
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
    fn accessors_return_configured_values() {
        let filter = make_filter(vec!["bash", "read"], 7);
        assert_eq!(filter.top_k(), 7);
        assert_eq!(filter.always_on_count(), 2);
        assert_eq!(filter.embedding_count(), 5);
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

    // --- ToolDependencyGraph tests ---

    fn make_dep_graph(rules: &[(&str, Vec<&str>, Vec<&str>)]) -> ToolDependencyGraph {
        let deps = rules
            .iter()
            .map(|(id, requires, prefers)| {
                (
                    (*id).to_owned(),
                    crate::config::ToolDependency {
                        requires: requires.iter().map(|s| (*s).to_owned()).collect(),
                        prefers: prefers.iter().map(|s| (*s).to_owned()).collect(),
                    },
                )
            })
            .collect();
        ToolDependencyGraph::new(deps)
    }

    fn completed(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn requirements_met_no_deps() {
        let graph = make_dep_graph(&[]);
        assert!(graph.requirements_met("any_tool", &completed(&[])));
    }

    #[test]
    fn requirements_met_all_satisfied() {
        let graph = make_dep_graph(&[("apply_patch", vec!["read"], vec![])]);
        assert!(graph.requirements_met("apply_patch", &completed(&["read"])));
    }

    #[test]
    fn requirements_met_unmet() {
        let graph = make_dep_graph(&[("apply_patch", vec!["read"], vec![])]);
        assert!(!graph.requirements_met("apply_patch", &completed(&[])));
    }

    #[test]
    fn requirements_met_unconfigured_tool() {
        let graph = make_dep_graph(&[("apply_patch", vec!["read"], vec![])]);
        // tools not in the graph are always available
        assert!(graph.requirements_met("grep", &completed(&[])));
    }

    #[test]
    fn preference_boost_none_met() {
        let graph = make_dep_graph(&[("format", vec![], vec!["search", "grep"])]);
        let boost = graph.preference_boost("format", &completed(&[]), 0.15, 0.2);
        assert_eq!(boost, 0.0);
    }

    #[test]
    fn preference_boost_partial() {
        let graph = make_dep_graph(&[("format", vec![], vec!["search", "grep"])]);
        let boost = graph.preference_boost("format", &completed(&["search"]), 0.15, 0.2);
        assert!((boost - 0.15).abs() < 1e-5);
    }

    #[test]
    fn preference_boost_capped_at_max() {
        // 3 prefs x 0.15 = 0.45 but max is 0.2
        let graph = make_dep_graph(&[("format", vec![], vec!["a", "b", "c"])]);
        let boost = graph.preference_boost("format", &completed(&["a", "b", "c"]), 0.15, 0.2);
        assert!((boost - 0.2).abs() < 1e-5);
    }

    #[test]
    fn cycle_detection_simple_cycle() {
        // A requires B, B requires A → both should have requires cleared
        let graph = make_dep_graph(&[
            ("tool_a", vec!["tool_b"], vec![]),
            ("tool_b", vec!["tool_a"], vec![]),
        ]);
        // After cycle removal both should be unconditionally available
        assert!(graph.requirements_met("tool_a", &completed(&[])));
        assert!(graph.requirements_met("tool_b", &completed(&[])));
    }

    #[test]
    fn cycle_detection_does_not_affect_non_cycle_tools() {
        // A requires B, B requires C (no cycle), C requires D (cycle: D requires C)
        let graph = make_dep_graph(&[
            ("tool_a", vec!["tool_b"], vec![]),
            ("tool_b", vec!["tool_c"], vec![]),
            ("tool_c", vec!["tool_d"], vec![]),
            ("tool_d", vec!["tool_c"], vec![]), // cycle: c <-> d
        ]);
        // tool_c and tool_d participate in cycle → unconditionally available
        assert!(graph.requirements_met("tool_c", &completed(&[])));
        assert!(graph.requirements_met("tool_d", &completed(&[])));
        // tool_a and tool_b are NOT in a cycle → still gated
        assert!(!graph.requirements_met("tool_a", &completed(&[])));
        assert!(!graph.requirements_met("tool_b", &completed(&[])));
    }

    #[test]
    fn apply_excludes_gated_tool() {
        let graph = make_dep_graph(&[("apply_patch", vec!["read"], vec![])]);
        let filter = make_filter(vec!["bash"], 5);
        let all_ids = vec!["bash", "read", "apply_patch", "grep"];
        let query_emb = vec![0.5, 0.5, 0.0];
        let mut result = filter.filter(&all_ids, &[], "test", &query_emb);
        // Ensure apply_patch is included before dependency gate
        result.included.insert("apply_patch".into());

        let always_on: HashSet<String> = ["bash".into()].into();
        graph.apply(&mut result, &completed(&[]), 0.15, 0.2, &always_on);

        assert!(!result.included.contains("apply_patch"));
        assert_eq!(result.dependency_exclusions.len(), 1);
        assert_eq!(result.dependency_exclusions[0].tool_id, "apply_patch");
        assert_eq!(result.dependency_exclusions[0].unmet_requires, vec!["read"]);
    }

    #[test]
    fn apply_includes_gated_tool_when_dep_met() {
        let graph = make_dep_graph(&[("apply_patch", vec!["read"], vec![])]);
        let filter = make_filter(vec!["bash"], 5);
        let all_ids = vec!["bash", "read", "apply_patch"];
        let query_emb = vec![0.5, 0.5, 0.0];
        let mut result = filter.filter(&all_ids, &[], "test", &query_emb);
        result.included.insert("apply_patch".into());

        let always_on: HashSet<String> = ["bash".into()].into();
        graph.apply(&mut result, &completed(&["read"]), 0.15, 0.2, &always_on);

        assert!(result.included.contains("apply_patch"));
        assert!(result.dependency_exclusions.is_empty());
    }

    #[test]
    fn apply_deadlock_fallback_when_all_gated() {
        // Build a minimal filter with no embeddings so only bash (always-on) and
        // only_tool (NoEmbedding) are in the result set.
        let filter = ToolSchemaFilter::new(
            vec!["bash".into()],
            5,
            5,
            vec![], // no embeddings: only_tool will be included via NoEmbedding fallback
        );
        let graph = make_dep_graph(&[("only_tool", vec!["missing"], vec![])]);
        let all_ids = vec!["bash", "only_tool"];
        let query_emb = vec![0.5, 0.5, 0.0];
        let mut result = filter.filter(&all_ids, &[], "test", &query_emb);

        // At this point: included = {bash, only_tool}, non_always_on_included = 1
        assert!(result.included.contains("only_tool"));
        assert!(result.included.contains("bash"));

        let always_on: HashSet<String> = ["bash".into()].into();
        graph.apply(&mut result, &completed(&[]), 0.15, 0.2, &always_on);

        // Deadlock fallback: only_tool remains included (all non-always-on would be blocked)
        assert!(result.included.contains("only_tool"));
        assert!(result.dependency_exclusions.is_empty());
    }

    #[test]
    fn apply_always_on_bypasses_gate() {
        let graph = make_dep_graph(&[("bash", vec!["nonexistent"], vec![])]);
        let filter = make_filter(vec!["bash"], 5);
        let all_ids = vec!["bash", "grep"];
        let query_emb = vec![0.5, 0.5, 0.0];
        let mut result = filter.filter(&all_ids, &[], "test", &query_emb);

        let always_on: HashSet<String> = ["bash".into()].into();
        graph.apply(&mut result, &completed(&[]), 0.15, 0.2, &always_on);

        // bash is always-on, bypasses hard gate
        assert!(result.included.contains("bash"));
    }
}
