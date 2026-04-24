// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Configuration for the per-turn completion notification subsystem.
//!
//! Notifications are best-effort, fire-and-forget signals sent after each agent turn
//! completes. Two channels are supported: macOS native banners (via `osascript`)
//! and an ntfy-compatible JSON webhook POST.
//!
//! # Defaults
//!
//! All fields default to disabled so existing configs are not affected.
//!
//! # Examples
//!
//! ```toml
//! [notifications]
//! enabled = true
//! macos_native = true
//! webhook_url = "https://ntfy.sh"
//! webhook_topic = "my-topic-here"
//! title = "Zeph"
//! min_turn_duration_ms = 3000
//! only_on_error = false
//! ```

use serde::{Deserialize, Serialize};

fn default_title() -> String {
    "Zeph".to_owned()
}

/// Configuration for the per-turn completion notifier.
///
/// Both channels (macOS and webhook) are independently enableable.
/// At least one channel must be reachable for a notification to fire.
// Config structs legitimately use multiple boolean flags — each maps to a distinct TOML key.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NotificationsConfig {
    /// Master switch. When `false`, no notifications are sent regardless of other fields.
    #[serde(default)]
    pub enabled: bool,

    /// Send a macOS Notification Center banner via `osascript`.
    ///
    /// Silently no-ops on non-macOS platforms.
    #[serde(default)]
    pub macos_native: bool,

    /// URL for the ntfy-compatible webhook endpoint (e.g. `"https://ntfy.sh"`).
    ///
    /// Empty string or absent means the webhook channel is disabled.
    #[serde(default)]
    pub webhook_url: Option<String>,

    /// ntfy topic. Required when `webhook_url` is set; ignored otherwise.
    #[serde(default)]
    pub webhook_topic: Option<String>,

    /// Notification title shown in banners and webhook payloads.
    #[serde(default = "default_title")]
    pub title: String,

    /// Minimum successful-turn wall-clock duration in milliseconds before a notification fires.
    ///
    /// Set to `0` to always notify. Does NOT apply to error turns — errors always fire
    /// regardless of duration.
    #[serde(default)]
    pub min_turn_duration_ms: u64,

    /// When `true`, only fire on turns that completed with an error.
    #[serde(default)]
    pub only_on_error: bool,

    /// Allow non-HTTPS webhook URLs.
    ///
    /// When `false` (the default) only `https://` webhook URLs are accepted.
    /// Set to `true` to allow `http://` URLs for local testing only — never use
    /// in production as the notification payload is sent in plaintext.
    #[serde(default)]
    pub webhook_allow_insecure: bool,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            macos_native: false,
            webhook_url: None,
            webhook_topic: None,
            title: default_title(),
            min_turn_duration_ms: 0,
            only_on_error: false,
            webhook_allow_insecure: false,
        }
    }
}
