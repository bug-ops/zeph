// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::VecDeque;
use std::time::Duration;

use zeph_tools::{OverflowConfig, ResultCacheConfig, TafcConfig, ToolResultCache};

use super::DOOM_LOOP_WINDOW;

pub(crate) struct ToolOrchestrator {
    pub(super) doom_loop_history: Vec<u64>,
    pub(super) max_iterations: usize,
    pub(super) summarize_tool_output_enabled: bool,
    pub(super) overflow_config: OverflowConfig,
    /// Sliding window of recent (`tool_name`, `args_hash`) pairs for repeat-detection.
    /// Only LLM-initiated calls are recorded here — retry re-executions are excluded.
    /// Window capacity = 2 * `repeat_threshold`.
    pub(super) recent_tool_calls: VecDeque<(String, u64)>,
    /// Number of identical (`tool_name`, `args_hash`) appearances in the window required
    /// to trigger repeat-detection abort. 0 = disabled.
    pub(super) repeat_threshold: usize,
    /// Max retries for transient errors per tool call. 0 = disabled.
    pub(super) max_tool_retries: usize,
    /// Maximum wall-clock time (seconds) to spend on retries for a single tool call.
    /// 0 = no wall-clock budget (only `max_tool_retries` applies).
    pub(super) max_retry_duration_secs: u64,
    /// Pre-execution verifiers run before every native tool call (`TrustBench` pattern,
    /// issue #1630). Stored here rather than on `SecurityState` because they are tool-layer
    /// concerns: they inspect tool arguments at dispatch time, consistent with
    /// repeat-detection, rate-limiting, and overflow controls which also live here.
    pub(super) pre_execution_verifiers: Vec<Box<dyn zeph_tools::PreExecutionVerifier>>,
    /// Think-Augmented Function Calling configuration.
    pub(crate) tafc: TafcConfig,
    /// Session-scoped cache for tool results. Persists across tool rounds within a session;
    /// reset only on `/clear`. Unlike repeat-detection and doom-loop state (which reset per
    /// round), the cache is intentionally long-lived — its value comes from reuse across turns.
    pub(super) result_cache: ToolResultCache,
}

/// Truncate a tool name to at most 256 bytes, respecting UTF-8 char boundaries.
///
/// Used by both `push_tool_call` and `is_repeat` to ensure stored and queried
/// names always match when the original name exceeds the limit.
fn truncate_tool_name(name: &str) -> &str {
    const MAX_TOOL_NAME_BYTES: usize = 256;
    if name.len() <= MAX_TOOL_NAME_BYTES {
        return name;
    }
    let mut idx = MAX_TOOL_NAME_BYTES;
    while !name.is_char_boundary(idx) {
        idx -= 1;
    }
    &name[..idx]
}

impl ToolOrchestrator {
    #[must_use]
    pub(crate) fn new() -> Self {
        let repeat_threshold = 2_usize;
        Self {
            doom_loop_history: Vec::new(),
            max_iterations: 10,
            summarize_tool_output_enabled: false,
            overflow_config: OverflowConfig::default(),
            recent_tool_calls: VecDeque::with_capacity(2 * repeat_threshold),
            repeat_threshold,
            max_tool_retries: 2,
            max_retry_duration_secs: 30,
            pre_execution_verifiers: Vec::new(),
            tafc: TafcConfig::default(),
            result_cache: ToolResultCache::new(true, Some(Duration::from_secs(300))),
        }
    }

    /// Initialize the result cache from config.
    pub(crate) fn set_cache_config(&mut self, config: &ResultCacheConfig) {
        let ttl = if config.ttl_secs == 0 {
            None // ttl_secs = 0 → never expire
        } else {
            Some(Duration::from_secs(config.ttl_secs))
        };
        self.result_cache = ToolResultCache::new(config.enabled, ttl);
    }

    /// Clear the result cache. Called on `/clear`.
    pub(crate) fn clear_cache(&mut self) {
        self.result_cache.clear();
    }

    /// Returns a formatted cache stats string for `/cache-stats` command.
    pub(crate) fn cache_stats(&self) -> String {
        let cache = &self.result_cache;
        let status = if cache.is_enabled() {
            "enabled"
        } else {
            "disabled"
        };
        let hits = cache.hits();
        let misses = cache.misses();
        let total = hits + misses;
        #[allow(clippy::cast_precision_loss)]
        let hit_rate = if total > 0 {
            format!("{:.1}%", (hits as f64 / total as f64) * 100.0)
        } else {
            "n/a".to_owned()
        };
        let ttl_display = if cache.ttl_secs() == 0 {
            "never".to_owned()
        } else {
            format!("{}s", cache.ttl_secs())
        };
        format!(
            "Tool result cache: {status}\nEntries: {}, Hits: {hits}, Misses: {misses}, Hit rate: {hit_rate}\nTTL: {ttl_display}",
            cache.len(),
        )
    }

