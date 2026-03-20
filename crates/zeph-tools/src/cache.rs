// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use crate::executor::ToolOutput;

/// Tools that must never have their results cached due to side effects.
///
/// Any tool with side effects (writes, state mutations, external actions) MUST be listed here.
/// MCP tools (`mcp_` prefix) are non-cacheable by default — they are third-party and opaque.
/// `memory_search` is excluded to avoid stale results after `memory_save` calls.
static NON_CACHEABLE_TOOLS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "bash",          // shell commands have side effects and depend on mutable state
        "memory_save",   // writes to memory store
        "memory_search", // results may change after memory_save; consistency > performance
        "scheduler",     // creates/modifies scheduled tasks
        "write",         // writes files
    ])
});

/// Returns `true` if the tool's results can be safely cached.
///
/// MCP tools (identified by `mcp_` prefix) are always non-cacheable by default
/// since they are third-party and may have unknown side effects.
#[must_use]
pub fn is_cacheable(tool_name: &str) -> bool {
    if tool_name.starts_with("mcp_") {
        return false;
    }
    !NON_CACHEABLE_TOOLS.contains(tool_name)
}

/// Composite key identifying a unique tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub tool_name: String,
    pub args_hash: u64,
}

impl CacheKey {
    #[must_use]
    pub fn new(tool_name: impl Into<String>, args_hash: u64) -> Self {
        Self {
            tool_name: tool_name.into(),
            args_hash,
        }
    }
}

/// A single cached tool result with insertion timestamp.
#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub output: ToolOutput,
    pub inserted_at: Instant,
}

impl CacheEntry {
    fn is_expired(&self, ttl: Duration) -> bool {
        self.inserted_at.elapsed() > ttl
    }
}

/// In-memory, session-scoped cache for tool results.
///
/// # Design
/// - `ttl = None` means entries never expire (useful for batch/scripted sessions).
/// - `ttl = Some(d)` means entries expire after duration `d`.
/// - Lazy eviction: expired entries are removed on `get()`.
/// - No max-size cap: a session cache is bounded by session duration and interaction rate.
/// - Not `Send + Sync` by design — accessed only from the agent's single-threaded loop.
#[derive(Debug)]
pub struct ToolResultCache {
    entries: HashMap<CacheKey, CacheEntry>,
    /// `None` = never expire. `Some(d)` = expire after `d`.
    ttl: Option<Duration>,
    enabled: bool,
    hits: u64,
    misses: u64,
}

impl ToolResultCache {
    /// Create a new cache with the given TTL and enabled state.
    ///
    /// `ttl = None` means entries never expire.
    #[must_use]
    pub fn new(enabled: bool, ttl: Option<Duration>) -> Self {
        Self {
            entries: HashMap::new(),
            ttl,
            enabled,
            hits: 0,
            misses: 0,
        }
    }

    /// Look up a cached result. Returns `None` on miss or if expired.
    ///
    /// Expired entries are removed lazily on access.
    pub fn get(&mut self, key: &CacheKey) -> Option<ToolOutput> {
        if !self.enabled {
            return None;
        }
        if let Some(entry) = self.entries.get(key) {
            if self.ttl.is_some_and(|ttl| entry.is_expired(ttl)) {
                self.entries.remove(key);
                return None;
            }
            let output = entry.output.clone();
            self.hits += 1;
            return Some(output);
        }
        self.misses += 1;
        None
    }

    /// Store a tool result in the cache.
    pub fn put(&mut self, key: CacheKey, output: ToolOutput) {
        if !self.enabled {
            return;
        }
        self.entries.insert(
            key,
            CacheEntry {
                output,
                inserted_at: Instant::now(),
            },
        );
    }

