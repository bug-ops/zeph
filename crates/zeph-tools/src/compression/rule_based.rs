// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Regex-based tool output compressor with self-evolution support.
//!
//! Rules are loaded from `SQLite` and stored as compiled `regex::Regex` values in a
//! `parking_lot::RwLock<Vec<CompiledRule>>`. Hit counts are tracked separately in a
//! `dashmap::DashMap<String, AtomicU64>` so that a rules-vec swap (on reload) cannot
//! lose unflushed counters.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use parking_lot::RwLock;
use zeph_common::ToolName;

use super::{CompressionError, CompressionRuleStore, OutputCompressor, safe_compile};

/// A compiled compression rule ready for matching.
struct CompiledRule {
    id: String,
    /// When `Some`, this rule only applies to tools whose name matches the glob.
    glob: Option<globset::GlobMatcher>,
    pattern: regex::Regex,
    replacement_template: String,
}

/// Regex-based compressor that applies operator- and LLM-evolved rules to tool output.
///
/// Rules are sorted deterministically by `id` to ensure stable application order.
/// Hit counts are stored in `hits` keyed by `rule.id`; the `rules` vec can be swapped
/// on reload without losing any unflushed counts.
///
/// # Invariants
///
/// - Rules are applied in `id`-ascending order (deterministic).
/// - `compress` returns the first successful match (earliest rule wins).
/// - A rule is skipped when `glob` is set and does not match `tool_name`.
/// - `regex::Regex::replace_all` guarantees linear time (no catastrophic backtracking).
///   No `catch_unwind` is needed around `replace_all`.
pub struct RuleBasedCompressor {
    rules: RwLock<Vec<CompiledRule>>,
    hits: DashMap<String, AtomicU64>,
    store: Arc<CompressionRuleStore>,
    max_output_lines: usize,
    regex_timeout_ms: u64,
}

impl std::fmt::Debug for RuleBasedCompressor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuleBasedCompressor")
            .field("rules_count", &self.rules.read().len())
            .field("max_output_lines", &self.max_output_lines)
            .field("regex_timeout_ms", &self.regex_timeout_ms)
            .finish_non_exhaustive()
    }
}

impl RuleBasedCompressor {
    /// Load all active rules from the store and compile them.
    ///
    /// Rules that fail compilation are skipped and logged as warnings.
    ///
    /// `regex_timeout_ms` controls the DoS-safe regex compilation timeout passed to
    /// [`super::safe_compile`]. Sourced from `[tools.compression] regex_compile_timeout_ms`.
    ///
    /// # Errors
    ///
    /// Returns [`CompressionError::Db`] if the store query fails.
    pub async fn load(
        store: Arc<CompressionRuleStore>,
        max_output_lines: usize,
        regex_timeout_ms: u64,
    ) -> Result<Self, CompressionError> {
        let raw_rules = store.list_active().await?;
        let mut compiled = Vec::with_capacity(raw_rules.len());
        let hits = DashMap::new();

        for rule in raw_rules {
            let glob = if let Some(ref g) = rule.tool_glob {
                match globset::Glob::new(g) {
                    Ok(glob) => Some(glob.compile_matcher()),
                    Err(e) => {
                        tracing::warn!(rule_id = %rule.id, pattern = %g, error = %e, "rule: invalid glob, skipping");
                        continue;
                    }
                }
            } else {
                None
            };

            match super::safe_compile(&rule.pattern, regex_timeout_ms).await {
                Ok(re) => {
                    hits.insert(rule.id.clone(), AtomicU64::new(0));
                    compiled.push(CompiledRule {
                        id: rule.id,
                        glob,
                        pattern: re,
                        replacement_template: rule.replacement_template,
                    });
                }
                Err(e) => {
                    tracing::warn!(rule_id = %rule.id, error = %e, "rule: compile failed, skipping");
                }
            }
        }

        compiled.sort_unstable_by(|a, b| a.id.cmp(&b.id));

        Ok(Self {
            rules: RwLock::new(compiled),
            hits,
            store,
            max_output_lines,
            regex_timeout_ms,
        })
    }

    /// Reload rules from the store, preserving hit counts for still-present rules
    /// and flushing counts for rules that no longer exist.
    ///
    /// # Errors
    ///
    /// Returns [`CompressionError::Db`] if the store query fails.
    pub async fn reload(&self) -> Result<(), CompressionError> {
        let raw_rules = self.store.list_active().await?;
        let mut compiled = Vec::with_capacity(raw_rules.len());

        // Flush and remove hits for rules that are no longer in the store.
        let active_ids: std::collections::HashSet<&str> =
            raw_rules.iter().map(|r| r.id.as_str()).collect();
        let stale_ids: Vec<String> = self
            .hits
            .iter()
            .filter(|e| !active_ids.contains(e.key().as_str()))
            .map(|e| e.key().clone())
            .collect();
        for id in stale_ids {
            self.hits.remove(&id);
        }

        for rule in raw_rules {
            let glob = if let Some(ref g) = rule.tool_glob {
                match globset::Glob::new(g) {
                    Ok(glob) => Some(glob.compile_matcher()),
                    Err(e) => {
                        tracing::warn!(rule_id = %rule.id, error = %e, "reload: invalid glob");
                        continue;
                    }
                }
            } else {
                None
            };

            match safe_compile(&rule.pattern, self.regex_timeout_ms).await {
                Ok(re) => {
                    self.hits
                        .entry(rule.id.clone())
                        .or_insert_with(|| AtomicU64::new(0));
                    compiled.push(CompiledRule {
                        id: rule.id,
                        glob,
                        pattern: re,
                        replacement_template: rule.replacement_template,
                    });
                }
                Err(e) => {
                    tracing::warn!(rule_id = %rule.id, error = %e, "reload: compile failed");
                }
            }
        }

        compiled.sort_unstable_by(|a, b| a.id.cmp(&b.id));
        *self.rules.write() = compiled;
        Ok(())
    }

