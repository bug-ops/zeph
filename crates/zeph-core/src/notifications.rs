// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Best-effort per-turn completion notifier.
//!
//! Fires after each agent turn completes via two independent channels:
//! - **macOS native** — `osascript` banner via stdin (no argument-injection risk)
//! - **ntfy webhook** — JSON POST to an ntfy-compatible endpoint
//!
//! All notifications are fire-and-forget: failures are logged at `warn` level and
//! never propagated to the caller. Secrets are redacted before any payload leaves
//! the process.
//!
//! # Gating
//!
//! [`Notifier::should_fire`] applies all gate conditions in order:
//! 1. Master `enabled` switch must be `true`
//! 2. `llm_requests == 0` → skip (slash commands, cache-only, security-blocked turns)
//! 3. `only_on_error && !is_error` → skip
//! 4. Duration gate (`min_turn_duration_ms`) applies only to successful turns;
//!    error turns always fire regardless of duration
//!
//! # Examples
//!
//! ```no_run
//! use zeph_core::notifications::{Notifier, TurnSummary, TurnExitStatus};
//! use zeph_config::NotificationsConfig;
//!
//! let cfg = NotificationsConfig {
//!     enabled: true,
//!     macos_native: true,
//!     ..Default::default()
//! };
//! let notifier = Notifier::new(cfg);
//! let summary = TurnSummary {
//!     duration_ms: 5000,
//!     preview: "Done. Files updated.".to_owned(),
//!     tool_calls: 2,
//!     llm_requests: 1,
//!     exit_status: TurnExitStatus::Success,
//! };
//! // Fire and forget — errors are logged, never propagated.
//! notifier.fire(&summary);
//! ```

use std::time::Duration;

use serde::Serialize;
use tracing::warn;
use zeph_config::NotificationsConfig;

use crate::redact::scrub_content;

// ── Public types ─────────────────────────────────────────────────────────────

/// Whether a turn completed successfully or with an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnExitStatus {
    /// Turn completed without error.
    Success,
    /// Turn completed with an error (tool failure, LLM error, etc.).
    Error,
}

/// Lightweight summary of a completed agent turn used as notification input.
///
/// Built by the agent loop after `channel.flush_chunks()` and passed to
/// [`Notifier::fire`]. Contains only what is needed for gate decisions and
/// notification body assembly — no LLM payloads or raw tool outputs.
#[derive(Debug, Clone)]
pub struct TurnSummary {
    /// Total wall-clock duration of the turn in milliseconds.
    pub duration_ms: u64,
    /// First ≤ 160 chars of the assistant response, already redacted by the caller.
    pub preview: String,
    /// Number of tool calls dispatched this turn.
    pub tool_calls: u32,
    /// Number of completed LLM round-trips this turn.
    /// Zero for slash commands, cache-only turns, and security-blocked inputs.
    pub llm_requests: u32,
    /// Whether the turn ended with an error.
    pub exit_status: TurnExitStatus,
}

/// Per-turn completion notifier.
///
/// Holds a shared [`reqwest::Client`] and the resolved config. Construct once at
/// agent startup via [`Notifier::new`] and call [`Notifier::fire`] after each turn.
///
/// All I/O is spawned onto the Tokio runtime via `tokio::spawn`; `fire` returns
/// immediately without blocking the agent loop.
///
/// Cloning is cheap — `reqwest::Client` is an `Arc`-backed handle.
#[derive(Clone)]
pub struct Notifier {
    cfg: NotificationsConfig,
    http: reqwest::Client,
}

