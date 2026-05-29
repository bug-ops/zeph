// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Acon tool-result compression (#4021).
//!
//! Stateless, pure-function compression pass that enforces per-result and batch-level
//! token budgets on tool outputs **before** they enter message history. This runs as a
//! pre-processing step in `zeph-core`'s tier loop, not as part of context assembly.
//!
//! # Compression model
//!
//! - Results below `passthrough_threshold`: returned unchanged (`PassThrough`).
//! - Results above `passthrough_threshold`: char-truncated to approximately
//!   `passthrough_threshold` tokens with a `" [...truncated]"` suffix (`Truncated`).
//!   The suffix adds ~3–4 tokens, so `compressed_tokens` may slightly exceed
//!   `passthrough_threshold` — callers must not rely on exact equality.
//! - LLM summarization is **not** performed here — it is the caller's responsibility in
//!   `zeph-core`. The caller may pre-summarize a result and then pass the shortened text
//!   to `compress_single` or `compress_batch`.
//! - After per-result compression, `compress_batch` enforces the `total_budget` cap by
//!   proportionally trimming the largest results (`BatchTrimmed`).

use zeph_common::memory::TokenCounting;
use zeph_config::AconConfig;

const TRUNCATION_MARKER: &str = " [...truncated]";

/// Configuration for Acon tool-result compression.
///
/// Constructed from [`AconConfig`] (zeph-config) at session init via the [`From`] impl.
#[derive(Debug, Clone)]
pub struct ToolResultCompressionConfig {
    /// Token count below which results pass through unchanged.
    /// Also the approximate truncation target: results above this are char-truncated to
    /// approximately this many tokens (the `" [...truncated]"` suffix adds ~3–4 tokens).
    /// Default: `2000`.
    pub passthrough_threshold: usize,
    /// Token count above which the caller should attempt LLM summarization before
    /// falling back to truncation. Not enforced here — informational for the caller.
    /// Default: `4000`.
    pub summarize_threshold: usize,
    /// Maximum total tokens for all tool results combined in a single turn. Default: `8000`.
    pub total_budget: usize,
}

impl From<&AconConfig> for ToolResultCompressionConfig {
    fn from(cfg: &AconConfig) -> Self {
        Self {
            passthrough_threshold: cfg.passthrough_threshold,
            summarize_threshold: cfg.summarize_threshold,
            total_budget: cfg.total_budget,
        }
    }
}

/// A tool result entry before compression, used as input to `compress_batch`.
///
/// The `index` field is used as a deterministic tiebreaker when two results have
/// equal token counts during batch budget enforcement (lower index trimmed first).
pub struct ToolResultEntry<'a> {
    /// Tool name for tracing and logging.
    pub tool_name: &'a str,
    /// Raw tool result text.
    pub text: &'a str,
    /// Position in the original tool call list. Used as tiebreaker in batch trimming.
    pub index: usize,
}

/// Method applied when compressing a single tool result.
///
/// Does NOT include a `Summarized` variant — LLM summarization is the caller's
/// responsibility in `zeph-core` before calling these methods.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionMethod {
    /// Result was within `passthrough_threshold` — returned unchanged.
    PassThrough,
    /// Result was truncated at a char boundary to approximately `passthrough_threshold` tokens.
    /// The `" [...truncated]"` suffix adds ~3–4 tokens, so `compressed_tokens` may slightly
    /// exceed `passthrough_threshold`.
    Truncated,
    /// Result was proportionally trimmed during batch budget enforcement.
    BatchTrimmed,
}

/// Output of compressing a single tool result.
#[derive(Debug, Clone)]
pub struct CompressedToolResult {
    /// Compressed (or unchanged) text.
    pub text: String,
    /// Token count before compression.
    pub original_tokens: usize,
    /// Token count after compression.
    pub compressed_tokens: usize,
    /// Method applied.
    pub method: CompressionMethod,
}

