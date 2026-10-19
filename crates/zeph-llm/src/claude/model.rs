// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Closed classification of Anthropic model identifiers and their wire capabilities.
//!
//! [`ClaudeModel::from_model_id`] parses a model id once; [`ClaudeModel::capabilities`] is a
//! single exhaustive match, so adding a variant forces every capability to be decided at
//! compile time instead of being inferred from substring checks scattered across the provider.

use std::num::NonZeroU32;

/// Build a non-zero window size in a const context.
const fn window(tokens: u32) -> NonZeroU32 {
    match NonZeroU32::new(tokens) {
        Some(w) => w,
        None => panic!("context window must be non-zero"),
    }
}

/// Context window of models without a beta-gated extension.
pub(super) const WINDOW_200K: NonZeroU32 = window(200_000);
/// Native 1M-token context window.
pub(super) const WINDOW_1M: NonZeroU32 = window(1_000_000);

/// Default ceiling applied to [`ModelCapabilities::native_window`] unless the provider opts
/// into `enable_extended_context`, so budgets stay at the long-standing 200k unless asked.
pub(super) const CLAUDE_DEFAULT_WINDOW_CEILING: NonZeroU32 = WINDOW_200K;

/// A recognised Anthropic model generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClaudeModel {
    /// Claude Haiku 4.5.
    Haiku45,
    /// Claude Haiku 5.5.
    Haiku55,
    /// Claude Sonnet 4.6.
    Sonnet46,
    /// Claude Sonnet 5.
    Sonnet5,
    /// Claude Sonnet 5.5.
    Sonnet55,
    /// Claude Opus 4.6.
    Opus46,
    /// Claude Opus 4.7.
    Opus47,
    /// Claude Opus 4.8.
    Opus48,
    /// Claude Opus 5.
    Opus5,
    /// Claude Opus 5.5.
    Opus55,
    /// Claude Fable 5.
    Fable5,
    /// Claude Fable 5.1.
    Fable51,
    /// Claude Mythos 5.
    Mythos5,
    /// Claude Mythos 5.1.
    Mythos51,
    /// Any Claude model older than 4.6 (Claude 3.x, Sonnet 4/4.5, Opus 4/4.1/4.5).
    Legacy,
    /// An identifier that does not match a known generation; treated conservatively.
    Unknown,
}

/// Wire-level behaviour of a [`ClaudeModel`].
#[allow(clippy::struct_excessive_bools)] // independent per-model facts; per-flag enums would add types, not safety
#[derive(Debug, Clone, Copy)]
pub(super) struct ModelCapabilities {
    /// `Extended` thinking is converted to `effort`; `budget_tokens` is rejected or deprecated.
    pub prefers_effort: bool,
    /// A trailing assistant message (prefill) is rejected regardless of thinking state.
    pub rejects_prefill: bool,
    /// `temperature` (and other sampling parameters) are rejected.
    pub rejects_sampling: bool,
    /// The model thinks even when `thinking` is not configured, so `max_tokens` needs headroom.
    pub thinking_always_on: bool,
    /// A forced `tool_choice` returns 400; only `auto` is accepted.
    pub forbids_forced_tool_choice: bool,
    /// The model validates thinking-block prefix binding and supports `drop_block`.
    pub preserved_thinking: bool,
    /// Supports `compact_20260112` server-side compaction.
    pub server_compaction: bool,
    /// Needs the `interleaved-thinking-2025-05-14` beta for `Extended` thinking with tools.
    pub needs_interleaved_beta: bool,
    /// Needs the `extended-cache-ttl-2025-04-25` beta to use the 1-hour cache TTL; newer models
    /// accept it natively and unknown models are not sent the header.
    pub extended_ttl_beta: bool,
    /// Supports `output_config.format` JSON-schema structured outputs (no forced tool choice).
    pub structured_outputs: bool,
    /// Accepts a `role: "system"` message between conversation turns (mid-conversation system
    /// directives). Other models receive the same text as user content.
    pub mid_conversation_system: bool,
    /// Needs the `context-1m-2025-08-07` beta to reach a 1M window.
    pub needs_1m_beta: bool,
    /// Context window without any beta, `None` when unknown.
    pub native_window: Option<NonZeroU32>,
}