    pub(super) fn push_doom_hash(&mut self, hash: u64) {
        self.doom_loop_history.push(hash);
    }

    pub(super) fn clear_doom_history(&mut self) {
        self.doom_loop_history.clear();
    }

    /// Reset the repeat-detection sliding window between user turns.
    pub(super) fn clear_recent_tool_calls(&mut self) {
        self.recent_tool_calls.clear();
    }

    /// Returns `true` if the last `DOOM_LOOP_WINDOW` hashes are identical.
    pub(super) fn is_doom_loop(&self) -> bool {
        if self.doom_loop_history.len() < DOOM_LOOP_WINDOW {
            return false;
        }
        let recent = &self.doom_loop_history[self.doom_loop_history.len() - DOOM_LOOP_WINDOW..];
        recent.windows(2).all(|w| w[0] == w[1])
    }

    /// Record a tool call (LLM-initiated only — not retry re-executions).
    ///
    /// Maintains a sliding window of size `2 * repeat_threshold`.
    /// Tool names are truncated to 256 bytes to prevent unbounded memory growth
    /// from adversarially long names.
    pub(super) fn push_tool_call(&mut self, name: &str, args_hash: u64) {
        if self.repeat_threshold == 0 {
            return;
        }
        let window = 2 * self.repeat_threshold;
        if self.recent_tool_calls.len() >= window {
            self.recent_tool_calls.pop_front();
        }
        self.recent_tool_calls
            .push_back((truncate_tool_name(name).to_owned(), args_hash)); // lgtm[rust/cleartext-logging]
    }