/// Stateless tool-result compressor for Acon (#4021).
///
/// All methods are pure functions: they take text, a token counter, and a config, and
/// return compressed text with metadata. No I/O, no async, no agent state.
///
/// # Examples
///
/// ```
/// use zeph_context::tool_result_compress::{
///     ToolResultCompressor, ToolResultCompressionConfig, CompressionMethod,
/// };
///
/// struct WordCounter;
/// impl zeph_common::memory::TokenCounting for WordCounter {
///     fn count_tokens(&self, text: &str) -> usize { text.split_whitespace().count() }
///     fn count_tool_schema_tokens(&self, _schema: &serde_json::Value) -> usize { 0 }
/// }
///
/// let config = ToolResultCompressionConfig {
///     passthrough_threshold: 5,
///     summarize_threshold: 10,
///     total_budget: 20,
/// };
/// let tc = WordCounter;
///
/// let result = ToolResultCompressor::compress_single("hello world", &tc, &config);
/// assert_eq!(result.method, CompressionMethod::PassThrough);
/// ```
pub struct ToolResultCompressor;

impl ToolResultCompressor {
    /// Compress a single tool result text.
    ///
    /// - Below `passthrough_threshold` tokens: returned unchanged.
    /// - At or above `passthrough_threshold` tokens: char-truncated so that the truncated
    ///   text has approximately `passthrough_threshold` tokens (using a char-boundary-safe
    ///   cut at `passthrough_threshold * 4` bytes), with `" [...truncated]"` appended.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_context::tool_result_compress::{
    ///     ToolResultCompressor, ToolResultCompressionConfig, CompressionMethod,
    /// };
    ///
    /// struct WordCounter;
    /// impl zeph_common::memory::TokenCounting for WordCounter {
    ///     fn count_tokens(&self, text: &str) -> usize { text.split_whitespace().count() }
    ///     fn count_tool_schema_tokens(&self, _schema: &serde_json::Value) -> usize { 0 }
    /// }
    ///
    /// let config = ToolResultCompressionConfig {
    ///     passthrough_threshold: 3,
    ///     summarize_threshold: 10,
    ///     total_budget: 20,
    /// };
    /// let tc = WordCounter;
    ///
    /// // Short text passes through.
    /// let r = ToolResultCompressor::compress_single("one two three", &tc, &config);
    /// assert_eq!(r.method, CompressionMethod::PassThrough);
    ///
    /// // Long text is truncated.
    /// let r = ToolResultCompressor::compress_single("one two three four five", &tc, &config);
    /// assert_eq!(r.method, CompressionMethod::Truncated);
    /// assert!(r.text.ends_with("[...truncated]"));
    /// ```
    #[must_use]
    pub fn compress_single(
        text: &str,
        tc: &dyn TokenCounting,
        config: &ToolResultCompressionConfig,
    ) -> CompressedToolResult {
        let original_tokens = tc.count_tokens(text);

        if original_tokens <= config.passthrough_threshold {
            return CompressedToolResult {
                text: text.to_owned(),
                original_tokens,
                compressed_tokens: original_tokens,
                method: CompressionMethod::PassThrough,
            };
        }

        // Truncate at a char boundary. The heuristic is ~4 bytes per token.
        // Subtract the marker's byte length so the final result (text + marker) fits within
        // the passthrough_threshold token budget — without this the marker inflates the count.
        let byte_limit = config
            .passthrough_threshold
            .saturating_mul(4)
            .saturating_sub(TRUNCATION_MARKER.len());
        let cut = text.floor_char_boundary(byte_limit.min(text.len()));
        let truncated = format!("{}{}", &text[..cut], TRUNCATION_MARKER);
        let compressed_tokens = tc.count_tokens(&truncated);

        CompressedToolResult {
            text: truncated,
            original_tokens,
            compressed_tokens,
            method: CompressionMethod::Truncated,
        }
    }

