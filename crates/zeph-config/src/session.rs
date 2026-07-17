// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Session-scoped user experience settings (#3064).
//!
//! Configures behaviours that shape the user's experience per session, such as
//! showing a recap of the previous conversation on resume.

use serde::{Deserialize, Serialize};

use crate::providers::ProviderName;

/// Top-level `[session]` config block.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)] // config struct — boolean flags are idiomatic for TOML-deserialized configuration
pub struct SessionConfig {
    /// Recap-on-resume settings.
    pub recap: RecapConfig,
    /// Whether to persist the last-used provider per channel across restarts.
    ///
    /// When `true` (the default), the agent stores the active provider name in `SQLite`
    /// after each `/provider` switch and restores it on the next startup for the same
    /// `(channel_type, channel_id)` pair.
    ///
    /// Set to `false` to always start with the configured primary provider.
    pub provider_persistence: bool,
    /// Whether to persist per-session provider override parameters across restarts (#4654).
    ///
    /// Currently persists `reasoning_effort` only (Phase 1). Only takes effect when
    /// `provider_persistence` is also `true` — overrides are meaningless without a persisted
    /// provider to apply them to. Default: `true`.
    pub persist_provider_overrides: bool,
    /// Whether to maintain a durable, replayable JSONL event log per conversation-session
    /// (spec-068, #5343). Default: `true`.
    ///
    /// When `true`, every channel (CLI, TUI, Telegram, ACP) mints a
    /// [`zeph_common::SessionId`] on first turn and appends
    /// `SessionEvent`s to `<data_dir>/<session_id>/events.jsonl`. When `false`, only the
    /// existing `messages` `SQLite` projection is written (pre-#5343 behavior).
    pub enabled: bool,
    /// Directory under which per-session event logs are stored (spec-068 §4.1).
    ///
    /// Default: `.zeph/sessions` (sibling of `memory.sqlite_path`'s parent directory).
    pub data_dir: String,
    /// Opt-in AEAD encryption of session event logs. Deferred to a post-MVP implementation
    /// (spec-068 §4.3) — currently has no effect. Default: `false`.
    pub encrypt: bool,
    /// Event-log size (in MB) that acts as a rotate/condense trigger guard (spec-068 §17).
    /// Default: `256`.
    pub max_event_log_mb: u64,
    /// Durable context condensation settings (spec-068 §8).
    pub condense: CondenseConfig,
    /// Resume-visibility settings: banner and `/history` bounds (spec-068 §13, §18).
    pub resume: ResumeConfig,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            recap: RecapConfig::default(),
            provider_persistence: true,
            persist_provider_overrides: true,
            enabled: true,
            data_dir: ".zeph/sessions".to_owned(),
            encrypt: false,
            max_event_log_mb: 256,
            condense: CondenseConfig::default(),
            resume: ResumeConfig::default(),
        }
    }
}

/// `[session.resume]` — resume-visibility settings (spec-068 §13, §18).
///
/// Controls the neutral "Resuming session" banner shown by display-owning channels
/// (CLI, TUI) on startup and the bound applied to the `/history` command with no
/// argument. Has no effect on chat channels (Telegram/Discord/Slack) or ACP/IDE
/// sessions, which are exempt from the automatic banner in v1 (spec-068 §13.2, §13.8).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)] // config struct — boolean flags are idiomatic for TOML-deserialized configuration
pub struct ResumeConfig {
    /// Show the neutral resume banner on display-owning channels (CLI/TUI) when
    /// resuming a non-empty prior conversation. Default: `true`.
    pub show_banner: bool,
    /// Intended to always render full history on startup instead of the collapsed banner.
    ///
    /// Not consumed in v1 — no code path currently dispatches an `/history`-equivalent
    /// expansion when this is set; enabling it changes nothing observable. Reserved for a
    /// future PR (would need the same `/history all` pagination path this PR already built,
    /// triggered automatically at startup instead of on user request). Default: `false`.
    pub auto_expand: bool,
    /// Bound applied to `/history` with no argument — the last N messages, sliced
    /// before formatting (INV-SP-6). Default: `20`.
    pub expand_default_lines: usize,
    /// Opt-in: fold the cached `session.recap` summary into the resume banner.
    ///
    /// Not consumed in v1 — wiring this would require invoking the `/recap` LLM
    /// path at startup, which is explicitly out of scope for spec-068 §13 (resume
    /// visibility is a presentation-only feature over already-persisted data).
    /// Reserved for a future PR. Default: `false`.
    pub show_recap: bool,
}