impl Notifier {
    /// Create a notifier from a [`NotificationsConfig`].
    ///
    /// Constructs a shared HTTP client with a 5-second connect timeout. The client
    /// is reused across all webhook calls for the agent session.
    ///
    /// If `webhook_url` is set but fails URL validation (unparseable or non-HTTP(S)
    /// scheme), it is cleared to `None` and a warning is logged. This prevents SSRF
    /// via malformed URLs (e.g. `file://`, `ftp://`).
    #[must_use]
    pub fn new(cfg: NotificationsConfig) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        let mut cfg = cfg;
        if cfg
            .webhook_url
            .as_deref()
            .is_some_and(|url| !validate_webhook_url(url, cfg.webhook_allow_insecure))
        {
            cfg.webhook_url = None;
        }
        Self { cfg, http }
    }

    /// Evaluate all gate conditions and return `true` when the notification should fire.
    ///
    /// Gates applied in order (all must pass):
    /// 1. `enabled` is `true`
    /// 2. `summary.llm_requests > 0` (zero-LLM turns are never notified)
    /// 3. If `only_on_error`: turn must have errored
    /// 4. For successful turns: `duration_ms >= min_turn_duration_ms`
    ///    (error turns bypass the duration gate)
    #[must_use]
    pub fn should_fire(&self, summary: &TurnSummary) -> bool {
        if !self.cfg.enabled {
            return false;
        }
        // Gate S6: never notify for zero-LLM turns (slash commands, cache hits, etc.)
        // Exception M8 from critic: allow zero-LLM errors through so setup failures surface.
        if summary.llm_requests == 0 && summary.exit_status == TurnExitStatus::Success {
            return false;
        }
        match summary.exit_status {
            // Gate S4: errors always fire, bypassing the duration gate.
            TurnExitStatus::Error => true,
            TurnExitStatus::Success => {
                if self.cfg.only_on_error {
                    return false;
                }
                // Duration gate applies only to successful turns.
                summary.duration_ms >= self.cfg.min_turn_duration_ms
            }
        }
    }

    /// Fire all enabled notification channels for this turn summary.
    ///
    /// Returns immediately — all I/O is spawned as a background task. Failures
    /// are logged at `warn` level and never propagated. The spawned task has an
    /// internal 5-second per-channel timeout.
    pub fn fire(&self, summary: &TurnSummary) {
        let cfg = self.cfg.clone();
        let http = self.http.clone();
        let summary = summary.clone();

        tokio::spawn(async move {
            fire_all_channels(&cfg, &http, &summary).await;
        });
    }

    /// Fire a test notification with a fixed message.
    ///
    /// Used by the `zeph notify test` CLI subcommand. Returns an error if all
    /// channels are disabled or if every channel failed.
    ///
    /// # Errors
    ///
    /// - `NotifyTestError::AllDisabled` — no channel is enabled
    /// - `NotifyTestError::MacOsFailed` — macOS notification failed (macOS only)
    /// - `NotifyTestError::WebhookFailed` — webhook POST failed
    pub async fn fire_test(&self) -> Result<(), NotifyTestError> {
        if !self.cfg.enabled {
            return Err(NotifyTestError::MasterSwitchDisabled);
        }

        let macos_enabled = self.cfg.macos_native;
        let webhook_enabled = self.cfg.webhook_url.is_some() && self.cfg.webhook_topic.is_some();

        if !macos_enabled && !webhook_enabled {
            return Err(NotifyTestError::AllDisabled);
        }

        let summary = TurnSummary {
            duration_ms: 0,
            preview: "Zeph is working".to_owned(),
            tool_calls: 0,
            llm_requests: 1,
            exit_status: TurnExitStatus::Success,
        };

        #[cfg(target_os = "macos")]
        if macos_enabled {
            fire_macos_native(&self.cfg.title, "Zeph is working")
                .await
                .map_err(|e| NotifyTestError::MacOsFailed(e.to_string()))?;
        }

        if let (Some(url), Some(topic)) = (&self.cfg.webhook_url, &self.cfg.webhook_topic) {
            fire_webhook(&self.http, url, &self.cfg.title, topic, &summary)
                .await
                .map_err(|e| NotifyTestError::WebhookFailed(e.to_string()))?;
        }

        Ok(())
    }
}