    /// Returns `true` if the same `(name, args_hash)` pair appears `>= repeat_threshold`
    /// times in the current window.
    ///
    /// Applies the same 256-byte name truncation as `push_tool_call` so that long names
    /// are correctly matched against stored (truncated) entries.
    pub(super) fn is_repeat(&self, name: &str, args_hash: u64) -> bool {
        if self.repeat_threshold == 0 {
            return false;
        }
        let name = truncate_tool_name(name);
        let count = self
            .recent_tool_calls
            .iter()
            .filter(|(n, h)| n == name && *h == args_hash)
            .count();
        count >= self.repeat_threshold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_defaults() {
        let o = ToolOrchestrator::new();
        assert!(o.doom_loop_history.is_empty());
        assert_eq!(o.max_iterations, 10);
        assert!(!o.summarize_tool_output_enabled);
        assert!(o.recent_tool_calls.is_empty());
        assert_eq!(o.repeat_threshold, 2);
        assert_eq!(o.max_tool_retries, 2);
        assert_eq!(o.max_retry_duration_secs, 30);
    }

    #[test]
    fn is_doom_loop_insufficient_history() {
        let mut o = ToolOrchestrator::new();
        o.push_doom_hash(42);
        o.push_doom_hash(42);
        assert!(!o.is_doom_loop());
    }

    #[test]
    fn is_doom_loop_identical_hashes() {
        let mut o = ToolOrchestrator::new();
        o.push_doom_hash(7);
        o.push_doom_hash(7);
        o.push_doom_hash(7);
        assert!(o.is_doom_loop());
    }

    #[test]
    fn is_doom_loop_mixed_hashes() {
        let mut o = ToolOrchestrator::new();
        o.push_doom_hash(1);
        o.push_doom_hash(2);
        o.push_doom_hash(3);
        assert!(!o.is_doom_loop());
    }

    #[test]
    fn is_doom_loop_only_recent_window_matters() {
        let mut o = ToolOrchestrator::new();
        o.push_doom_hash(1);
        o.push_doom_hash(2);
        o.push_doom_hash(9);
        o.push_doom_hash(9);
        o.push_doom_hash(9);
        assert!(o.is_doom_loop());
    }

    #[test]
    fn clear_doom_history_resets() {
        let mut o = ToolOrchestrator::new();
        o.push_doom_hash(5);
        o.push_doom_hash(5);
        o.push_doom_hash(5);
        assert!(o.is_doom_loop());
        o.clear_doom_history();
        assert!(!o.is_doom_loop());
        assert!(o.doom_loop_history.is_empty());
    }

    // Repeat-detection tests

    #[test]
    fn repeat_detection_no_repeat_before_threshold() {
        let mut o = ToolOrchestrator::new();
        o.push_tool_call("bash", 42);
        // Only 1 occurrence, threshold is 2 — not a repeat yet
        assert!(!o.is_repeat("bash", 42));
    }

    #[test]
    fn repeat_detection_triggers_at_threshold() {
        let mut o = ToolOrchestrator::new();
        o.push_tool_call("bash", 42);
        o.push_tool_call("bash", 42);
        // 2 occurrences >= threshold 2 → repeat
        assert!(o.is_repeat("bash", 42));
    }

    #[test]
    fn repeat_detection_different_args_no_repeat() {
        let mut o = ToolOrchestrator::new();
        o.push_tool_call("bash", 1);
        o.push_tool_call("bash", 2);
        assert!(!o.is_repeat("bash", 1));
        assert!(!o.is_repeat("bash", 2));
    }

    #[test]
    fn repeat_detection_different_tool_no_repeat() {
        let mut o = ToolOrchestrator::new();
        o.push_tool_call("bash", 42);
        o.push_tool_call("read", 42);
        assert!(!o.is_repeat("bash", 42));
        assert!(!o.is_repeat("read", 42));
    }

    #[test]
    fn repeat_detection_window_evicts_old_entries() {
        let mut o = ToolOrchestrator::new();
        // Window size = 2 * threshold = 4
        o.push_tool_call("bash", 42);
        o.push_tool_call("read", 1);
        o.push_tool_call("read", 2);
        o.push_tool_call("read", 3);
        // Now push another entry — "bash:42" should be evicted from front
        o.push_tool_call("read", 4);
        // "bash:42" was in the window once, now evicted → not a repeat
        assert!(!o.is_repeat("bash", 42));
    }

    #[test]
    fn repeat_detection_disabled_when_threshold_zero() {
        let mut o = ToolOrchestrator::new();
        o.repeat_threshold = 0;
        o.push_tool_call("bash", 42);
        o.push_tool_call("bash", 42);
        o.push_tool_call("bash", 42);
        // Threshold 0 means disabled
        assert!(!o.is_repeat("bash", 42));
    }

    // ── SEC-003: tool name truncation ─────────────────────────────────────────

    #[test]
    fn push_tool_call_long_name_truncated_to_256_bytes() {
        let mut o = ToolOrchestrator::new();
        // Name well above 256 bytes
        let long_name = "a".repeat(512);
        o.push_tool_call(&long_name, 99);
        let stored = &o.recent_tool_calls[0].0;
        assert_eq!(stored.len(), 256, "stored name must be exactly 256 bytes");
        assert!(
            stored.is_char_boundary(stored.len()),
            "truncation must land on char boundary"
        );
    }

    #[test]
    fn push_tool_call_unicode_name_truncated_at_char_boundary() {
        let mut o = ToolOrchestrator::new();
        // Each '日' is 3 bytes. 256 / 3 = 85 full chars = 255 bytes.
        // Appending one more gives 258 bytes total — must truncate to 255.
        let base: String = "日".repeat(85); // 255 bytes
        let long_name = format!("{base}日"); // 258 bytes — crosses 256-byte boundary
        o.push_tool_call(&long_name, 1);
        let stored = &o.recent_tool_calls[0].0;
        assert!(stored.len() <= 256, "stored name must not exceed 256 bytes");
        assert!(
            stored.is_char_boundary(stored.len()),
            "must be valid UTF-8 boundary"
        );
    }

    #[test]
    fn push_tool_call_short_name_not_truncated() {
        let mut o = ToolOrchestrator::new();
        let short_name = "shell";
        o.push_tool_call(short_name, 7);
        assert_eq!(o.recent_tool_calls[0].0, short_name);
    }

    #[test]
    fn push_tool_call_300_byte_name_truncated_to_at_most_256() {
        // SEC-003: specifically test with 300-byte name as the boundary case
        let mut o = ToolOrchestrator::new();
        let name_300 = "x".repeat(300);
        o.push_tool_call(&name_300, 42);
        let stored = &o.recent_tool_calls[0].0;
        assert!(
            stored.len() <= 256,
            "300-byte name must be stored as ≤256 bytes, got {}",
            stored.len()
        );
        assert!(stored.is_char_boundary(stored.len()));
    }

    // ── SEC-004: retry budget field and logic ─────────────────────────────────

    #[test]
    fn max_retry_duration_secs_default_is_30() {
        // SEC-004: verify the budget field is set at construction time
        let o = ToolOrchestrator::new();
        assert_eq!(o.max_retry_duration_secs, 30);
    }

    #[test]
    fn retry_budget_condition_zero_disables_check() {
        // SEC-004: when max_retry_duration_secs == 0, the budget check must be skipped.
        // This mirrors the `if max_retry_duration_secs > 0` guard in native.rs.
        let budget_secs: u64 = 0;
        // Simulate that 60 seconds have elapsed — budget check is disabled.
        let elapsed_secs: u64 = 60;
        let budget_exceeded = budget_secs > 0 && elapsed_secs >= budget_secs;
        assert!(
            !budget_exceeded,
            "budget=0 must disable the wall-clock check"
        );
    }

    #[test]
    fn retry_budget_condition_exceeded_triggers_break() {
        // SEC-004: simulate elapsed > budget with a real Instant to confirm the
        // condition that native.rs evaluates before breaking the retry loop.
        let budget_secs: u64 = 1;
        // Subtract 2 seconds to ensure elapsed >= budget without sleeping.
        let retry_start = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(2))
            .unwrap();
        let elapsed_secs = retry_start.elapsed().as_secs();
        let budget_exceeded = budget_secs > 0 && elapsed_secs >= budget_secs;
        assert!(
            budget_exceeded,
            "elapsed {elapsed_secs}s should exceed budget {budget_secs}s"
        );
    }

