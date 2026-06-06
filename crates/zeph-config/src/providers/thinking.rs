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
    /// Extended 1-hour TTL. Requires the `extended-cache-ttl-2025-04-25` beta header.
    /// Cache writes cost approximately 2× more than `Ephemeral`.
    #[serde(rename = "1h")]
    OneHour,
}

impl CacheTtl {
    /// Returns `true` when this TTL variant requires the `extended-cache-ttl-2025-04-25` beta
    /// header to be sent with each request.
    #[must_use]
    pub fn requires_beta(self) -> bool {
        match self {
            Self::OneHour => true,
            Self::Ephemeral => false,
        }
    }
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
