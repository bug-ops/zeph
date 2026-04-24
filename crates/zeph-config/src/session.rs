// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Session-scoped user experience settings (#3064).
//!
//! Configures behaviours that shape the user's experience per session, such as
//! showing a recap of the previous conversation on resume.

use serde::{Deserialize, Serialize};

/// Top-level `[session]` config block.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
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
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            recap: RecapConfig::default(),
            provider_persistence: true,
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
    /// An empty string falls back to the primary provider. Default: `""`.
    pub provider: String,

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
            provider: String::new(),
            max_input_messages: 20,
        }
    }
}