    /// Drain pending hit counts into a batch and write them to the store.
    ///
    /// Called during the `maybe_autodream` maintenance pass. Resets all counters
    /// to zero after flushing.
    ///
    /// # Errors
    ///
    /// Returns a database error if the batch write fails.
    pub async fn flush_hit_counts(&self) -> Result<(), CompressionError> {
        let batch: Vec<(String, u64)> = self
            .hits
            .iter()
            .filter_map(|e| {
                let delta = e.value().swap(0, Ordering::Relaxed);
                if delta > 0 {
                    Some((e.key().clone(), delta))
                } else {
                    None
                }
            })
            .collect();

        if batch.is_empty() {
            return Ok(());
        }

        self.store.increment_hits(&batch).await?;
        Ok(())
    }
}

impl OutputCompressor for RuleBasedCompressor {
    fn compress<'a>(
        &'a self,
        tool_name: &'a ToolName,
        output: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, CompressionError>> + Send + 'a>> {
        Box::pin(async move {
            // Drop span guard before first await; EnteredSpan is not Send.
            drop(
                tracing::info_span!("tools.compression.compress", tool = %tool_name.as_str())
                    .entered(),
            );
            let rules = self.rules.read();
            for rule in rules.iter() {
                if rule
                    .glob
                    .as_ref()
                    .is_some_and(|g| !g.is_match(tool_name.as_str()))
                {
                    continue;
                }
                if rule.pattern.is_match(output) {
                    let compressed = rule
                        .pattern
                        .replace_all(output, rule.replacement_template.as_str())
                        .into_owned();

                    if compressed.len() < output.len() {
                        if let Some(entry) = self.hits.get(&rule.id) {
                            entry.fetch_add(1, Ordering::Relaxed);
                        }
                        tracing::debug!(
                            rule_id = %rule.id,
                            tool = %tool_name.as_str(),
                            original_bytes = output.len(),
                            compressed_bytes = compressed.len(),
                            "compression applied"
                        );
                        return Ok(Some(compressed));
                    }
                }
            }
            Ok(None)
        })
    }

    fn name(&self) -> &'static str {
        "rule_based"
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zeph_common::ToolName;

    use super::*;
    use crate::compression::{CompressionRuleStore, OutputCompressor, store::CompressionRule};

    async fn make_store_with_rules(rules: &[(&str, &str)]) -> Arc<CompressionRuleStore> {
        let pool = sqlx::SqlitePool::connect(":memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE compression_rules (\
             id TEXT PRIMARY KEY, tool_glob TEXT, pattern TEXT NOT NULL, \
             replacement_template TEXT NOT NULL, hit_count INTEGER NOT NULL DEFAULT 0, \
             source TEXT NOT NULL DEFAULT 'operator', created_at TEXT NOT NULL, \
             UNIQUE(tool_glob, pattern))",
        )
        .execute(&pool)
        .await
        .unwrap();

        let store = Arc::new(CompressionRuleStore::new(Arc::new(pool)));
        for (i, (pattern, replacement)) in rules.iter().enumerate() {
            store
                .upsert(&CompressionRule {
                    id: format!("rule-{i}"),
                    tool_glob: None,
                    pattern: (*pattern).to_owned(),
                    replacement_template: (*replacement).to_owned(),
                    hit_count: 0,
                    source: "operator".to_owned(),
                    created_at: "2026-01-01T00:00:00Z".to_owned(),
                })
                .await
                .unwrap();
        }
        store
    }

    #[tokio::test]
    async fn compress_returns_none_when_no_rule_matches() {
        let store = make_store_with_rules(&[(r"\d+", "N")]).await;
        let compressor = RuleBasedCompressor::load(store, 2, 500).await.unwrap();
        let tool = ToolName::new("shell");
        // Input has no digits — pattern won't match.
        let input = "line\n".repeat(10);
        let result = compressor.compress(&tool, &input).await.unwrap();
        assert!(result.is_none(), "no rule should match non-digit input");
    }

    #[tokio::test]
    async fn compress_applies_matching_rule() {
        // Replace every digit sequence with "N".
        let store = make_store_with_rules(&[(r"\d+", "N")]).await;
        let compressor = RuleBasedCompressor::load(store, 2, 500).await.unwrap();
        let tool = ToolName::new("shell");
        // 10 lines, each "12345\n" → replaced with "N\n".
        let input: String = "12345\n".repeat(10);
        let result = compressor.compress(&tool, &input).await.unwrap();
        assert!(result.is_some(), "rule should have matched");
        let compressed = result.unwrap();
        assert!(compressed.len() < input.len(), "compressed must be shorter");
        assert!(compressed.contains('N'));
    }

    #[tokio::test]
    async fn compress_returns_none_when_not_shorter() {
        // Replacement is longer than original — should not be returned.
        let long_replacement = "VERY_LONG_REPLACEMENT_THAT_IS_DEFINITELY_LONGER_THAN_ORIGINAL";
        let store = make_store_with_rules(&[(r"\d", long_replacement)]).await;
        let compressor = RuleBasedCompressor::load(store, 2, 500).await.unwrap();
        let tool = ToolName::new("shell");
        let input = "1\n".repeat(10);
        let result = compressor.compress(&tool, &input).await.unwrap();
        assert!(
            result.is_none(),
            "compression that doesn't reduce size should return None"
        );
    }
}