/// Error returned by [`Notifier::fire_test`].
#[derive(Debug, thiserror::Error)]
pub enum NotifyTestError {
    /// The master `notifications.enabled` switch is `false`.
    #[error("notifications are disabled (set notifications.enabled = true to enable)")]
    MasterSwitchDisabled,
    /// No channels are enabled in the current configuration.
    #[error("all notification channels are disabled")]
    AllDisabled,
    /// macOS notification failed.
    #[error("macOS notification failed: {0}")]
    MacOsFailed(String),
    /// Webhook POST failed.
    #[error("webhook notification failed: {0}")]
    WebhookFailed(String),
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Fire all enabled channels for `summary`. Called from a spawned task.
async fn fire_all_channels(
    cfg: &NotificationsConfig,
    http: &reqwest::Client,
    summary: &TurnSummary,
) {
    let title = &cfg.title;

    #[cfg(target_os = "macos")]
    {
        let message = build_notification_message(summary);
        if cfg.macos_native
            && let Err(e) = fire_macos_native(title, &message).await
        {
            warn!(error = %e, "macOS notification failed");
        }
    }

    if let (Some(url), Some(topic)) = (&cfg.webhook_url, &cfg.webhook_topic)
        && let Err(e) = fire_webhook(http, url, title, topic, summary).await
    {
        warn!(error = %e, "webhook notification failed");
    }
}

/// Build the notification body from a turn summary, applying secret redaction.
fn build_notification_message(summary: &TurnSummary) -> String {
    let status = if summary.exit_status == TurnExitStatus::Error {
        "Error"
    } else {
        "Done"
    };

    // Apply scrub_content to redact any secrets that may be in the preview.
    let safe_preview = scrub_content(&summary.preview);

    if safe_preview.is_empty() {
        format!("{status} — {dur}ms", dur = summary.duration_ms)
    } else {
        format!(
            "{status} — {dur}ms\n{preview}",
            dur = summary.duration_ms,
            preview = safe_preview,
        )
    }
}

/// Sanitize a string for safe inclusion inside an `AppleScript` `"..."` literal.
///
/// Steps applied in order (order is important):
/// 1. Replace all ASCII control characters (< 0x20) and Unicode control chars with space
///    (tab `\t` is also replaced — single-line banners only)
/// 2. Replace newlines `\n` and carriage returns `\r` with a single space
/// 3. Truncate to `max` chars, appending `…` when cut
/// 4. Strip `\` and `"` — `AppleScript` does not support backslash escaping inside strings,
///    so these characters must be removed to prevent injection
///
/// # Examples
///
/// ```
/// # use zeph_core::notifications::sanitize_applescript_payload;
/// let s = sanitize_applescript_payload("Hello\nWorld\"", 200);
/// assert_eq!(s, "Hello World");
/// ```
#[must_use]
pub fn sanitize_applescript_payload(s: &str, max: usize) -> String {
    // Step 1 + 2: normalise control characters and Unicode line/paragraph separators.
    // U+2028 (LINE SEPARATOR) and U+2029 (PARAGRAPH SEPARATOR) are not in the Unicode Cc
    // category but still break AppleScript string literals, so they are explicitly replaced.
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_control() || c == '\u{2028}' || c == '\u{2029}' {
                ' '
            } else {
                c
            }
        })
        .collect();
    // Step 3: truncate to `max` char count (not bytes).
    let char_count = cleaned.chars().count();
    let truncated: String = if char_count > max {
        let end = cleaned
            .char_indices()
            .nth(max)
            .map_or(cleaned.len(), |(i, _)| i);
        let mut t = cleaned[..end].to_owned();
        t.push('…');
        t
    } else {
        cleaned
    };
    // Step 4: strip characters that cannot be safely embedded in an AppleScript string literal.
    // AppleScript does not use backslash escaping inside strings — the only safe approach
    // is to remove double-quote and backslash characters entirely.
    truncated.replace(['\\', '"'], "")
}

/// Validate a webhook URL for safe use as a notification endpoint.
///
/// Returns `true` when the URL is acceptable. Logs a warning and returns `false`
/// when the URL is unparseable or uses a non-HTTP(S) scheme. Accepts `http://`
/// only when `allow_insecure` is `true` (opt-in for local testing).
fn validate_webhook_url(url: &str, allow_insecure: bool) -> bool {
    match url.parse::<reqwest::Url>() {
        Ok(parsed) => {
            if parsed.scheme() == "https" {
                return true;
            }
            if allow_insecure && parsed.scheme() == "http" {
                warn!(
                    "webhook_url uses insecure HTTP scheme; set webhook_allow_insecure=false for production"
                );
                return true;
            }
            warn!(
                scheme = parsed.scheme(),
                "webhook_url has non-HTTP(S) scheme — channel disabled"
            );
            false
        }
        Err(e) => {
            warn!(error = %e, "webhook_url is not a valid URL — channel disabled");
            false
        }
    }
}

