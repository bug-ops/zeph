// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};

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

fn default_rate_limit_enabled() -> bool {
    true
}

/// Configuration for the tool rate limiter, nested under `[security.rate_limit]`.
///
/// Enabled by default. All limits are calls per 60-second window.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct RateLimitConfig {
    /// Master switch. When `false`, all checks are no-ops.
    #[serde(default = "default_rate_limit_enabled")]
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
            enabled: true,
            shell_calls_per_minute: default_shell_limit(),
            web_calls_per_minute: default_web_limit(),
            memory_calls_per_minute: default_memory_limit(),
            mcp_calls_per_minute: default_mcp_limit(),
            other_calls_per_minute: default_other_limit(),
            circuit_breaker_cooldown_secs: default_cooldown_secs(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_enabled() {
        assert!(RateLimitConfig::default().enabled);
    }

    #[test]
    fn absent_section_defaults_to_enabled() {
        let cfg: RateLimitConfig = toml::from_str("").unwrap();
        assert!(
            cfg.enabled,
            "absent [security.rate_limit] must default to enabled = true"
        );
    }

    /// Regression test for the serde field-default split-brain (issue #6469): a
    /// present-but-partial section must resolve `enabled` via the named default
    /// function, not `bool::default()` (`false`), so it agrees with an absent section.
    #[test]
    fn partial_section_still_defaults_to_enabled() {
        let cfg: RateLimitConfig = toml::from_str("shell_calls_per_minute = 50").unwrap();
        assert!(
            cfg.enabled,
            "partial [security.rate_limit] section must still default enabled = true"
        );
        assert_eq!(cfg.shell_calls_per_minute, 50);
    }

    #[test]
    fn explicit_disabled_is_preserved() {
        let cfg: RateLimitConfig = toml::from_str("enabled = false").unwrap();
        assert!(!cfg.enabled, "explicit enabled = false must be preserved");
    }

    #[test]
    fn explicit_disabled_with_partial_section_is_preserved() {
        let cfg: RateLimitConfig =
            toml::from_str("enabled = false\nshell_calls_per_minute = 50").unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.shell_calls_per_minute, 50);
    }
}
