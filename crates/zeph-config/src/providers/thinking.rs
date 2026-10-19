// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Thinking, reasoning-effort, and prompt-cache config enums shared across providers.
//!
//! These types model provider-specific reasoning controls (Claude extended/adaptive
//! thinking, Gemini thinking levels) and the Anthropic prompt-cache TTL. They are
//! embedded in [`super::ProviderEntry`] and serialized into `[[llm.providers]]` entries.

use serde::{Deserialize, Serialize};

#[non_exhaustive]
/// Extended or adaptive thinking mode for Claude.
///
/// Serializes with `mode` as tag:
/// `{ "mode": "extended", "budget_tokens": 10000 }` or `{ "mode": "adaptive" }`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ThinkingConfig {
    /// Extended thinking with an explicit token budget.
    Extended {
        /// Maximum thinking tokens to allocate.
        budget_tokens: u32,
    },
    /// Adaptive thinking that selects effort automatically.
    Adaptive {
        /// Explicit effort hint when provided; model-chosen when `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<ThinkingEffort>,
    },
}

/// Effort level for adaptive thinking.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ThinkingEffort {
    /// Minimal thinking; fastest responses.
    Low,
    /// Balanced thinking depth. This is the default.
    #[default]
    Medium,
    /// Maximum thinking depth; slowest responses.
    High,
}

#[non_exhaustive]
/// Prompt-cache TTL variant for the Anthropic API.
///
/// When used as a TOML config value the accepted strings are `"ephemeral"` and `"1h"`.
/// On the wire (Anthropic API), `OneHour` serializes as `"1h"` inside the `cache_control.ttl`
/// field.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum CacheTtl {
    /// Default ephemeral TTL (~5 minutes). No beta header required.
    #[default]
    Ephemeral,
    /// Extended 1-hour TTL. The `extended-cache-ttl-2025-04-25` beta header is sent only to
    /// models that still require it (legacy models and Haiku 4.5).
    /// Cache writes cost approximately 2× more than `Ephemeral`.
    #[serde(rename = "1h")]
    OneHour,
}

/// Thinking level for Gemini models that support extended reasoning.
///
/// Maps to `generationConfig.thinkingConfig.thinkingLevel` in the Gemini API.
/// Valid for Gemini 3+ models. For Gemini 2.5, use `thinking_budget` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum GeminiThinkingLevel {
    /// Minimal reasoning pass.
    Minimal,
    /// Low reasoning depth.
    Low,
    /// Medium reasoning depth.
    Medium,
    /// Full reasoning depth.
    High,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct ThinkingExample {
        thinking: ThinkingConfig,
    }

    #[test]
    fn default_toml_thinking_example_deserializes() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/default.toml");
        let text = std::fs::read_to_string(path).expect("config/default.toml readable");
        let line = text
            .lines()
            .find_map(|l| l.strip_prefix("# thinking = "))
            .expect("default.toml carries a commented `thinking` example");
        let example: ThinkingExample = toml::from_str(&format!(
            "thinking = {}",
            line.split('#').next().unwrap_or(line)
        ))
        .expect("example parses");
        assert_eq!(
            example.thinking,
            ThinkingConfig::Adaptive {
                effort: Some(ThinkingEffort::Medium)
            }
        );
    }
}