impl ModelCapabilities {
    /// Window reachable when the caller opts into the extended context window.
    pub(super) fn extended_window(&self) -> Option<NonZeroU32> {
        if self.needs_1m_beta {
            Some(WINDOW_1M)
        } else {
            self.native_window
        }
    }

    /// Window used when the caller has not opted in: the native window, capped at the default
    /// ceiling.
    pub(super) fn default_window(&self) -> Option<NonZeroU32> {
        self.native_window
            .map(|w| w.min(CLAUDE_DEFAULT_WINDOW_CEILING))
    }

    const NONE: Self = Self {
        prefers_effort: false,
        rejects_prefill: false,
        rejects_sampling: false,
        thinking_always_on: false,
        forbids_forced_tool_choice: false,
        preserved_thinking: false,
        server_compaction: false,
        needs_interleaved_beta: false,
        extended_ttl_beta: false,
        needs_1m_beta: false,
        structured_outputs: false,
        mid_conversation_system: false,
        native_window: None,
    };

    /// Opus 4.7 and later share the strict request surface.
    const STRICT: Self = Self {
        prefers_effort: true,
        rejects_prefill: true,
        rejects_sampling: true,
        server_compaction: true,
        native_window: Some(WINDOW_1M),
        ..Self::NONE
    };

    /// The 5.x generation: strict surface and thinking that cannot be turned off.
    const GEN5: Self = Self {
        thinking_always_on: true,
        ..Self::STRICT
    };

    /// 5.x models that reject a forced `tool_choice` and run the prefix-binding check.
    const GEN5_BOUND: Self = Self {
        forbids_forced_tool_choice: true,
        preserved_thinking: true,
        structured_outputs: true,
        ..Self::GEN5
    };
}

impl ClaudeModel {
    /// Classify a model id.
    ///
    /// Provider prefixes (`anthropic.`, `us.anthropic.`), Vertex `@date` and Bedrock `-v1:0`
    /// suffixes and `-YYYYMMDD` stamps are ignored; the family and the longest numeric version
    /// are matched (`sonnet-5-5` is Sonnet 5.5, not Sonnet 5).
    pub(super) fn from_model_id(id: &str) -> Self {
        let lower = id.to_ascii_lowercase();
        let Some(start) = lower.find("claude-") else {
            return Self::Unknown;
        };
        let rest = &lower[start + "claude-".len()..];
        let rest = rest.split(['@', ':']).next().unwrap_or(rest);
        let mut tokens: Vec<&str> = rest.split('-').collect();
        if tokens.last().is_some_and(|t| is_date_stamp(t)) {
            tokens.pop();
        }
        let family = tokens.first().copied().unwrap_or_default();
        if family.parse::<u32>().is_ok() {
            return Self::Legacy;
        }
        let mut versions = tokens
            .iter()
            .skip(1)
            .map_while(|t| (t.len() <= 2).then(|| t.parse::<u32>().ok()).flatten());
        let version = (versions.next(), versions.next());
        match (family, version) {
            ("haiku", (Some(4), _)) => Self::Haiku45,
            ("haiku", (Some(5), Some(5))) => Self::Haiku55,
            ("haiku" | "sonnet" | "opus", (Some(3), _))
            | ("sonnet" | "opus", (Some(4), None | Some(0..=5))) => Self::Legacy,
            ("sonnet", (Some(4), Some(6))) => Self::Sonnet46,
            ("sonnet", (Some(5), None | Some(0))) => Self::Sonnet5,
            ("sonnet", (Some(5), Some(5))) => Self::Sonnet55,
            ("opus", (Some(4), Some(6))) => Self::Opus46,
            ("opus", (Some(4), Some(7))) => Self::Opus47,
            ("opus", (Some(4), Some(8))) => Self::Opus48,
            ("opus", (Some(5), None | Some(0))) => Self::Opus5,
            ("opus", (Some(5), Some(5))) => Self::Opus55,
            ("fable", (Some(5), None | Some(0))) => Self::Fable5,
            ("fable", (Some(5), Some(1))) => Self::Fable51,
            ("mythos", (Some(5), None | Some(0))) => Self::Mythos5,
            ("mythos", (Some(5), Some(1))) => Self::Mythos51,
            _ => Self::Unknown,
        }
    }

