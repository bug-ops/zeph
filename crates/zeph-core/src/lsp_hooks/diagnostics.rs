// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Diagnostics-on-save hook: fetches LSP diagnostics via mcpls after `write`.

use std::sync::Arc;

use serde::Deserialize;
use serde::de::{Deserializer, IgnoredAny};
use zeph_mcp::McpCaller;
use zeph_memory::TokenCounter;

use crate::config::{DiagnosticSeverity, LspConfig};
use zeph_sanitizer::{ContentSanitizer, ContentSource, ContentSourceKind};

use super::LspNote;

/// Diagnostic severity as received from the LSP bridge.
///
/// mcpls serializes severity as a lowercase string (`"error"`); raw LSP uses the
/// integers 1..=4. Missing or unrecognized values are treated as `Error` so that
/// a diagnostic is never silently dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LspSeverity {
    Error,
    Warning,
    Info,
    Hint,
}

impl LspSeverity {
    /// Lower rank is more severe.
    fn rank(self) -> u8 {
        match self {
            Self::Error => 1,
            Self::Warning => 2,
            Self::Info => 3,
            Self::Hint => 4,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Info => "info",
            Self::Hint => "hint",
        }
    }

    fn from_number(n: u64) -> Self {
        match n {
            2 => Self::Warning,
            3 => Self::Info,
            4 => Self::Hint,
            _ => Self::Error,
        }
    }

    fn from_text(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "warning" | "warn" => Self::Warning,
            "information" | "info" => Self::Info,
            "hint" => Self::Hint,
            _ => Self::Error,
        }
    }
}

impl<'de> Deserialize<'de> for LspSeverity {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Number(u64),
            Text(String),
            Other(IgnoredAny),
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::Number(n) => Self::from_number(n),
            Raw::Text(s) => Self::from_text(&s),
            Raw::Other(_) => Self::Error,
        })
    }
}

/// Rank of the configured minimum severity, comparable with [`LspSeverity::rank`].
fn threshold_rank(min: DiagnosticSeverity) -> u8 {
    match min {
        DiagnosticSeverity::Error => 1,
        DiagnosticSeverity::Warning => 2,
        DiagnosticSeverity::Info => 3,
        _ => 4,
    }
}

#[derive(Debug, Deserialize)]
struct WirePosition {
    #[serde(default)]
    line: u64,
}

#[derive(Debug, Deserialize)]
struct WireRange {
    start: WirePosition,
}

/// One diagnostic entry as returned by `get_diagnostics`; unknown fields are ignored.
#[derive(Debug, Deserialize)]
pub(super) struct WireDiagnostic {
    #[serde(default = "default_severity")]
    severity: LspSeverity,
    message: Option<String>,
    range: Option<WireRange>,
}

fn default_severity() -> LspSeverity {
    LspSeverity::Error
}

