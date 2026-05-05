// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Diagnostics-on-save hook: fetches LSP diagnostics via mcpls after `write`.

use std::sync::Arc;

use zeph_mcp::McpCaller;
use zeph_memory::TokenCounter;

use crate::config::{DiagnosticSeverity, LspConfig};
use zeph_sanitizer::{ContentSanitizer, ContentSource, ContentSourceKind};

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

/// Parse a diagnostics JSON response from mcpls.
///
/// Handles both the legacy bare-array format and the v0.3.6+ object wrapper
/// `{"diagnostics": [...]}`. Returns `None` if the text cannot be parsed or
/// the resolved value is not a JSON array.
fn parse_diagnostics_json(json_text: &str) -> Option<Vec<serde_json::Value>> {
    let parsed: serde_json::Value = serde_json::from_str(json_text).ok()?;
    let array = parsed.get("diagnostics").unwrap_or(&parsed);
    serde_json::from_value(array.clone()).ok()
}

/// Fetch diagnostics for `file_path` from the configured mcpls MCP server.
///
/// Returns `None` on error or when there are no diagnostics meeting the filter.
pub(super) async fn fetch_diagnostics(
    manager: &impl McpCaller,
    config: &LspConfig,
    file_path: &str,
    token_counter: &Arc<TokenCounter>,
    sanitizer: &ContentSanitizer,
) -> Option<LspNote> {
    let timeout = std::time::Duration::from_secs(config.call_timeout_secs);
    let args = serde_json::json!({ "file_path": file_path });

    tracing::debug!(
        path = file_path,
        timeout_secs = config.call_timeout_secs,
        "LSP diagnostics: calling get_diagnostics"
    );

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

    let Some(diagnostics) = parse_diagnostics_json(json_text) else {
        tracing::debug!(
            path = file_path,
            "LSP diagnostics: failed to parse response JSON"
        );
        return None;
    };

    let threshold = severity_threshold(config.diagnostics.min_severity);
    let max_per_file = config.diagnostics.max_per_file;
    let total_diagnostics = diagnostics.len();

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
        tracing::debug!(
            path = file_path,
            diagnostics = total_diagnostics,
            threshold,
            max_per_file,
            "LSP diagnostics: result empty after filtering"
        );
        return None;
    }

    tracing::debug!(
        path = file_path,
        diagnostics = total_diagnostics,
        injected = lines.len(),
        threshold,
        max_per_file,
        "LSP diagnostics: injecting diagnostics note"
    );

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

    // Tests that verify fetch_diagnostics passes the correct argument key ("file_path")
    // to McpManager.call_tool. These are regression tests for issue #1538 where the
    // wrong key ("path") was used, causing silent MCP call failures.

    use std::sync::Arc;

    use crate::lsp_hooks::test_helpers::RecordingCaller;

    #[tokio::test]
    async fn fetch_diagnostics_passes_file_path_key() {
        use zeph_memory::TokenCounter;

        use crate::config::LspConfig;
        use zeph_sanitizer::{ContentIsolationConfig, ContentSanitizer};

        let diagnostics_json = serde_json::json!([
            { "severity": 1, "message": "type error", "range": { "start": { "line": 0 } } }
        ])
        .to_string();

        let mock = RecordingCaller::new().with_text(&diagnostics_json);
        let config = LspConfig::default();
        let tc = Arc::new(TokenCounter::default());
        let sanitizer = ContentSanitizer::new(&ContentIsolationConfig::default());

        fetch_diagnostics(&mock, &config, "src/lib.rs", &tc, &sanitizer).await;

        let calls = mock.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "expected exactly one call_tool invocation");
        let args = &calls[0].2;
        assert!(
            args.get("file_path").is_some(),
            "call_tool args must contain 'file_path' key, got: {args}"
        );
        assert!(
            args.get("path").is_none(),
            "call_tool args must NOT contain old 'path' key, got: {args}"
        );
        assert_eq!(calls[0].1, "get_diagnostics");
    }

    #[tokio::test]
    async fn fetch_diagnostics_parses_object_wrapper() {
        use zeph_memory::TokenCounter;

        use crate::config::LspConfig;
        use zeph_sanitizer::{ContentIsolationConfig, ContentSanitizer};

        // mcpls v0.3.6+ returns {"diagnostics": [...]} instead of a bare array.
        let wrapped = serde_json::json!({
            "diagnostics": [
                { "severity": 1, "message": "type error", "range": { "start": { "line": 0 } } }
            ]
        })
        .to_string();

        let mock = RecordingCaller::new().with_text(&wrapped);
        let config = LspConfig::default();
        let tc = Arc::new(TokenCounter::default());
        let sanitizer = ContentSanitizer::new(&ContentIsolationConfig::default());

        let result = fetch_diagnostics(&mock, &config, "src/lib.rs", &tc, &sanitizer).await;
        assert!(
            result.is_some(),
            "diagnostics wrapped in {{\"diagnostics\": [...]}} must be parsed successfully"
        );
        let note = result.unwrap();
        assert!(
            note.content.contains("type error"),
            "diagnostic message must appear in the note content"
        );
    }

    #[test]
    fn parse_diagnostics_json_bare_array() {
        let json = r#"[{"severity":1,"message":"err","range":{"start":{"line":0}}}]"#;
        let result = parse_diagnostics_json(json);
        assert!(result.is_some(), "bare array must be parsed");
        assert_eq!(result.unwrap().len(), 1);
    }

    #[test]
    fn parse_diagnostics_json_object_wrapper_empty() {
        let json = r#"{"diagnostics":[]}"#;
        let result = parse_diagnostics_json(json);
        assert!(
            result.is_some(),
            "object wrapper with empty array must return Some(vec![])"
        );
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn parse_diagnostics_json_object_missing_diagnostics_key() {
        // An object without a "diagnostics" key is treated as if the value itself is the array.
        // Since an object is not a Vec<Value>, this must return None.
        let json = r#"{"other_key": [1, 2, 3]}"#;
        let result = parse_diagnostics_json(json);
        assert!(
            result.is_none(),
            "object without 'diagnostics' key must return None"
        );
    }

    #[test]
    fn parse_diagnostics_json_invalid_json() {
        let result = parse_diagnostics_json("not json at all {{{");
        assert!(result.is_none(), "invalid JSON must return None");
    }

    #[tokio::test]
    async fn fetch_diagnostics_file_path_value_matches_input() {
        use zeph_memory::TokenCounter;

        use crate::config::LspConfig;
        use zeph_sanitizer::{ContentIsolationConfig, ContentSanitizer};

        let mock = RecordingCaller::new().with_text("[]");
        let config = LspConfig::default();
        let tc = Arc::new(TokenCounter::default());
        let sanitizer = ContentSanitizer::new(&ContentIsolationConfig::default());

        fetch_diagnostics(
            &mock,
            &config,
            "crates/zeph-core/src/agent.rs",
            &tc,
            &sanitizer,
        )
        .await;

        let calls = mock.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0]
                .2
                .get("file_path")
                .and_then(serde_json::Value::as_str),
            Some("crates/zeph-core/src/agent.rs"),
            "file_path value must match the input path"
        );
    }
}