    /// Wire capabilities of this model. Exhaustive: a new variant must be decided here.
    pub(super) fn capabilities(self) -> ModelCapabilities {
        let none = ModelCapabilities::NONE;
        match self {
            Self::Legacy => ModelCapabilities {
                needs_1m_beta: true,
                extended_ttl_beta: true,
                native_window: Some(WINDOW_200K),
                ..none
            },
            Self::Haiku45 => ModelCapabilities {
                extended_ttl_beta: true,
                native_window: Some(WINDOW_200K),
                ..none
            },
            Self::Sonnet46 => ModelCapabilities {
                rejects_prefill: true,
                server_compaction: true,
                needs_interleaved_beta: true,
                native_window: Some(WINDOW_1M),
                ..none
            },
            Self::Opus46 => ModelCapabilities {
                prefers_effort: true,
                server_compaction: true,
                native_window: Some(WINDOW_1M),
                ..none
            },
            Self::Opus47 => ModelCapabilities::STRICT,
            Self::Opus48 => ModelCapabilities {
                mid_conversation_system: true,
                ..ModelCapabilities::STRICT
            },
            Self::Sonnet5 => ModelCapabilities::GEN5,
            Self::Opus5 | Self::Fable5 | Self::Mythos5 => ModelCapabilities {
                mid_conversation_system: true,
                ..ModelCapabilities::GEN5
            },
            Self::Opus55 | Self::Sonnet55 | Self::Fable51 => ModelCapabilities {
                mid_conversation_system: true,
                ..ModelCapabilities::GEN5_BOUND
            },
            Self::Haiku55 => ModelCapabilities {
                preserved_thinking: true,
                ..ModelCapabilities::GEN5
            },
            Self::Mythos51 => ModelCapabilities {
                forbids_forced_tool_choice: true,
                structured_outputs: true,
                mid_conversation_system: true,
                ..ModelCapabilities::GEN5
            },
            Self::Unknown => ModelCapabilities {
                prefers_effort: true,
                rejects_prefill: true,
                rejects_sampling: true,
                forbids_forced_tool_choice: true,
                ..none
            },
        }
    }
}