/// Format a single diagnostic entry into a display line.
///
/// Extracted for unit testing independently of MCP calls.
pub(super) fn format_diagnostic(file_path: &str, d: &WireDiagnostic) -> String {
    // LSP lines are 0-indexed; add 1 for display.
    let line = d
        .range
        .as_ref()
        .map_or(0, |r| r.start.line.saturating_add(1));
    let severity_str = d.severity.label();
    let message = d.message.as_deref().unwrap_or("(no message)");
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
/// the resolved value is not a JSON array. Entries that are not valid diagnostics are skipped.
fn parse_diagnostics_json(json_text: &str) -> Option<Vec<WireDiagnostic>> {
    let parsed: serde_json::Value = serde_json::from_str(json_text).ok()?;
    let array = parsed.get("diagnostics").unwrap_or(&parsed).as_array()?;
    // One malformed entry (e.g. a `MarkupContent` message) must not drop the whole batch.
    Some(
        array
            .iter()
            .filter_map(|item| WireDiagnostic::deserialize(item).ok())
            .collect(),
    )
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

    let threshold = threshold_rank(config.diagnostics.min_severity);
    let max_per_file = config.diagnostics.max_per_file;
    let total_diagnostics = diagnostics.len();

    let lines: Vec<String> = diagnostics
        .iter()
        .filter(|d| d.severity.rank() <= threshold)
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

    fn diag(severity: u64, message: &str, line: u64) -> WireDiagnostic {
        serde_json::from_value(serde_json::json!({
            "severity": severity,
            "message": message,
            "range": { "start": { "line": line } }
        }))
        .unwrap()
    }

    fn wire(v: serde_json::Value) -> WireDiagnostic {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn threshold_rank_values() {
        assert_eq!(threshold_rank(DiagnosticSeverity::Error), 1);
        assert_eq!(threshold_rank(DiagnosticSeverity::Warning), 2);
        assert_eq!(threshold_rank(DiagnosticSeverity::Info), 3);
        assert_eq!(threshold_rank(DiagnosticSeverity::Hint), 4);
    }

    #[test]
    fn severity_parses_strings_and_integers() {
        let cases = [
            (serde_json::json!("error"), LspSeverity::Error),
            (serde_json::json!("warning"), LspSeverity::Warning),
            (serde_json::json!("information"), LspSeverity::Info),
            (serde_json::json!("info"), LspSeverity::Info),
            (serde_json::json!("hint"), LspSeverity::Hint),
            (serde_json::json!("ERROR"), LspSeverity::Error),
            (serde_json::json!(1), LspSeverity::Error),
            (serde_json::json!(2), LspSeverity::Warning),
            (serde_json::json!(3), LspSeverity::Info),
            (serde_json::json!(4), LspSeverity::Hint),
        ];
        for (raw, expected) in cases {
            let d = wire(serde_json::json!({ "severity": raw.clone() }));
            assert_eq!(d.severity, expected, "severity {raw}");
        }
    }

    #[test]
    fn severity_missing_or_unknown_counts_as_error() {
        for d in [
            serde_json::json!({}),
            serde_json::json!({ "severity": null }),
            serde_json::json!({ "severity": "bogus" }),
            serde_json::json!({ "severity": 0 }),
            serde_json::json!({ "severity": 9 }),
            serde_json::json!({ "severity": [1] }),
        ] {
            assert_eq!(wire(d).severity, LspSeverity::Error);
        }
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
        let d = wire(serde_json::json!({ "severity": 1, "range": { "start": { "line": 0 } } }));
        let line = format_diagnostic("f.rs", &d);
        assert!(line.contains("(no message)"));
    }

    #[test]
    fn format_diagnostic_missing_range() {
        let d = wire(serde_json::json!({ "severity": 1, "message": "oops" }));
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

    fn mcpls_payload(severity: &str) -> String {
        serde_json::json!({
            "diagnostics": [{
                "code": "E0308",
                "message": "mismatched types",
                "range": { "start": { "line": 9, "character": 4 }, "end": { "line": 9, "character": 8 } },
                "severity": severity,
                "source": "rustc"
            }]
        })
        .to_string()
    }

    async fn fetch_with_min(
        payload: &str,
        min: DiagnosticSeverity,
    ) -> Option<crate::lsp_hooks::LspNote> {
        use zeph_sanitizer::ContentIsolationConfig;

        let mock = RecordingCaller::new().with_text(payload);
        let mut config = LspConfig::default();
        config.diagnostics.min_severity = min;
        let tc = Arc::new(TokenCounter::default());
        let sanitizer = ContentSanitizer::new(&ContentIsolationConfig::default());
        fetch_diagnostics(&mock, &config, "src/lib.rs", &tc, &sanitizer).await
    }

    #[tokio::test]
    async fn fetch_diagnostics_keeps_mcpls_string_severity() {
        let note = fetch_with_min(&mcpls_payload("error"), DiagnosticSeverity::Error)
            .await
            .expect("string-severity error diagnostic must be surfaced");
        assert_eq!(note.kind, "diagnostics");
        assert!(
            note.content
                .contains("src/lib.rs:10 error: mismatched types")
        );
    }

    #[tokio::test]
    async fn fetch_diagnostics_threshold_filtering() {
        use DiagnosticSeverity::{Error, Hint, Info, Warning};
        let cases = [
            ("error", Error, true),
            ("warning", Error, false),
            ("warning", Warning, true),
            ("information", Warning, false),
            ("information", Info, true),
            ("hint", Info, false),
            ("hint", Hint, true),
        ];
        for (sev, min, kept) in cases {
            let note = fetch_with_min(&mcpls_payload(sev), min).await;
            assert_eq!(note.is_some(), kept, "severity {sev} vs min {min:?}");
        }
    }

    #[tokio::test]
    async fn fetch_diagnostics_missing_severity_kept_at_error_threshold() {
        let payload = r#"[{"message":"no severity","range":{"start":{"line":0}}}]"#;
        let note = fetch_with_min(payload, DiagnosticSeverity::Error).await;
        assert!(note.is_some());
    }

    #[test]
    fn parse_diagnostics_json_skips_only_the_malformed_entry() {
        let json = r#"[
            {"severity": 1, "message": "first", "range": {"start": {"line": 0}}},
            {"severity": 1, "message": {"kind": "markdown", "value": "rich"}},
            {"severity": 1, "message": "third", "range": {"start": {"line": -4}}},
            {"severity": 2, "message": "fourth", "range": {"start": {"line": 3}}}
        ]"#;
        let parsed = parse_diagnostics_json(json).expect("array");
        let messages: Vec<_> = parsed.iter().filter_map(|d| d.message.as_deref()).collect();
        assert_eq!(messages, ["first", "fourth"]);
    }

    #[test]
    fn huge_line_number_does_not_overflow_when_formatted() {
        let d = wire(serde_json::json!({"message": "m", "range": {"start": {"line": u64::MAX}}}));
        assert!(format_diagnostic("f.rs", &d).starts_with(&format!("f.rs:{}", u64::MAX)));
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
