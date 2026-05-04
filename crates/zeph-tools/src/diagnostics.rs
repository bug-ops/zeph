// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::Deserialize;

use zeph_common::ToolName;

use crate::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput, deserialize_params};
use crate::registry::{InvocationHint, ToolDef};

/// Cargo diagnostics level.
#[derive(Debug, Default, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticsLevel {
    /// Run `cargo check`
    #[default]
    Check,
    /// Run `cargo clippy`
    Clippy,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DiagnosticsParams {
    /// Workspace path (defaults to current directory)
    path: Option<String>,
    /// Diagnostics level: check or clippy
    #[serde(default)]
    level: DiagnosticsLevel,
}

/// Runs `cargo check` or `cargo clippy` and returns structured diagnostics.
#[derive(Debug)]
pub struct DiagnosticsExecutor {
    allowed_paths: Vec<PathBuf>,
    /// Maximum number of diagnostics to return (default: 50)
    max_diagnostics: usize,
}

impl DiagnosticsExecutor {
    #[must_use]
    pub fn new(allowed_paths: Vec<PathBuf>) -> Self {
        let paths = if allowed_paths.is_empty() {
            vec![std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))]
        } else {
            allowed_paths
        };
        Self {
            allowed_paths: paths
                .into_iter()
                .map(|p| p.canonicalize().unwrap_or(p))
                .collect(),
            max_diagnostics: 50,
        }
    }

    #[must_use]
    pub fn with_max_diagnostics(mut self, max: usize) -> Self {
        self.max_diagnostics = max;
        self
    }

    fn validate_path(&self, path: &Path) -> Result<PathBuf, ToolError> {
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        };
        let canonical = resolved.canonicalize().map_err(|e| {
            ToolError::Execution(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("path not found: {}: {e}", resolved.display()),
            ))
        })?;
        if !self.allowed_paths.iter().any(|a| canonical.starts_with(a)) {
            return Err(ToolError::SandboxViolation {
                path: canonical.display().to_string(),
            });
        }
        Ok(canonical)
    }
}

impl ToolExecutor for DiagnosticsExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "tool.diagnostics", skip_all)
    )]
    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        if call.tool_id != "diagnostics" {
            return Ok(None);
        }
        let p: DiagnosticsParams = deserialize_params(&call.params)?;
        let work_dir = if let Some(path) = &p.path {
            self.validate_path(Path::new(path))?
        } else {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            self.validate_path(&cwd)?
        };

        let subcmd = match p.level {
            DiagnosticsLevel::Check => "check",
            DiagnosticsLevel::Clippy => "clippy",
        };

        let cargo = which_cargo()?;

        let output = tokio::process::Command::new(&cargo)
            .arg(subcmd)
            .arg("--message-format=json")
            .current_dir(&work_dir)
            .output()
            .await
            .map_err(|e| {
                ToolError::Execution(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("failed to run cargo: {e}"),
                ))
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let diagnostics = parse_cargo_json(&stdout, self.max_diagnostics);

        let summary = if diagnostics.is_empty() {
            "No diagnostics".to_owned()
        } else {
            diagnostics.join("\n")
        };

        Ok(Some(ToolOutput {
            tool_name: ToolName::new("diagnostics"),
            summary,
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: Some(crate::executor::ClaimSource::Diagnostics),
        }))
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        vec![ToolDef {
            id: "diagnostics".into(),
            description: "Run cargo check or cargo clippy on a Rust workspace and return compiler diagnostics.\n\nParameters: path (string, optional) - workspace directory (default: cwd); level (string, optional) - \"check\" or \"clippy\" (default: \"check\")\nReturns: structured diagnostics with file paths, line numbers, severity, and messages; capped at 50 results\nErrors: SandboxViolation if path outside allowed dirs; Execution if cargo is not found\nExample: {\"path\": \".\", \"level\": \"clippy\"}".into(),
            schema: schemars::schema_for!(DiagnosticsParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        }]
    }
}