fn is_date_stamp(token: &str) -> bool {
    token.len() == 8 && token.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_model_id_table() {
        let cases = [
            ("claude-sonnet-5", ClaudeModel::Sonnet5),
            ("claude-sonnet-5-5", ClaudeModel::Sonnet55),
            ("claude-sonnet-5-5-20260801", ClaudeModel::Sonnet55),
            ("claude-sonnet-4-6-20250514", ClaudeModel::Sonnet46),
            ("claude-sonnet-4-5-20250929", ClaudeModel::Legacy),
            ("claude-sonnet-4-20250514", ClaudeModel::Legacy),
            ("claude-3-5-sonnet-20241022", ClaudeModel::Legacy),
            ("claude-opus-4-6", ClaudeModel::Opus46),
            ("claude-opus-4-7", ClaudeModel::Opus47),
            ("claude-opus-4-8", ClaudeModel::Opus48),
            ("claude-opus-4-1-20250805", ClaudeModel::Legacy),
            ("claude-opus-5", ClaudeModel::Opus5),
            ("claude-opus-5-5", ClaudeModel::Opus55),
            ("claude-haiku-4-5-20251001", ClaudeModel::Haiku45),
            ("claude-haiku-5-5", ClaudeModel::Haiku55),
            ("claude-fable-5", ClaudeModel::Fable5),
            ("claude-fable-5-1", ClaudeModel::Fable51),
            ("claude-mythos-5", ClaudeModel::Mythos5),
            ("claude-mythos-5-1", ClaudeModel::Mythos51),
            ("anthropic.claude-opus-5-5-v1:0", ClaudeModel::Opus55),
            ("us.anthropic.claude-sonnet-5-5", ClaudeModel::Sonnet55),
            ("claude-sonnet-4-5@20250929", ClaudeModel::Legacy),
            ("claude-opus-4-9", ClaudeModel::Unknown),
            ("claude-mythos-preview", ClaudeModel::Unknown),
            ("gpt-4o", ClaudeModel::Unknown),
        ];
        for (id, expected) in cases {
            assert_eq!(ClaudeModel::from_model_id(id), expected, "{id}");
        }
    }

    #[test]
    fn unknown_is_conservative() {
        let cap = ClaudeModel::Unknown.capabilities();
        assert!(cap.forbids_forced_tool_choice);
        assert!(cap.rejects_prefill && cap.rejects_sampling && cap.prefers_effort);
        assert!(!cap.server_compaction && !cap.needs_1m_beta && !cap.preserved_thinking);
        assert!(cap.native_window.is_none());
    }

    #[test]
    fn preserved_thinking_models() {
        let bound: Vec<_> = [
            ClaudeModel::Fable51,
            ClaudeModel::Opus55,
            ClaudeModel::Sonnet55,
            ClaudeModel::Haiku55,
        ]
        .into_iter()
        .filter(|m| m.capabilities().preserved_thinking)
        .collect();
        assert_eq!(bound.len(), 4);
        assert!(!ClaudeModel::Mythos51.capabilities().preserved_thinking);
        assert!(!ClaudeModel::Opus5.capabilities().preserved_thinking);
    }

    #[test]
    fn forced_tool_choice_matrix() {
        for m in [
            ClaudeModel::Opus55,
            ClaudeModel::Sonnet55,
            ClaudeModel::Fable51,
            ClaudeModel::Mythos51,
        ] {
            assert!(m.capabilities().forbids_forced_tool_choice, "{m:?}");
        }
        for m in [
            ClaudeModel::Opus5,
            ClaudeModel::Sonnet5,
            ClaudeModel::Haiku55,
            ClaudeModel::Opus48,
        ] {
            assert!(!m.capabilities().forbids_forced_tool_choice, "{m:?}");
        }
    }

    #[test]
    fn mid_conversation_system_matrix() {
        for m in [
            ClaudeModel::Opus5,
            ClaudeModel::Opus55,
            ClaudeModel::Opus48,
            ClaudeModel::Fable5,
            ClaudeModel::Fable51,
            ClaudeModel::Mythos5,
            ClaudeModel::Mythos51,
            ClaudeModel::Sonnet55,
        ] {
            assert!(m.capabilities().mid_conversation_system, "{m:?}");
        }
        for m in [
            ClaudeModel::Sonnet5,
            ClaudeModel::Haiku55,
            ClaudeModel::Haiku45,
            ClaudeModel::Opus47,
            ClaudeModel::Opus46,
            ClaudeModel::Legacy,
            ClaudeModel::Unknown,
        ] {
            assert!(!m.capabilities().mid_conversation_system, "{m:?}");
        }
    }

    #[test]
    fn server_compaction_support() {
        assert!(ClaudeModel::Opus46.capabilities().server_compaction);
        assert!(ClaudeModel::Haiku55.capabilities().server_compaction);
        assert!(!ClaudeModel::Haiku45.capabilities().server_compaction);
        assert!(!ClaudeModel::Legacy.capabilities().server_compaction);
    }

    #[test]
    fn window_ceiling_and_extension() {
        let opus = ClaudeModel::Opus55.capabilities();
        assert_eq!(opus.default_window(), Some(WINDOW_200K));
        assert_eq!(opus.extended_window(), Some(WINDOW_1M));

        let legacy = ClaudeModel::Legacy.capabilities();
        assert_eq!(legacy.default_window(), Some(WINDOW_200K));
        assert_eq!(legacy.extended_window(), Some(WINDOW_1M));

        let haiku = ClaudeModel::Haiku45.capabilities();
        assert_eq!(haiku.extended_window(), Some(WINDOW_200K));
    }
}
