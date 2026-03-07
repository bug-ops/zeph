// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Diagnostics-on-save hook: fetches LSP diagnostics via mcpls after `write`.

use std::sync::Arc;

use zeph_mcp::McpManager;
use zeph_memory::TokenCounter;

use crate::config::{DiagnosticSeverity, LspConfig};
use crate::sanitizer::{ContentSanitizer, ContentSource, ContentSourceKind};

use super::LspNote;

/// Minimum integer severity value that passes the configured filter.
///
/// LSP severity levels: 1=Error, 2=Warning, 3=Information, 4=Hint.
/// Lower numbers are more severe.
fn severity_threshold(min: DiagnosticSeverity) -> u64 {
    match min {
        DiagnosticSeverity::Error => 1,
        DiagnosticSeverity::Warning => 2,
        DiagnosticSeverity::Info => 3,
        DiagnosticSeverity::Hint => 4,
    }
}

/// Format a single diagnostic entry into a display line.
///
/// Extracted for unit testing independently of MCP calls.
pub(super) fn format_diagnostic(file_path: &str, d: &serde_json::Value) -> String {
    // LSP lines are 0-indexed; add 1 for display.
    let line = d
        .pointer("/range/start/line")
        .and_then(serde_json::Value::as_u64)
        .map_or(0, |l| l + 1);
    let severity_str = match d.get("severity").and_then(serde_json::Value::as_u64) {
        Some(1) => "error",
        Some(2) => "warning",
        Some(3) => "info",
        _ => "hint",
    };
    let message = d
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("(no message)");
    // Truncate message before formatting to avoid unbounded allocations.
    let safe_message: String = message.chars().take(200).collect();
    // Sanitize file_path: strip newlines/CR that could break line-oriented output.
    let safe_path: String = file_path
        .chars()
        .filter(|&c| c != '\n' && c != '\r')
        .collect();
    format!("{safe_path}:{line} {severity_str}: {safe_message}")
}

/// Fetch diagnostics for `file_path` from the configured mcpls MCP server.
///
/// Returns `None` on error or when there are no diagnostics meeting the filter.
pub(super) async fn fetch_diagnostics(
    manager: &Arc<McpManager>,
    config: &LspConfig,
    file_path: &str,
    token_counter: &Arc<TokenCounter>,
    sanitizer: &ContentSanitizer,
) -> Option<LspNote> {
    let timeout = std::time::Duration::from_secs(config.call_timeout_secs);
    let args = serde_json::json!({ "path": file_path });

    let call_result = match tokio::time::timeout(
        timeout,
        manager.call_tool(&config.mcp_server_id, "get_diagnostics", args),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::debug!(path = file_path, error = %e, "LSP diagnostics fetch failed");
            return None;
        }
        Err(_) => {
            tracing::debug!(path = file_path, "LSP diagnostics fetch timed out");
            return None;
        }
    };

    // Extract text content from the MCP response.
    // mcpls returns diagnostics as JSON text in the first content item.
    let json_text = call_result
        .content
        .iter()
        .find_map(|c| c.as_text().map(|t| t.text.as_str()))?;

    // Attempt to parse as a JSON array of diagnostic objects.
    // Expected shape: [{ "severity": 1, "message": "...", "range": { "start": { "line": N } } }]
    let diagnostics: Vec<serde_json::Value> = serde_json::from_str(json_text).unwrap_or_default();

    let threshold = severity_threshold(config.diagnostics.min_severity);
    let max_per_file = config.diagnostics.max_per_file;

    let lines: Vec<String> = diagnostics
        .iter()
        .filter(|d| {
            d.get("severity")
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|s| s <= threshold)
        })
        .take(max_per_file)
        .map(|d| format_diagnostic(file_path, d))
        .collect();

    if lines.is_empty() {
        return None;
    }

    let raw_content = lines.join("\n");

    // Sanitize via ContentSanitizer (injection pattern detection + spotlighting).
    let clean = sanitizer.sanitize(
        &raw_content,
        ContentSource::new(ContentSourceKind::McpResponse).with_identifier("mcpls/diagnostics"),
    );
    if !clean.injection_flags.is_empty() {
        tracing::warn!(
            path = file_path,
            flags = ?clean.injection_flags.iter().map(|f| f.pattern_name).collect::<Vec<_>>(),
            "LSP diagnostics contain injection patterns"
        );
    }

    let estimated_tokens = token_counter.count_tokens(&clean.body);

    Some(LspNote {
        kind: "diagnostics",
        content: clean.body,
        estimated_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag(severity: u64, message: &str, line: u64) -> serde_json::Value {
        serde_json::json!({
            "severity": severity,
            "message": message,
            "range": { "start": { "line": line } }
        })
    }

    #[test]
    fn severity_threshold_values() {
        assert_eq!(severity_threshold(DiagnosticSeverity::Error), 1);
        assert_eq!(severity_threshold(DiagnosticSeverity::Warning), 2);
        assert_eq!(severity_threshold(DiagnosticSeverity::Info), 3);
        assert_eq!(severity_threshold(DiagnosticSeverity::Hint), 4);
    }

    #[test]
    fn format_diagnostic_basic() {
        let d = diag(1, "type mismatch", 4);
        let line = format_diagnostic("src/main.rs", &d);
        assert_eq!(line, "src/main.rs:5 error: type mismatch");
    }

    #[test]
    fn format_diagnostic_warning() {
        let d = diag(2, "unused variable", 0);
        let line = format_diagnostic("lib.rs", &d);
        assert_eq!(line, "lib.rs:1 warning: unused variable");
    }

    #[test]
    fn format_diagnostic_strips_path_newlines() {
        let d = diag(1, "err", 0);
        let line = format_diagnostic("src/\nfoo.rs", &d);
        assert!(!line.contains('\n'), "newline in path must be stripped");
        assert!(line.contains("src/foo.rs:1 error: err"));
    }

    #[test]
    fn format_diagnostic_truncates_message() {
        let long_msg = "x".repeat(300);
        let d = diag(1, &long_msg, 0);
        let line = format_diagnostic("f.rs", &d);
        // message portion is truncated to 200 chars
        let msg_part: String = line.chars().skip("f.rs:1 error: ".len()).collect();
        assert_eq!(msg_part.chars().count(), 200);
    }

    #[test]
    fn format_diagnostic_no_message_field() {
        let d = serde_json::json!({ "severity": 1, "range": { "start": { "line": 0 } } });
        let line = format_diagnostic("f.rs", &d);
        assert!(line.contains("(no message)"));
    }

    #[test]
    fn format_diagnostic_missing_range() {
        let d = serde_json::json!({ "severity": 1, "message": "oops" });
        let line = format_diagnostic("f.rs", &d);
        // line defaults to 0 when range is absent
        assert_eq!(line, "f.rs:0 error: oops");
    }
}