/// Fire a macOS Notification Center banner via osascript (stdin-fed to avoid arg injection).
#[cfg(target_os = "macos")]
async fn fire_macos_native(
    title: &str,
    message: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::AsyncWriteExt as _;
    use tokio::process::Command;

    let safe_title = sanitize_applescript_payload(title, 120);
    let safe_message = sanitize_applescript_payload(message, 240);

    let script = format!(r#"display notification "{safe_message}" with title "{safe_title}""#);

    let mut child = Command::new("osascript")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(script.as_bytes()).await?;
        stdin.shutdown().await?;
    }

    // Wait up to 5s for osascript to complete; ignore exit status (best-effort).
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;

    Ok(())
}

/// ntfy-compatible JSON webhook body.
///
/// Matches the [ntfy publish-as-JSON](https://docs.ntfy.sh/publish/#publish-as-json) schema.
#[derive(Serialize)]
struct NtfyWebhookBody<'a> {
    topic: &'a str,
    title: &'a str,
    message: &'a str,
    tags: Vec<&'a str>,
    /// Priority 1–5. Default 3; error turns use 4.
    priority: u8,
}

/// POST a notification to an ntfy-compatible JSON endpoint.
async fn fire_webhook(
    client: &reqwest::Client,
    url: &str,
    title: &str,
    topic: &str,
    summary: &TurnSummary,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let message = build_notification_message(summary);
    let (tags, priority) = if summary.exit_status == TurnExitStatus::Error {
        (vec!["zeph", "error"], 4u8)
    } else {
        (vec!["zeph", "turn-complete"], 3u8)
    };

    let body = NtfyWebhookBody {
        topic,
        title,
        message: &message,
        tags,
        priority,
    };

    // Timeout is already set on the client (5s), but wrap for clarity.
    tokio::time::timeout(Duration::from_secs(5), client.post(url).json(&body).send())
        .await??
        .error_for_status()?;

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_config::NotificationsConfig;

    fn make_notifier(cfg: NotificationsConfig) -> Notifier {
        Notifier::new(cfg)
    }

    fn success_summary(duration_ms: u64, llm_requests: u32) -> TurnSummary {
        TurnSummary {
            duration_ms,
            preview: "All done.".to_owned(),
            tool_calls: 0,
            llm_requests,
            exit_status: TurnExitStatus::Success,
        }
    }

    fn error_summary(duration_ms: u64, llm_requests: u32) -> TurnSummary {
        TurnSummary {
            duration_ms,
            preview: "Error occurred.".to_owned(),
            tool_calls: 0,
            llm_requests,
            exit_status: TurnExitStatus::Error,
        }
    }

    // ── should_fire gate tests ────────────────────────────────────────────────

    #[test]
    fn should_fire_disabled_master_switch() {
        let n = make_notifier(NotificationsConfig {
            enabled: false,
            ..Default::default()
        });
        assert!(!n.should_fire(&success_summary(5000, 1)));
    }

    #[test]
    fn should_fire_zero_llm_success_skipped() {
        let n = make_notifier(NotificationsConfig {
            enabled: true,
            ..Default::default()
        });
        // Zero-LLM successful turns (slash commands, cache hits) are never notified.
        assert!(!n.should_fire(&success_summary(0, 0)));
    }

    #[test]
    fn should_fire_zero_llm_error_fires() {
        // Critic M8: zero-LLM errors (setup failures) should still fire.
        let n = make_notifier(NotificationsConfig {
            enabled: true,
            ..Default::default()
        });
        assert!(n.should_fire(&error_summary(0, 0)));
    }

    #[test]
    fn should_fire_only_on_error_skips_success() {
        let n = make_notifier(NotificationsConfig {
            enabled: true,
            only_on_error: true,
            ..Default::default()
        });
        assert!(!n.should_fire(&success_summary(5000, 1)));
    }

    #[test]
    fn should_fire_only_on_error_fires_on_error() {
        let n = make_notifier(NotificationsConfig {
            enabled: true,
            only_on_error: true,
            ..Default::default()
        });
        assert!(n.should_fire(&error_summary(100, 1)));
    }

    #[test]
    fn should_fire_duration_gate_success_below_threshold() {
        let n = make_notifier(NotificationsConfig {
            enabled: true,
            min_turn_duration_ms: 3000,
            ..Default::default()
        });
        assert!(!n.should_fire(&success_summary(2999, 1)));
    }

    #[test]
    fn should_fire_duration_gate_success_at_threshold() {
        let n = make_notifier(NotificationsConfig {
            enabled: true,
            min_turn_duration_ms: 3000,
            ..Default::default()
        });
        assert!(n.should_fire(&success_summary(3000, 1)));
    }

    #[test]
    fn should_fire_error_bypasses_duration_gate() {
        // Gate S4: errors always fire even when below min_turn_duration_ms.
        let n = make_notifier(NotificationsConfig {
            enabled: true,
            min_turn_duration_ms: 3000,
            ..Default::default()
        });
        assert!(n.should_fire(&error_summary(100, 1)));
    }

    // ── sanitize_applescript_payload tests ────────────────────────────────────

    #[test]
    fn sanitize_control_chars_replaced_with_space() {
        let result = sanitize_applescript_payload("Hello\nWorld", 200);
        // Newline becomes space, no quotes broken
        assert!(!result.contains('\n'));
        assert!(result.contains("Hello World"));
    }

    #[test]
    fn sanitize_quotes_stripped() {
        // AppleScript has no backslash escape — double quotes are stripped entirely.
        let result = sanitize_applescript_payload(r#"say "hi""#, 200);
        assert!(!result.contains('"'));
        assert_eq!(result, "say hi");
    }

    #[test]
    fn sanitize_backslash_stripped() {
        // AppleScript has no backslash escape — backslashes are stripped entirely.
        let result = sanitize_applescript_payload(r"C:\Users\foo", 200);
        assert_eq!(result, "C:Usersfoo");
    }

    #[test]
    fn sanitize_truncation_appends_ellipsis() {
        let long = "a".repeat(300);
        let result = sanitize_applescript_payload(&long, 200);
        assert!(result.ends_with('…'));
        // Char count should be max + 1 for the ellipsis.
        assert_eq!(result.chars().count(), 201);
    }

    #[test]
    fn sanitize_no_truncation_when_short() {
        let result = sanitize_applescript_payload("short", 200);
        assert_eq!(result, "short");
    }

    #[test]
    fn sanitize_injection_attempt() {
        // Classic AppleScript injection via closing the string and calling display dialog.
        let payload = r#""; display dialog "gotcha"; ""#;
        let result = sanitize_applescript_payload(payload, 200);
        // All `"` must be escaped; the script cannot terminate the outer string.
        assert!(!result.contains('"'));
    }

    #[test]
    fn sanitize_applescript_payload_empty() {
        assert_eq!(sanitize_applescript_payload("", 200), "");
    }

    #[test]
    fn sanitize_tab_replaced() {
        let result = sanitize_applescript_payload("a\tb", 200);
        assert_eq!(result, "a b");
    }

    #[test]
    fn sanitize_line_separators() {
        let s = "hello\u{2028}world\u{2029}end";
        let result = sanitize_applescript_payload(s, 200);
        assert!(!result.contains('\u{2028}'));
        assert!(!result.contains('\u{2029}'));
        assert_eq!(result, "hello world end");
    }

    // ── build_notification_message tests ──────────────────────────────────────

    #[test]
    fn notification_message_success() {
        let summary = success_summary(1234, 1);
        let msg = build_notification_message(&summary);
        assert!(msg.starts_with("Done"));
        assert!(msg.contains("1234ms"));
    }

    #[test]
    fn notification_message_error() {
        let summary = error_summary(500, 1);
        let msg = build_notification_message(&summary);
        assert!(msg.starts_with("Error"));
    }

    #[test]
    fn notification_message_redacts_secrets() {
        let summary = TurnSummary {
            duration_ms: 100,
            preview: "Done. Key: sk-abc123xyz".to_owned(),
            tool_calls: 0,
            llm_requests: 1,
            exit_status: TurnExitStatus::Success,
        };
        let msg = build_notification_message(&summary);
        assert!(!msg.contains("sk-abc123xyz"), "secret must be redacted");
        assert!(
            msg.contains("[REDACTED]"),
            "should contain redaction marker"
        );
    }
}
