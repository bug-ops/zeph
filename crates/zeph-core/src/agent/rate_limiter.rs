// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-category sliding-window tool rate limiter with circuit-breaker.
//!
//! Tracks calls per minute across five tool categories (Shell, Web, Memory, Mcp, Other).
//! When a category exceeds its configured limit, the circuit breaker trips and further
//! calls in that category are rejected until the cooldown expires.
//!
//! Configured under `[security.rate_limit]` in the agent config file.
//! Disabled by default — opt-in.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// ToolCategory
// ---------------------------------------------------------------------------

/// Logical category used for per-category rate limiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolCategory {
    Shell,
    Web,
    Memory,
    Mcp,
    Other,
}

impl ToolCategory {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Web => "web",
            Self::Memory => "memory",
            Self::Mcp => "mcp",
            Self::Other => "other",
        }
    }
}

/// Classify a tool name into a [`ToolCategory`].
///
/// Matches the same logic used in `sanitize_tool_output` for source kind differentiation.
#[must_use]
pub fn tool_category(name: &str) -> ToolCategory {
    if name.contains(':') || name == "mcp" || name == "search_code" {
        ToolCategory::Mcp
    } else if name == "web-scrape" || name == "web_scrape" || name == "fetch" {
        ToolCategory::Web
    } else if name == "shell" || name == "terminal" || name == "bash" {
        ToolCategory::Shell
    } else if name == "memory_save" || name == "memory_search" {
        ToolCategory::Memory
    } else {
        ToolCategory::Other
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

fn default_shell_limit() -> usize {
    30
}

fn default_web_limit() -> usize {
    20
}

fn default_memory_limit() -> usize {
    60
}

fn default_mcp_limit() -> usize {
    40
}

fn default_other_limit() -> usize {
    60
}

fn default_cooldown_secs() -> u64 {
    30
}

/// Configuration for the tool rate limiter, nested under `[security.rate_limit]`.
///
/// Disabled by default. All limits are calls per 60-second window.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct RateLimitConfig {
    /// Master switch. When `false`, all checks are no-ops.
    #[serde(default)]
    pub enabled: bool,
    /// Maximum shell tool calls per minute.
    #[serde(default = "default_shell_limit")]
    pub shell_calls_per_minute: usize,
    /// Maximum web tool calls per minute.
    #[serde(default = "default_web_limit")]
    pub web_calls_per_minute: usize,
    /// Maximum memory tool calls per minute.
    #[serde(default = "default_memory_limit")]
    pub memory_calls_per_minute: usize,
    /// Maximum MCP tool calls per minute.
    #[serde(default = "default_mcp_limit")]
    pub mcp_calls_per_minute: usize,
    /// Maximum other tool calls per minute.
    #[serde(default = "default_other_limit")]
    pub other_calls_per_minute: usize,
    /// Seconds the circuit breaker stays tripped after a limit is exceeded.
    #[serde(default = "default_cooldown_secs")]
    pub circuit_breaker_cooldown_secs: u64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            shell_calls_per_minute: default_shell_limit(),
            web_calls_per_minute: default_web_limit(),
            memory_calls_per_minute: default_memory_limit(),
            mcp_calls_per_minute: default_mcp_limit(),
            other_calls_per_minute: default_other_limit(),
            circuit_breaker_cooldown_secs: default_cooldown_secs(),
        }
    }
}

// ---------------------------------------------------------------------------
// RateLimitExceeded
// ---------------------------------------------------------------------------

/// Returned by [`ToolRateLimiter::check_batch`] when a tool call is blocked.
#[derive(Debug, Clone)]
pub struct RateLimitExceeded {
    pub category: ToolCategory,
    /// Current window count at the time the limit was exceeded.
    pub count: usize,
    pub limit: usize,
    pub cooldown_remaining_secs: u64,
}

impl RateLimitExceeded {
    /// Format as a user-visible error message to inject as a synthetic tool output.
    #[must_use]
    pub fn to_error_message(&self) -> String {
        format!(
            "[rate-limited] {} calls exceeded {}/min (current: {}). \
             Circuit breaker active, cooldown: {}s. \
             Try again later or use a different approach.",
            self.category.as_str(),
            self.limit,
            self.count,
            self.cooldown_remaining_secs
        )
    }
}

// ---------------------------------------------------------------------------
// SlidingWindow
// ---------------------------------------------------------------------------