    /// Remove all entries and reset hit/miss counters.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.hits = 0;
        self.misses = 0;
    }

    /// Number of entries currently in the cache (including potentially expired ones).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Total cache hits since last `clear()`.
    #[must_use]
    pub fn hits(&self) -> u64 {
        self.hits
    }

    /// Total cache misses since last `clear()`.
    #[must_use]
    pub fn misses(&self) -> u64 {
        self.misses
    }

    /// Whether the cache is enabled.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// TTL in seconds for display (0 = never expire).
    #[must_use]
    pub fn ttl_secs(&self) -> u64 {
        self.ttl.map_or(0, |d| d.as_secs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_output(summary: &str) -> ToolOutput {
        ToolOutput {
            tool_name: "test".to_owned(),
            summary: summary.to_owned(),
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
        }
    }

    fn key(name: &str, hash: u64) -> CacheKey {
        CacheKey::new(name, hash)
    }

    #[test]
    fn miss_on_empty_cache() {
        let mut cache = ToolResultCache::new(true, Some(Duration::from_secs(300)));
        assert!(cache.get(&key("read", 1)).is_none());
        assert_eq!(cache.misses(), 1);
        assert_eq!(cache.hits(), 0);
    }

    #[test]
    fn put_then_get_returns_cached() {
        let mut cache = ToolResultCache::new(true, Some(Duration::from_secs(300)));
        let out = make_output("file contents");
        cache.put(key("read", 42), out.clone());
        let result = cache.get(&key("read", 42));
        assert!(result.is_some());
        assert_eq!(result.unwrap().summary, "file contents");
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.misses(), 0);
    }

    #[test]
    fn different_hash_is_miss() {
        let mut cache = ToolResultCache::new(true, Some(Duration::from_secs(300)));
        cache.put(key("read", 1), make_output("a"));
        assert!(cache.get(&key("read", 2)).is_none());
    }

    #[test]
    fn different_tool_name_is_miss() {
        let mut cache = ToolResultCache::new(true, Some(Duration::from_secs(300)));
        cache.put(key("read", 1), make_output("a"));
        assert!(cache.get(&key("write", 1)).is_none());
    }

    #[test]
    fn ttl_none_never_expires() {
        let mut cache = ToolResultCache::new(true, None);
        cache.put(key("read", 1), make_output("content"));
        // Without TTL, entry should always be present
        assert!(cache.get(&key("read", 1)).is_some());
        assert_eq!(cache.hits(), 1);
    }

    #[test]
    fn ttl_zero_duration_expires_immediately() {
        // Duration::ZERO → elapsed() > Duration::ZERO is true immediately (any nanosecond suffices).
        // This verifies the behaviour: does not panic, and the entry is gone after get().
        let mut cache = ToolResultCache::new(true, Some(Duration::ZERO));
        cache.put(key("read", 1), make_output("content"));
        let result = cache.get(&key("read", 1));
        // Entry expired on access — None and evicted from map.
        assert!(
            result.is_none(),
            "Duration::ZERO entry must expire on first get()"
        );
        assert_eq!(cache.len(), 0, "expired entry must be removed from map");
    }

    #[test]
    fn ttl_expired_returns_none() {
        let mut cache = ToolResultCache::new(true, Some(Duration::from_millis(1)));
        cache.put(key("read", 1), make_output("content"));
        std::thread::sleep(Duration::from_millis(10));
        assert!(cache.get(&key("read", 1)).is_none());
        // expired entry is evicted, re-query also miss
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn clear_removes_all_and_resets_counters() {
        let mut cache = ToolResultCache::new(true, Some(Duration::from_secs(300)));
        cache.put(key("read", 1), make_output("a"));
        cache.put(key("web_scrape", 2), make_output("b"));
        // generate some hits/misses
        cache.get(&key("read", 1));
        cache.get(&key("missing", 99));
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.misses(), 1);

        cache.clear();
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.hits(), 0);
        assert_eq!(cache.misses(), 0);
        assert!(cache.get(&key("read", 1)).is_none());
    }

    #[test]
    fn disabled_cache_always_misses() {
        let mut cache = ToolResultCache::new(false, Some(Duration::from_secs(300)));
        cache.put(key("read", 1), make_output("content"));
        // put is a no-op when disabled
        assert!(cache.get(&key("read", 1)).is_none());
        assert_eq!(cache.len(), 0);
        // misses counter also stays 0 when disabled
        assert_eq!(cache.misses(), 0);
    }

    #[test]
    fn is_cacheable_returns_false_for_deny_list() {
        assert!(!is_cacheable("bash"));
        assert!(!is_cacheable("memory_save"));
        assert!(!is_cacheable("memory_search"));
        assert!(!is_cacheable("scheduler"));
        assert!(!is_cacheable("write"));
    }

    #[test]
    fn is_cacheable_returns_false_for_mcp_prefix() {
        assert!(!is_cacheable("mcp_github_list_issues"));
        assert!(!is_cacheable("mcp_send_email"));
        assert!(!is_cacheable("mcp_"));
    }

    #[test]
    fn is_cacheable_returns_true_for_read_only_tools() {
        assert!(is_cacheable("read"));
        assert!(is_cacheable("web_scrape"));
        assert!(is_cacheable("search_code"));
        assert!(is_cacheable("load_skill"));
        assert!(is_cacheable("diagnostics"));
    }

    #[test]
    fn counter_increments_correctly() {
        let mut cache = ToolResultCache::new(true, Some(Duration::from_secs(300)));
        cache.put(key("read", 1), make_output("a"));
        cache.put(key("read", 2), make_output("b"));

        cache.get(&key("read", 1)); // hit
        cache.get(&key("read", 1)); // hit
        cache.get(&key("read", 99)); // miss

        assert_eq!(cache.hits(), 2);
        assert_eq!(cache.misses(), 1);
    }

    #[test]
    fn ttl_secs_returns_zero_for_none() {
        let cache = ToolResultCache::new(true, None);
        assert_eq!(cache.ttl_secs(), 0);
    }

    #[test]
    fn ttl_secs_returns_seconds_for_some() {
        let cache = ToolResultCache::new(true, Some(Duration::from_secs(300)));
        assert_eq!(cache.ttl_secs(), 300);
    }
}