impl Default for ResumeConfig {
    fn default() -> Self {
        Self {
            show_banner: true,
            auto_expand: false,
            expand_default_lines: 20,
            show_recap: false,
        }
    }
}

/// `[session.condense]` — durable context condensation policy (spec-068 §8).
///
/// Distinct from live in-memory compaction (`zeph-context`): condensation operates at the
/// event-log level and is recorded as a replayable `Condensation` event.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct CondenseConfig {
    /// Provider name from `[[llm.providers]]` for condensation LLM calls.
    ///
    /// An empty [`ProviderName`] falls back to the primary provider. Default: `""`.
    pub condense_provider: ProviderName,
    /// Fraction of the context budget that triggers condensation on resume/mid-session.
    /// Default: `0.85`.
    pub threshold: f64,
    /// Minimum number of recent events to preserve after condensation. Default: `20`.
    pub keep_recent: usize,
}

impl Default for CondenseConfig {
    fn default() -> Self {
        Self {
            condense_provider: ProviderName::default(),
            threshold: 0.85,
            keep_recent: 20,
        }
    }
}

/// `[session.recap]` — controls the session recap feature (#3064).
///
/// A recap summarises the previous conversation in a few sentences and is
/// shown to the user when they resume a session that has a persisted digest.
///
/// # Example
///
/// ```toml
/// [session.recap]
/// on_resume = true
/// max_tokens = 200
/// provider = ""
/// max_input_messages = 20
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct RecapConfig {
    /// Show a recap of the previous session when resuming a conversation.
    ///
    /// When `true` and a persisted digest exists for the conversation, the
    /// agent emits a brief recap before accepting the first user message.
    /// Default: `true`.
    pub on_resume: bool,

    /// Maximum tokens for the recap text.
    ///
    /// Limits the length of the generated or cached recap. Default: `200`.
    pub max_tokens: usize,

    /// Provider name from `[[llm.providers]]` for recap LLM calls.
    ///
    /// An empty [`ProviderName`] falls back to the primary provider. Default: `""`.
    pub provider: ProviderName,

    /// Maximum recent messages included when generating a fresh recap.
    ///
    /// Used only when no cached digest is available (fresh-generation path).
    /// Default: `20`.
    pub max_input_messages: usize,
}

impl Default for RecapConfig {
    fn default() -> Self {
        Self {
            on_resume: true,
            max_tokens: 200,
            provider: ProviderName::default(),
            max_input_messages: 20,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn condense_config_default_provider_is_empty() {
        let cfg = CondenseConfig::default();
        assert!(
            cfg.condense_provider.is_empty(),
            "condense_provider must default to empty (fallback to primary provider), matching \
             the sibling recap/feedback/arise_trace provider fields — a non-empty default like \
             \"fast\" spams a fallback WARN when no provider named \"fast\" is configured (#5665)"
        );
    }

    #[test]
    fn condense_config_empty_section_uses_defaults() {
        let cfg: CondenseConfig = toml::from_str("").unwrap();
        assert!(cfg.condense_provider.is_empty());
        assert!((cfg.threshold - 0.85).abs() < f64::EPSILON);
        assert_eq!(cfg.keep_recent, 20);
    }

    #[test]
    fn condense_config_explicit_provider_roundtrip() {
        let cfg: CondenseConfig = toml::from_str(r#"condense_provider = "fast""#).unwrap();
        assert_eq!(cfg.condense_provider, "fast");
    }

    #[test]
    fn resume_config_defaults() {
        let cfg = ResumeConfig::default();
        assert!(cfg.show_banner);
        assert!(!cfg.auto_expand);
        assert_eq!(cfg.expand_default_lines, 20);
        assert!(!cfg.show_recap);
    }

    #[test]
    fn resume_config_empty_section_uses_defaults() {
        let cfg: ResumeConfig = toml::from_str("").unwrap();
        assert!(cfg.show_banner);
        assert_eq!(cfg.expand_default_lines, 20);
    }

    #[test]
    fn resume_config_explicit_overrides_roundtrip() {
        let cfg: ResumeConfig = toml::from_str(
            "show_banner = false\nauto_expand = true\nexpand_default_lines = 50\nshow_recap = true",
        )
        .unwrap();
        assert!(!cfg.show_banner);
        assert!(cfg.auto_expand);
        assert_eq!(cfg.expand_default_lines, 50);
        assert!(cfg.show_recap);
    }
}