    /// Compress a batch of tool results, enforcing both per-result and total-budget limits.
    ///
    /// 1. Applies `compress_single` to each entry.
    /// 2. If the total compressed tokens still exceed `total_budget`, trims results in
    ///    descending token-count order. Ties are broken by `entry.index` (lower index
    ///    trimmed first) for deterministic output.
    ///
    /// Returns one `CompressedToolResult` per input entry, in the same order.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_context::tool_result_compress::{
    ///     ToolResultCompressor, ToolResultCompressionConfig, ToolResultEntry, CompressionMethod,
    /// };
    ///
    /// struct WordCounter;
    /// impl zeph_common::memory::TokenCounting for WordCounter {
    ///     fn count_tokens(&self, text: &str) -> usize { text.split_whitespace().count() }
    ///     fn count_tool_schema_tokens(&self, _schema: &serde_json::Value) -> usize { 0 }
    /// }
    ///
    /// let config = ToolResultCompressionConfig {
    ///     passthrough_threshold: 100,
    ///     summarize_threshold: 200,
    ///     total_budget: 5,
    /// };
    /// let tc = WordCounter;
    /// let entries = vec![
    ///     ToolResultEntry { tool_name: "shell", text: "one two three", index: 0 },
    ///     ToolResultEntry { tool_name: "fetch", text: "four five six", index: 1 },
    /// ];
    /// let results = ToolResultCompressor::compress_batch(&entries, &tc, &config);
    /// assert_eq!(results.len(), 2);
    /// // Combined tokens (6) exceed total_budget (5) → at least one is BatchTrimmed.
    /// assert!(results.iter().any(|r| r.method == CompressionMethod::BatchTrimmed));
    /// ```
    #[must_use]
    pub fn compress_batch(
        entries: &[ToolResultEntry<'_>],
        tc: &dyn TokenCounting,
        config: &ToolResultCompressionConfig,
    ) -> Vec<CompressedToolResult> {
        if entries.is_empty() {
            return Vec::new();
        }

        // Phase 1: per-result compression.
        let mut results: Vec<CompressedToolResult> = entries
            .iter()
            .map(|e| Self::compress_single(e.text, tc, config))
            .collect();

        // Phase 2: batch budget enforcement.
        let total_tokens: usize = results.iter().map(|r| r.compressed_tokens).sum();
        if total_tokens <= config.total_budget {
            return results;
        }

        // Build a sorted list of (compressed_tokens, original_index) to trim the largest first.
        // Tiebreaker: lower input index is trimmed first (critic note M3).
        let mut order: Vec<usize> = (0..results.len()).collect();
        order.sort_unstable_by(|&a, &b| {
            let ta = results[a].compressed_tokens;
            let tb = results[b].compressed_tokens;
            // Descending by tokens, then ascending by index for ties.
            tb.cmp(&ta)
                .then_with(|| entries[a].index.cmp(&entries[b].index))
        });

        let mut remaining = total_tokens;
        for &idx in &order {
            if remaining <= config.total_budget {
                break;
            }
            let current = results[idx].compressed_tokens;
            // Shrink this result proportionally so the total fits.
            let excess = remaining.saturating_sub(config.total_budget);
            let target_tokens = current.saturating_sub(excess.min(current));
            // target_tokens == 0 means we'd remove the entire result — keep a minimal stub.
            let byte_limit = target_tokens.max(1).saturating_mul(4);
            let cut = results[idx]
                .text
                .floor_char_boundary(byte_limit.min(results[idx].text.len()));
            let trimmed = format!("{} [...truncated]", &results[idx].text[..cut]);
            let new_tokens = tc.count_tokens(&trimmed);
            remaining = remaining.saturating_sub(current).saturating_add(new_tokens);
            results[idx].compressed_tokens = new_tokens;
            results[idx].text = trimmed;
            results[idx].method = CompressionMethod::BatchTrimmed;
        }

        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct WordCounter;
    impl TokenCounting for WordCounter {
        fn count_tokens(&self, text: &str) -> usize {
            text.split_whitespace().count()
        }

        fn count_tool_schema_tokens(&self, _schema: &serde_json::Value) -> usize {
            0
        }
    }

    fn cfg(passthrough: usize, summarize: usize, budget: usize) -> ToolResultCompressionConfig {
        ToolResultCompressionConfig {
            passthrough_threshold: passthrough,
            summarize_threshold: summarize,
            total_budget: budget,
        }
    }

    #[test]
    fn compress_single_passthrough_below_threshold() {
        let tc = WordCounter;
        let config = cfg(10, 20, 40);
        let r = ToolResultCompressor::compress_single("one two three", &tc, &config);
        assert_eq!(r.method, CompressionMethod::PassThrough);
        assert_eq!(r.text, "one two three");
        assert_eq!(r.original_tokens, r.compressed_tokens);
    }

    #[test]
    fn compress_single_passthrough_at_exact_threshold() {
        let tc = WordCounter;
        // "a b c" = 3 words; threshold = 3 → passthrough.
        let config = cfg(3, 10, 20);
        let r = ToolResultCompressor::compress_single("a b c", &tc, &config);
        assert_eq!(r.method, CompressionMethod::PassThrough);
    }

    #[test]
    fn compress_single_truncated_above_threshold() {
        let tc = WordCounter;
        let config = cfg(2, 10, 20);
        let text = "one two three four five";
        let r = ToolResultCompressor::compress_single(text, &tc, &config);
        assert_eq!(r.method, CompressionMethod::Truncated);
        assert!(r.text.ends_with("[...truncated]"));
        assert!(r.compressed_tokens <= r.original_tokens);
    }

    #[test]
    fn compress_single_empty_text_passthrough() {
        let tc = WordCounter;
        let config = cfg(5, 10, 20);
        let r = ToolResultCompressor::compress_single("", &tc, &config);
        assert_eq!(r.method, CompressionMethod::PassThrough);
        assert_eq!(r.text, "");
    }

    #[test]
    fn compress_batch_empty_input() {
        let tc = WordCounter;
        let config = cfg(5, 10, 20);
        let results = ToolResultCompressor::compress_batch(&[], &tc, &config);
        assert!(results.is_empty());
    }

    #[test]
    fn compress_batch_within_budget_no_batch_trim() {
        let tc = WordCounter;
        let config = cfg(100, 200, 1000);
        let entries = vec![
            ToolResultEntry {
                tool_name: "a",
                text: "one two",
                index: 0,
            },
            ToolResultEntry {
                tool_name: "b",
                text: "three four",
                index: 1,
            },
        ];
        let results = ToolResultCompressor::compress_batch(&entries, &tc, &config);
        assert!(
            results
                .iter()
                .all(|r| r.method != CompressionMethod::BatchTrimmed),
            "no batch trimming expected within budget"
        );
    }

    #[test]
    fn compress_batch_exceeds_budget_trims_largest_first() {
        let tc = WordCounter;
        // budget = 3; two entries totaling 6 words.
        let config = cfg(100, 200, 3);
        let entries = vec![
            ToolResultEntry {
                tool_name: "a",
                text: "one two three",
                index: 0,
            }, // 3 tokens
            ToolResultEntry {
                tool_name: "b",
                text: "four five six",
                index: 1,
            }, // 3 tokens
        ];
        let results = ToolResultCompressor::compress_batch(&entries, &tc, &config);
        assert_eq!(results.len(), 2);
        // At least one must be BatchTrimmed.
        assert!(
            results
                .iter()
                .any(|r| r.method == CompressionMethod::BatchTrimmed)
        );
        // Total must not exceed budget (within rounding from the truncation marker).
        let total: usize = results.iter().map(|r| r.compressed_tokens).sum();
        assert!(
            total <= config.total_budget + 3,
            "total {total} should be near budget {}",
            config.total_budget
        );
    }

    #[test]
    fn compress_batch_tiebreaker_lower_index_trimmed_first() {
        let tc = WordCounter;
        // Both entries have the same token count. budget = 3 < 6 total.
        // Lower index (0) should be trimmed first.
        let config = cfg(100, 200, 3);
        let entries = vec![
            ToolResultEntry {
                tool_name: "a",
                text: "one two three",
                index: 0,
            },
            ToolResultEntry {
                tool_name: "b",
                text: "four five six",
                index: 1,
            },
        ];
        let results = ToolResultCompressor::compress_batch(&entries, &tc, &config);
        // Index 0 should be BatchTrimmed (lower index trimmed first on tie).
        assert_eq!(
            results[0].method,
            CompressionMethod::BatchTrimmed,
            "lower index must be trimmed first on equal token counts"
        );
    }

    #[test]
    fn acon_config_default_into_compression_config() {
        let acon = AconConfig::default();
        let cfg = ToolResultCompressionConfig::from(&acon);
        assert_eq!(cfg.passthrough_threshold, 2000);
        assert_eq!(cfg.summarize_threshold, 4000);
        assert_eq!(cfg.total_budget, 8000);
    }
}