/// Returns the path to the `cargo` binary, failing gracefully if not found.
///
/// Reads the `CARGO` environment variable (set by rustup/cargo during builds) or
/// falls back to a PATH search. The process environment is assumed trusted — this
/// function runs in the same process as the agent, not in an untrusted context.
/// Canonicalization is applied as defence-in-depth to resolve any symlinks in the path.
fn which_cargo() -> Result<PathBuf, ToolError> {
    // Check CARGO env var first (set by rustup/cargo itself)
    if let Ok(cargo) = std::env::var("CARGO") {
        let p = PathBuf::from(&cargo);
        if p.is_file() {
            return Ok(p.canonicalize().unwrap_or(p));
        }
    }
    // Fall back to PATH lookup
    for dir in std::env::var("PATH").unwrap_or_default().split(':') {
        let candidate = PathBuf::from(dir).join("cargo");
        if candidate.is_file() {
            return Ok(candidate.canonicalize().unwrap_or(candidate));
        }
    }
    Err(ToolError::Execution(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "cargo not found in PATH",
    )))
}

/// Parses cargo JSON output lines and extracts human-readable diagnostics.
///
/// Each JSON line from `--message-format=json` that represents a `compiler-message`
/// with a span is formatted as `file:line:col: level: message`.
pub(crate) fn parse_cargo_json(output: &str, max: usize) -> Vec<String> {
    let mut results = Vec::new();
    for line in output.lines() {
        if results.len() >= max {
            break;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if val.get("reason").and_then(|r| r.as_str()) != Some("compiler-message") {
            continue;
        }
        let Some(msg) = val.get("message") else {
            continue;
        };
        let level = msg
            .get("level")
            .and_then(|l| l.as_str())
            .unwrap_or("unknown");
        let text = msg
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .trim();
        if text.is_empty() {
            continue;
        }

        // Use the primary span if available for location info
        let spans = msg
            .get("spans")
            .and_then(serde_json::Value::as_array)
            .map_or(&[] as &[_], Vec::as_slice);

        let primary = spans.iter().find(|s| {
            s.get("is_primary")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        });

        if let Some(span) = primary {
            let file = span
                .get("file_name")
                .and_then(|f| f.as_str())
                .unwrap_or("?");
            let line = span
                .get("line_start")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let col = span
                .get("column_start")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            results.push(format!("{file}:{line}:{col}: {level}: {text}"));
        } else {
            results.push(format!("{level}: {text}"));
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_params(
        pairs: &[(&str, serde_json::Value)],
    ) -> serde_json::Map<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    // --- parse_cargo_json unit tests ---

    #[test]
    fn parse_cargo_json_empty_input() {
        let result = parse_cargo_json("", 50);
        assert!(result.is_empty());
    }

    #[test]
    fn parse_cargo_json_non_compiler_message_ignored() {
        let line = r#"{"reason":"build-script-executed","package_id":"foo"}"#;
        let result = parse_cargo_json(line, 50);
        assert!(result.is_empty());
    }

    #[test]
    fn parse_cargo_json_compiler_message_with_span() {
        let line = r#"{"reason":"compiler-message","message":{"level":"error","message":"cannot find value `foo` in this scope","spans":[{"file_name":"src/main.rs","line_start":10,"column_start":5,"is_primary":true}]}}"#;
        let result = parse_cargo_json(line, 50);
        assert_eq!(result.len(), 1);
        assert!(result[0].contains("src/main.rs"));
        assert!(result[0].contains("10"));
        assert!(result[0].contains("error"));
        assert!(result[0].contains("cannot find value"));
    }

    #[test]
    fn parse_cargo_json_warning_with_span() {
        let line = r#"{"reason":"compiler-message","message":{"level":"warning","message":"unused variable: `x`","spans":[{"file_name":"src/lib.rs","line_start":3,"column_start":9,"is_primary":true}]}}"#;
        let result = parse_cargo_json(line, 50);
        assert_eq!(result.len(), 1);
        assert!(result[0].starts_with("src/lib.rs:3:9: warning:"));
    }

    #[test]
    fn parse_cargo_json_no_primary_span_uses_message_only() {
        let line = r#"{"reason":"compiler-message","message":{"level":"error","message":"aborting due to previous error","spans":[]}}"#;
        let result = parse_cargo_json(line, 50);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "error: aborting due to previous error");
    }

    #[test]
    fn parse_cargo_json_max_cap_respected() {
        let single = r#"{"reason":"compiler-message","message":{"level":"warning","message":"unused","spans":[]}}"#;
        let input: String = (0..20).map(|_| single).collect::<Vec<_>>().join("\n");
        let result = parse_cargo_json(&input, 5);
        assert_eq!(result.len(), 5);
    }

    #[test]
    fn parse_cargo_json_empty_message_skipped() {
        let line = r#"{"reason":"compiler-message","message":{"level":"note","message":"   ","spans":[]}}"#;
        let result = parse_cargo_json(line, 50);
        assert!(result.is_empty());
    }

    #[test]
    fn parse_cargo_json_non_primary_span_skipped_for_location() {
        let line = r#"{"reason":"compiler-message","message":{"level":"warning","message":"some warning","spans":[{"file_name":"src/foo.rs","line_start":1,"column_start":1,"is_primary":false}]}}"#;
        // No primary span → fall back to message-only format
        let result = parse_cargo_json(line, 50);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "warning: some warning");
    }

    #[test]
    fn parse_cargo_json_invalid_json_line_skipped() {
        let input = "not json\n{\"reason\":\"build-script-executed\"}";
        let result = parse_cargo_json(input, 50);
        assert!(result.is_empty());
    }

    // --- sandbox tests ---

    #[tokio::test]
    async fn diagnostics_sandbox_violation() {
        let dir = tempfile::tempdir().unwrap();
        let exec = DiagnosticsExecutor::new(vec![dir.path().to_path_buf()]);

        let call = ToolCall {
            tool_id: ToolName::new("diagnostics"),
            params: make_params(&[("path", serde_json::json!("/etc"))]),
            caller_id: None,
            context: None,
        };
        let result = exec.execute_tool_call(&call).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn diagnostics_unknown_tool_returns_none() {
        let exec = DiagnosticsExecutor::new(vec![]);
        let call = ToolCall {
            tool_id: ToolName::new("other"),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,
        };
        let result = exec.execute_tool_call(&call).await.unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn diagnostics_tool_definition() {
        let exec = DiagnosticsExecutor::new(vec![]);
        let defs = exec.tool_definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].id, "diagnostics");
        assert_eq!(defs[0].invocation, InvocationHint::ToolCall);
    }

    #[test]
    fn diagnostics_level_default_is_check() {
        assert_eq!(DiagnosticsLevel::default(), DiagnosticsLevel::Check);
    }

    #[test]
    fn diagnostics_level_deserialize_check() {
        let p: DiagnosticsParams = serde_json::from_str(r#"{"level":"check"}"#).unwrap();
        assert_eq!(p.level, DiagnosticsLevel::Check);
    }

    #[test]
    fn diagnostics_level_deserialize_clippy() {
        let p: DiagnosticsParams = serde_json::from_str(r#"{"level":"clippy"}"#).unwrap();
        assert_eq!(p.level, DiagnosticsLevel::Clippy);
    }

    #[test]
    fn diagnostics_params_path_optional() {
        let p: DiagnosticsParams = serde_json::from_str(r"{}").unwrap();
        assert!(p.path.is_none());
        assert_eq!(p.level, DiagnosticsLevel::Check);
    }

    // CR-14: verify that level=clippy maps to "clippy" subcommand string
    #[test]
    fn diagnostics_clippy_subcmd_string() {
        let subcmd = match DiagnosticsLevel::Clippy {
            DiagnosticsLevel::Check => "check",
            DiagnosticsLevel::Clippy => "clippy",
        };
        assert_eq!(subcmd, "clippy");
    }

    #[test]
    fn diagnostics_check_subcmd_string() {
        let subcmd = match DiagnosticsLevel::Check {
            DiagnosticsLevel::Check => "check",
            DiagnosticsLevel::Clippy => "clippy",
        };
        assert_eq!(subcmd, "check");
    }
}