    // ── cache_stats() display ─────────────────────────────────────────────────

    #[test]
    fn cache_stats_disabled_shows_disabled_status() {
        let mut o = ToolOrchestrator::new();
        o.set_cache_config(&ResultCacheConfig {
            enabled: false,
            ttl_secs: 300,
        });
        let stats = o.cache_stats();
        assert!(
            stats.contains("disabled"),
            "expected 'disabled' in: {stats}"
        );
    }

    #[test]
    fn cache_stats_no_calls_shows_na_hit_rate() {
        let o = ToolOrchestrator::new();
        let stats = o.cache_stats();
        assert!(
            stats.contains("n/a"),
            "expected 'n/a' hit rate when total=0, got: {stats}"
        );
    }

    #[test]
    fn cache_stats_ttl_zero_shows_never() {
        let mut o = ToolOrchestrator::new();
        o.set_cache_config(&ResultCacheConfig {
            enabled: true,
            ttl_secs: 0,
        });
        let stats = o.cache_stats();
        assert!(
            stats.contains("never"),
            "expected 'never' TTL display for ttl_secs=0, got: {stats}"
        );
    }

    #[test]
    fn cache_stats_hit_rate_percentage() {
        use zeph_tools::CacheKey;
        let mut o = ToolOrchestrator::new();
        // Directly manipulate the cache to simulate 1 hit and 1 miss.
        // put() one entry, get() it (hit), then get() a missing key (miss).
        let output = zeph_tools::ToolOutput {
            tool_name: "read".to_owned(),
            summary: "contents".to_owned(),
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
        };
        o.result_cache.put(CacheKey::new("read", 1), output);
        o.result_cache.get(&CacheKey::new("read", 1)); // hit
        o.result_cache.get(&CacheKey::new("read", 99)); // miss
        let stats = o.cache_stats();
        assert!(
            stats.contains("50.0%"),
            "expected 50.0% hit rate (1 hit / 2 total), got: {stats}"
        );
        assert!(
            stats.contains("Hits: 1"),
            "expected 'Hits: 1', got: {stats}"
        );
        assert!(
            stats.contains("Misses: 1"),
            "expected 'Misses: 1', got: {stats}"
        );
    }

    #[test]
    fn set_cache_config_ttl_mapping() {
        let mut o = ToolOrchestrator::new();
        // ttl_secs = 0 → None (never expire) → ttl_secs() returns 0
        o.set_cache_config(&ResultCacheConfig {
            enabled: true,
            ttl_secs: 0,
        });
        assert_eq!(o.result_cache.ttl_secs(), 0);

        // ttl_secs = 60 → Some(60s) → ttl_secs() returns 60
        o.set_cache_config(&ResultCacheConfig {
            enabled: true,
            ttl_secs: 60,
        });
        assert_eq!(o.result_cache.ttl_secs(), 60);
    }
}