struct SlidingWindow {
    timestamps: VecDeque<Instant>,
}

impl SlidingWindow {
    fn new() -> Self {
        Self {
            timestamps: VecDeque::new(),
        }
    }

    /// Prune entries older than 60 seconds and return the current count.
    fn count(&mut self) -> usize {
        let cutoff = Instant::now()
            .checked_sub(std::time::Duration::from_secs(60))
            .unwrap_or(Instant::now());
        while self.timestamps.front().is_some_and(|&t| t <= cutoff) {
            self.timestamps.pop_front();
        }
        self.timestamps.len()
    }

    /// Record a new call timestamp (reserves the slot).
    fn push(&mut self) {
        self.timestamps.push_back(Instant::now());
    }
}

// ---------------------------------------------------------------------------
// ToolRateLimiter
// ---------------------------------------------------------------------------

/// Per-category sliding-window rate limiter with circuit-breaker.
///
/// Store directly on the agent (not `SecurityState`) because sliding-window state
/// mutates on every tool call.
pub struct ToolRateLimiter {
    config: RateLimitConfig,
    windows: HashMap<ToolCategory, SlidingWindow>,
    /// Maps category → the instant the circuit breaker tripped.
    tripped: HashMap<ToolCategory, Instant>,
}

impl ToolRateLimiter {
    /// Create a new limiter from the given configuration.
    #[must_use]
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            config,
            windows: HashMap::new(),
            tripped: HashMap::new(),
        }
    }

    /// Atomically check and reserve slots for a batch of tool names.
    ///
    /// Each item in the returned `Vec` is:
    /// - `None` — the call is allowed and the slot has been reserved.
    /// - `Some(exceeded)` — the call is blocked; inject a synthetic error `ToolOutput`.
    ///
    /// Slots are reserved **before** dispatch so that parallel calls in the same tier
    /// cannot bypass the limit by all passing the check before any records the use.
    ///
    /// When the limiter is disabled, all entries are `None`.
    #[must_use]
    pub fn check_batch(&mut self, tool_names: &[&str]) -> Vec<Option<RateLimitExceeded>> {
        if !self.config.enabled {
            return vec![None; tool_names.len()];
        }

        let mut results = Vec::with_capacity(tool_names.len());
        let now = Instant::now();

        for &name in tool_names {
            let category = tool_category(name);
            let limit = self.limit_for(category);
            let cooldown =
                std::time::Duration::from_secs(self.config.circuit_breaker_cooldown_secs);

            // Check circuit breaker first.
            if let Some(&trip_time) = self.tripped.get(&category) {
                let elapsed = now.duration_since(trip_time);
                if elapsed < cooldown {
                    let remaining = cooldown.checked_sub(elapsed).unwrap_or_default().as_secs();
                    results.push(Some(RateLimitExceeded {
                        category,
                        count: 0,
                        limit,
                        cooldown_remaining_secs: remaining,
                    }));
                    continue;
                }
                // Cooldown expired — reset circuit breaker.
                self.tripped.remove(&category);
                if let Some(w) = self.windows.get_mut(&category) {
                    // Prune stale entries now that we are resetting.
                    w.count();
                }
            }

            let window = self
                .windows
                .entry(category)
                .or_insert_with(SlidingWindow::new);
            let current_count = window.count();

            if current_count >= limit {
                // Trip circuit breaker.
                self.tripped.insert(category, now);
                tracing::warn!(
                    category = category.as_str(),
                    count = current_count,
                    limit,
                    "tool rate limiter: circuit breaker tripped"
                );
                results.push(Some(RateLimitExceeded {
                    category,
                    count: current_count,
                    limit,
                    cooldown_remaining_secs: self.config.circuit_breaker_cooldown_secs,
                }));
            } else {
                // Reserve slot atomically before dispatch.
                window.push();
                results.push(None);
            }
        }

        results
    }

    /// Returns `true` if the circuit breaker is currently tripped for `category`.
    ///
    /// Available for testing and future TUI diagnostics.
    #[must_use]
    #[allow(dead_code)]
    pub fn is_tripped(&self, category: ToolCategory) -> bool {
        if let Some(&trip_time) = self.tripped.get(&category) {
            let cooldown =
                std::time::Duration::from_secs(self.config.circuit_breaker_cooldown_secs);
            return Instant::now().duration_since(trip_time) < cooldown;
        }
        false
    }

    /// Returns the calls-per-minute limit for `category`.
    #[must_use]
    pub fn limit_for(&self, category: ToolCategory) -> usize {
        match category {
            ToolCategory::Shell => self.config.shell_calls_per_minute,
            ToolCategory::Web => self.config.web_calls_per_minute,
            ToolCategory::Memory => self.config.memory_calls_per_minute,
            ToolCategory::Mcp => self.config.mcp_calls_per_minute,
            ToolCategory::Other => self.config.other_calls_per_minute,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter_with(shell: usize, cooldown: u64) -> ToolRateLimiter {
        ToolRateLimiter::new(RateLimitConfig {
            enabled: true,
            shell_calls_per_minute: shell,
            web_calls_per_minute: shell,
            memory_calls_per_minute: shell,
            mcp_calls_per_minute: shell,
            other_calls_per_minute: shell,
            circuit_breaker_cooldown_secs: cooldown,
        })
    }

    // --- tool_category ---

    #[test]
    fn classifies_shell() {
        assert_eq!(tool_category("shell"), ToolCategory::Shell);
        assert_eq!(tool_category("bash"), ToolCategory::Shell);
        assert_eq!(tool_category("terminal"), ToolCategory::Shell);
    }

    #[test]
    fn classifies_web() {
        assert_eq!(tool_category("web-scrape"), ToolCategory::Web);
        assert_eq!(tool_category("web_scrape"), ToolCategory::Web);
        assert_eq!(tool_category("fetch"), ToolCategory::Web);
    }

    #[test]
    fn classifies_memory() {
        assert_eq!(tool_category("memory_save"), ToolCategory::Memory);
        assert_eq!(tool_category("memory_search"), ToolCategory::Memory);
    }

    #[test]
    fn classifies_mcp_by_colon() {
        assert_eq!(tool_category("server:tool"), ToolCategory::Mcp);
        assert_eq!(tool_category("mcp"), ToolCategory::Mcp);
    }

    #[test]
    fn classifies_other() {
        assert_eq!(tool_category("unknown_tool"), ToolCategory::Other);
    }

    // --- disabled limiter ---

    #[test]
    fn disabled_always_allows() {
        let mut limiter = ToolRateLimiter::new(RateLimitConfig::default());
        let results = limiter.check_batch(&["shell", "shell", "shell"]);
        assert!(results.iter().all(Option::is_none));
    }

    // --- check_batch ---

    #[test]
    fn allows_within_limit() {
        let mut limiter = limiter_with(5, 30);
        let results = limiter.check_batch(&["shell", "shell", "shell"]);
        assert!(results.iter().all(Option::is_none));
    }

    #[test]
    fn blocks_at_limit() {
        let mut limiter = limiter_with(2, 30);
        // Fill to limit.
        let r1 = limiter.check_batch(&["shell", "shell"]);
        assert!(r1.iter().all(Option::is_none), "first batch within limit");
        // Next one exceeds limit.
        let r2 = limiter.check_batch(&["shell"]);
        assert!(r2[0].is_some(), "call at limit+1 must be blocked");
    }

    #[test]
    fn batch_reserves_atomically() {
        // Limit = 3. Batch of 4 shell calls: first 3 should pass, 4th blocked.
        let mut limiter = limiter_with(3, 30);
        let results = limiter.check_batch(&["shell", "shell", "shell", "shell"]);
        let allowed: usize = results.iter().filter(|r| r.is_none()).count();
        let blocked: usize = results.iter().filter(|r| r.is_some()).count();
        assert_eq!(allowed, 3, "first 3 must be allowed");
        assert_eq!(blocked, 1, "4th must be blocked");
    }

    #[test]
    fn circuit_breaker_trips_on_overflow() {
        let mut limiter = limiter_with(1, 30);
        // First call — allowed.
        let r1 = limiter.check_batch(&["shell"]);
        assert!(r1[0].is_none());
        // Second call — trips breaker.
        let r2 = limiter.check_batch(&["shell"]);
        assert!(r2[0].is_some());
        // Third call — breaker still tripped, returns Some immediately.
        let r3 = limiter.check_batch(&["shell"]);
        assert!(r3[0].is_some());
        assert!(limiter.is_tripped(ToolCategory::Shell));
    }

    #[test]
    fn categories_are_independent() {
        let mut limiter = limiter_with(1, 30);
        // Fill shell limit.
        let _ = limiter.check_batch(&["shell"]);
        // shell is tripped, but web is independent.
        let r = limiter.check_batch(&["shell", "web_scrape"]);
        assert!(r[0].is_some(), "shell must be blocked");
        assert!(r[1].is_none(), "web must still be allowed");
    }

    #[test]
    fn error_message_format() {
        let exceeded = RateLimitExceeded {
            category: ToolCategory::Shell,
            count: 30,
            limit: 30,
            cooldown_remaining_secs: 25,
        };
        let msg = exceeded.to_error_message();
        assert!(msg.contains("[rate-limited]"));
        assert!(msg.contains("shell"));
        assert!(msg.contains("30/min"));
        assert!(msg.contains("25s"));
    }

    // --- tool_category edge cases ---

    #[test]
    fn classifies_search_code_as_mcp() {
        assert_eq!(tool_category("search_code"), ToolCategory::Mcp);
    }

    #[test]
    fn classifies_empty_string_as_other() {
        assert_eq!(tool_category(""), ToolCategory::Other);
    }

    // --- limit_for ---

    #[test]
    fn limit_for_returns_correct_per_category() {
        let limiter = ToolRateLimiter::new(RateLimitConfig {
            enabled: true,
            shell_calls_per_minute: 10,
            web_calls_per_minute: 20,
            memory_calls_per_minute: 30,
            mcp_calls_per_minute: 40,
            other_calls_per_minute: 50,
            circuit_breaker_cooldown_secs: 30,
        });
        assert_eq!(limiter.limit_for(ToolCategory::Shell), 10);
        assert_eq!(limiter.limit_for(ToolCategory::Web), 20);
        assert_eq!(limiter.limit_for(ToolCategory::Memory), 30);
        assert_eq!(limiter.limit_for(ToolCategory::Mcp), 40);
        assert_eq!(limiter.limit_for(ToolCategory::Other), 50);
    }

    // --- is_tripped for fresh limiter ---

    #[test]
    fn is_tripped_false_for_fresh_limiter() {
        let limiter = limiter_with(5, 30);
        assert!(!limiter.is_tripped(ToolCategory::Shell));
        assert!(!limiter.is_tripped(ToolCategory::Web));
    }

    // --- empty batch ---

    #[test]
    fn empty_batch_returns_empty() {
        let mut limiter = limiter_with(5, 30);
        let results = limiter.check_batch(&[]);
        assert!(results.is_empty());
    }

    #[test]
    fn disabled_empty_batch_returns_empty() {
        let mut limiter = ToolRateLimiter::new(RateLimitConfig::default());
        let results = limiter.check_batch(&[]);
        assert!(results.is_empty());
    }

    // --- error message zero cooldown ---

    #[test]
    fn error_message_zero_cooldown() {
        let exceeded = RateLimitExceeded {
            category: ToolCategory::Web,
            count: 5,
            limit: 5,
            cooldown_remaining_secs: 0,
        };
        let msg = exceeded.to_error_message();
        assert!(msg.contains("web"));
        assert!(msg.contains("0s"));
    }

    // --- circuit breaker tracks count correctly ---

    #[test]
    fn blocked_call_reports_count_and_limit() {
        let mut limiter = limiter_with(1, 60);
        // First call fills the limit.
        let _ = limiter.check_batch(&["shell"]);
        // Second call trips the breaker and returns Some with correct values.
        let r = limiter.check_batch(&["shell"]);
        let exceeded = r[0].as_ref().expect("must be blocked");
        assert_eq!(exceeded.limit, 1);
        assert_eq!(exceeded.category, ToolCategory::Shell);
    }

    // --- multiple categories in one batch ---

    #[test]
    fn mixed_batch_respects_per_category_limits() {
        let mut limiter = limiter_with(1, 30);
        // One call each to shell and web — should both pass (limit = 1 each).
        let results = limiter.check_batch(&["shell", "web_scrape"]);
        assert!(results[0].is_none(), "first shell call allowed");
        assert!(results[1].is_none(), "first web call allowed");
        // Next shell call should be blocked; web should also be blocked.
        let results2 = limiter.check_batch(&["shell", "web_scrape"]);
        assert!(results2[0].is_some(), "second shell call blocked");
        assert!(results2[1].is_some(), "second web call blocked");
    }
}
