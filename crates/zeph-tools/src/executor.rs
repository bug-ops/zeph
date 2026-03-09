// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt;

/// Data for rendering file diffs in the TUI.
#[derive(Debug, Clone)]
pub struct DiffData {
    pub file_path: String,
    pub old_content: String,
    pub new_content: String,
}

/// Structured tool invocation from LLM.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub tool_id: String,
    pub params: serde_json::Map<String, serde_json::Value>,
}

/// Cumulative filter statistics for a single tool execution.
#[derive(Debug, Clone, Default)]
pub struct FilterStats {
    pub raw_chars: usize,
    pub filtered_chars: usize,
    pub raw_lines: usize,
    pub filtered_lines: usize,
    pub confidence: Option<crate::FilterConfidence>,
    pub command: Option<String>,
    pub kept_lines: Vec<usize>,
}

impl FilterStats {
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn savings_pct(&self) -> f64 {
        if self.raw_chars == 0 {
            return 0.0;
        }
        (1.0 - self.filtered_chars as f64 / self.raw_chars as f64) * 100.0
    }

    #[must_use]
    pub fn estimated_tokens_saved(&self) -> usize {
        self.raw_chars.saturating_sub(self.filtered_chars) / 4
    }

    #[must_use]
    pub fn format_inline(&self, tool_name: &str) -> String {
        let cmd_label = self
            .command
            .as_deref()
            .map(|c| {
                let trimmed = c.trim();
                if trimmed.len() > 60 {
                    format!(" `{}…`", &trimmed[..57])
                } else {
                    format!(" `{trimmed}`")
                }
            })
            .unwrap_or_default();
        format!(
            "[{tool_name}]{cmd_label} {} lines \u{2192} {} lines, {:.1}% filtered",
            self.raw_lines,
            self.filtered_lines,
            self.savings_pct()
        )
    }
}

/// Structured result from tool execution.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub tool_name: String,
    pub summary: String,
    pub blocks_executed: u32,
    pub filter_stats: Option<FilterStats>,
    pub diff: Option<DiffData>,
    /// Whether this tool already streamed its output via `ToolEvent` channel.
    pub streamed: bool,
    /// Terminal ID when the tool was executed via IDE terminal (ACP terminal/* protocol).
    pub terminal_id: Option<String>,
    /// File paths touched by this tool call, for IDE follow-along (e.g. `ToolCallLocation`).
    pub locations: Option<Vec<String>>,
    /// Structured tool response payload for ACP intermediate `tool_call_update` notifications.
    pub raw_response: Option<serde_json::Value>,
}

impl fmt::Display for ToolOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.summary)
    }
}

pub const MAX_TOOL_OUTPUT_CHARS: usize = 30_000;

/// Truncate tool output that exceeds `MAX_TOOL_OUTPUT_CHARS` using head+tail split.
#[must_use]
pub fn truncate_tool_output(output: &str) -> String {
    truncate_tool_output_at(output, MAX_TOOL_OUTPUT_CHARS)
}

/// Truncate tool output that exceeds `max_chars` using head+tail split.
#[must_use]
pub fn truncate_tool_output_at(output: &str, max_chars: usize) -> String {
    if output.len() <= max_chars {
        return output.to_string();
    }

    let half = max_chars / 2;
    let head_end = output.floor_char_boundary(half);
    let tail_start = output.ceil_char_boundary(output.len() - half);
    let head = &output[..head_end];
    let tail = &output[tail_start..];
    let truncated = output.len() - head_end - (output.len() - tail_start);

    format!(
        "{head}\n\n... [truncated {truncated} chars, showing first and last ~{half} chars] ...\n\n{tail}"
    )
}

/// Event emitted during tool execution for real-time UI updates.
#[derive(Debug, Clone)]
pub enum ToolEvent {
    Started {
        tool_name: String,
        command: String,
    },
    OutputChunk {
        tool_name: String,
        command: String,
        chunk: String,
    },
    Completed {
        tool_name: String,
        command: String,
        output: String,
        success: bool,
        filter_stats: Option<FilterStats>,
        diff: Option<DiffData>,
    },
}

pub type ToolEventTx = tokio::sync::mpsc::UnboundedSender<ToolEvent>;

/// Classifies a tool error as transient (retryable) or permanent (abort immediately).
///
/// Transient errors may succeed on retry (network blips, race conditions).
/// Permanent errors will not succeed regardless of retries (policy, bad args, not found).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum ErrorKind {
    Transient,
    Permanent,
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transient => f.write_str("transient"),
            Self::Permanent => f.write_str("permanent"),
        }
    }
}

/// Errors that can occur during tool execution.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("command blocked by policy: {command}")]
    Blocked { command: String },

    #[error("path not allowed by sandbox: {path}")]
    SandboxViolation { path: String },

    #[error("command requires confirmation: {command}")]
    ConfirmationRequired { command: String },

    #[error("command timed out after {timeout_secs}s")]
    Timeout { timeout_secs: u64 },

    #[error("operation cancelled")]
    Cancelled,

    #[error("invalid tool parameters: {message}")]
    InvalidParams { message: String },

    #[error("execution failed: {0}")]
    Execution(#[from] std::io::Error),
}

impl ToolError {
    /// Classify this error as transient (retryable) or permanent.
    ///
    /// For `Execution(io::Error)`, the classification inspects `io::Error::kind()`:
    /// - Transient: `TimedOut`, `WouldBlock`, `Interrupted`, `ConnectionReset`,
    ///   `ConnectionAborted`, `BrokenPipe` — these may succeed on retry.
    /// - Permanent: `NotFound`, `PermissionDenied`, `AlreadyExists`, and all other
    ///   I/O error kinds — retrying would waste time with no benefit.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Timeout { .. } => ErrorKind::Transient,
            Self::Execution(io_err) => match io_err.kind() {
                std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::WouldBlock
                | std::io::ErrorKind::Interrupted
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe => ErrorKind::Transient,
                // NotFound, PermissionDenied, AlreadyExists, and everything else: permanent.
                _ => ErrorKind::Permanent,
            },
            Self::Blocked { .. }
            | Self::SandboxViolation { .. }
            | Self::ConfirmationRequired { .. }
            | Self::Cancelled
            | Self::InvalidParams { .. } => ErrorKind::Permanent,
        }
    }
}

/// Deserialize tool call params from a `serde_json::Map<String, Value>` into a typed struct.
///
/// # Errors
///
/// Returns `ToolError::InvalidParams` when deserialization fails.
pub fn deserialize_params<T: serde::de::DeserializeOwned>(
    params: &serde_json::Map<String, serde_json::Value>,
) -> Result<T, ToolError> {
    let obj = serde_json::Value::Object(params.clone());
    serde_json::from_value(obj).map_err(|e| ToolError::InvalidParams {
        message: e.to_string(),
    })
}

/// Async trait for tool execution backends (shell, future MCP, A2A).
///
/// Accepts the full LLM response and returns an optional output.
/// Returns `None` when no tool invocation is detected in the response.
pub trait ToolExecutor: Send + Sync {
    fn execute(
        &self,
        response: &str,
    ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send;

    /// Execute bypassing confirmation checks (called after user approves).
    /// Default: delegates to `execute`.
    fn execute_confirmed(
        &self,
        response: &str,
    ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        self.execute(response)
    }

    /// Return tool definitions this executor can handle.
    fn tool_definitions(&self) -> Vec<crate::registry::ToolDef> {
        vec![]
    }

    /// Execute a structured tool call. Returns `None` if `tool_id` is not handled.
    fn execute_tool_call(
        &self,
        _call: &ToolCall,
    ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        std::future::ready(Ok(None))
    }

    /// Inject environment variables for the currently active skill. No-op by default.
    fn set_skill_env(&self, _env: Option<std::collections::HashMap<String, String>>) {}

    /// Set the effective trust level for the currently active skill. No-op by default.
    fn set_effective_trust(&self, _level: crate::TrustLevel) {}
}

/// Object-safe erased version of [`ToolExecutor`] using boxed futures.
///
/// Implemented automatically for all `T: ToolExecutor + 'static`.
/// Use `Box<dyn ErasedToolExecutor>` when dynamic dispatch is required.
pub trait ErasedToolExecutor: Send + Sync {
    fn execute_erased<'a>(
        &'a self,
        response: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>>;

    fn execute_confirmed_erased<'a>(
        &'a self,
        response: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>>;

    fn tool_definitions_erased(&self) -> Vec<crate::registry::ToolDef>;

    fn execute_tool_call_erased<'a>(
        &'a self,
        call: &'a ToolCall,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>>;

    /// Inject environment variables for the currently active skill. No-op by default.
    fn set_skill_env(&self, _env: Option<std::collections::HashMap<String, String>>) {}

    /// Set the effective trust level for the currently active skill. No-op by default.
    fn set_effective_trust(&self, _level: crate::TrustLevel) {}
}

impl<T: ToolExecutor> ErasedToolExecutor for T {
    fn execute_erased<'a>(
        &'a self,
        response: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>>
    {
        Box::pin(self.execute(response))
    }

    fn execute_confirmed_erased<'a>(
        &'a self,
        response: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>>
    {
        Box::pin(self.execute_confirmed(response))
    }

    fn tool_definitions_erased(&self) -> Vec<crate::registry::ToolDef> {
        self.tool_definitions()
    }

    fn execute_tool_call_erased<'a>(
        &'a self,
        call: &'a ToolCall,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>>
    {
        Box::pin(self.execute_tool_call(call))
    }

    fn set_skill_env(&self, env: Option<std::collections::HashMap<String, String>>) {
        ToolExecutor::set_skill_env(self, env);
    }

    fn set_effective_trust(&self, level: crate::TrustLevel) {
        ToolExecutor::set_effective_trust(self, level);
    }
}

/// Wraps `Arc<dyn ErasedToolExecutor>` so it can be used as a concrete `ToolExecutor`.
///
/// Enables dynamic composition of tool executors at runtime without static type chains.
pub struct DynExecutor(pub std::sync::Arc<dyn ErasedToolExecutor>);

impl ToolExecutor for DynExecutor {
    fn execute(
        &self,
        response: &str,
    ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        // Clone data to satisfy the 'static-ish bound: erased futures must not borrow self.
        let inner = std::sync::Arc::clone(&self.0);
        let response = response.to_owned();
        async move { inner.execute_erased(&response).await }
    }

    fn execute_confirmed(
        &self,
        response: &str,
    ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        let inner = std::sync::Arc::clone(&self.0);
        let response = response.to_owned();
        async move { inner.execute_confirmed_erased(&response).await }
    }

    fn tool_definitions(&self) -> Vec<crate::registry::ToolDef> {
        self.0.tool_definitions_erased()
    }

    fn execute_tool_call(
        &self,
        call: &ToolCall,
    ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        let inner = std::sync::Arc::clone(&self.0);
        let call = call.clone();
        async move { inner.execute_tool_call_erased(&call).await }
    }

    fn set_skill_env(&self, env: Option<std::collections::HashMap<String, String>>) {
        ErasedToolExecutor::set_skill_env(self.0.as_ref(), env);
    }

    fn set_effective_trust(&self, level: crate::TrustLevel) {
        ErasedToolExecutor::set_effective_trust(self.0.as_ref(), level);
    }
}

/// Extract fenced code blocks with the given language marker from text.
///
/// Searches for `` ```{lang} `` … `` ``` `` pairs, returning trimmed content.
#[must_use]
pub fn extract_fenced_blocks<'a>(text: &'a str, lang: &str) -> Vec<&'a str> {
    let marker = format!("```{lang}");
    let marker_len = marker.len();
    let mut blocks = Vec::new();
    let mut rest = text;

    let mut search_from = 0;
    while let Some(rel) = rest[search_from..].find(&marker) {
        let start = search_from + rel;
        let after = &rest[start + marker_len..];
        // Word-boundary check: the character immediately after the marker must be
        // whitespace, end-of-string, or a non-word character (not alphanumeric / _ / -).
        // This prevents "```bash" from matching "```bashrc".
        let boundary_ok = after
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_' && c != '-');
        if !boundary_ok {
            search_from = start + marker_len;
            continue;
        }
        if let Some(end) = after.find("```") {
            blocks.push(after[..end].trim());
            rest = &after[end + 3..];
            search_from = 0;
        } else {
            break;
        }
    }

    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_output_display() {
        let output = ToolOutput {
            tool_name: "bash".to_owned(),
            summary: "$ echo hello\nhello".to_owned(),
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
        };
        assert_eq!(output.to_string(), "$ echo hello\nhello");
    }

    #[test]
    fn tool_error_blocked_display() {
        let err = ToolError::Blocked {
            command: "rm -rf /".to_owned(),
        };
        assert_eq!(err.to_string(), "command blocked by policy: rm -rf /");
    }

    #[test]
    fn tool_error_sandbox_violation_display() {
        let err = ToolError::SandboxViolation {
            path: "/etc/shadow".to_owned(),
        };
        assert_eq!(err.to_string(), "path not allowed by sandbox: /etc/shadow");
    }

    #[test]
    fn tool_error_confirmation_required_display() {
        let err = ToolError::ConfirmationRequired {
            command: "rm -rf /tmp".to_owned(),
        };
        assert_eq!(
            err.to_string(),
            "command requires confirmation: rm -rf /tmp"
        );
    }

    #[test]
    fn tool_error_timeout_display() {
        let err = ToolError::Timeout { timeout_secs: 30 };
        assert_eq!(err.to_string(), "command timed out after 30s");
    }

    #[test]
    fn tool_error_invalid_params_display() {
        let err = ToolError::InvalidParams {
            message: "missing field `command`".to_owned(),
        };
        assert_eq!(
            err.to_string(),
            "invalid tool parameters: missing field `command`"
        );
    }

    #[test]
    fn deserialize_params_valid() {
        #[derive(Debug, serde::Deserialize, PartialEq)]
        struct P {
            name: String,
            count: u32,
        }
        let mut map = serde_json::Map::new();
        map.insert("name".to_owned(), serde_json::json!("test"));
        map.insert("count".to_owned(), serde_json::json!(42));
        let p: P = deserialize_params(&map).unwrap();
        assert_eq!(
            p,
            P {
                name: "test".to_owned(),
                count: 42
            }
        );
    }

    #[test]
    fn deserialize_params_missing_required_field() {
        #[derive(Debug, serde::Deserialize)]
        #[allow(dead_code)]
        struct P {
            name: String,
        }
        let map = serde_json::Map::new();
        let err = deserialize_params::<P>(&map).unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams { .. }));
    }

    #[test]
    fn deserialize_params_wrong_type() {
        #[derive(Debug, serde::Deserialize)]
        #[allow(dead_code)]
        struct P {
            count: u32,
        }
        let mut map = serde_json::Map::new();
        map.insert("count".to_owned(), serde_json::json!("not a number"));
        let err = deserialize_params::<P>(&map).unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams { .. }));
    }

    #[test]
    fn deserialize_params_all_optional_empty() {
        #[derive(Debug, serde::Deserialize, PartialEq)]
        struct P {
            name: Option<String>,
        }
        let map = serde_json::Map::new();
        let p: P = deserialize_params(&map).unwrap();
        assert_eq!(p, P { name: None });
    }

    #[test]
    fn deserialize_params_ignores_extra_fields() {
        #[derive(Debug, serde::Deserialize, PartialEq)]
        struct P {
            name: String,
        }
        let mut map = serde_json::Map::new();
        map.insert("name".to_owned(), serde_json::json!("test"));
        map.insert("extra".to_owned(), serde_json::json!(true));
        let p: P = deserialize_params(&map).unwrap();
        assert_eq!(
            p,
            P {
                name: "test".to_owned()
            }
        );
    }

    #[test]
    fn tool_error_execution_display() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "bash not found");
        let err = ToolError::Execution(io_err);
        assert!(err.to_string().starts_with("execution failed:"));
        assert!(err.to_string().contains("bash not found"));
    }

    // ErrorKind classification tests
    #[test]
    fn error_kind_timeout_is_transient() {
        let err = ToolError::Timeout { timeout_secs: 30 };
        assert_eq!(err.kind(), ErrorKind::Transient);
    }

    #[test]
    fn error_kind_blocked_is_permanent() {
        let err = ToolError::Blocked {
            command: "rm -rf /".to_owned(),
        };
        assert_eq!(err.kind(), ErrorKind::Permanent);
    }

    #[test]
    fn error_kind_sandbox_violation_is_permanent() {
        let err = ToolError::SandboxViolation {
            path: "/etc/shadow".to_owned(),
        };
        assert_eq!(err.kind(), ErrorKind::Permanent);
    }

    #[test]
    fn error_kind_cancelled_is_permanent() {
        assert_eq!(ToolError::Cancelled.kind(), ErrorKind::Permanent);
    }

    #[test]
    fn error_kind_invalid_params_is_permanent() {
        let err = ToolError::InvalidParams {
            message: "bad arg".to_owned(),
        };
        assert_eq!(err.kind(), ErrorKind::Permanent);
    }

    #[test]
    fn error_kind_confirmation_required_is_permanent() {
        let err = ToolError::ConfirmationRequired {
            command: "rm /tmp/x".to_owned(),
        };
        assert_eq!(err.kind(), ErrorKind::Permanent);
    }

    #[test]
    fn error_kind_execution_timed_out_is_transient() {
        let io_err = std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout");
        assert_eq!(ToolError::Execution(io_err).kind(), ErrorKind::Transient);
    }

    #[test]
    fn error_kind_execution_interrupted_is_transient() {
        let io_err = std::io::Error::new(std::io::ErrorKind::Interrupted, "interrupted");
        assert_eq!(ToolError::Execution(io_err).kind(), ErrorKind::Transient);
    }

    #[test]
    fn error_kind_execution_connection_reset_is_transient() {
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        assert_eq!(ToolError::Execution(io_err).kind(), ErrorKind::Transient);
    }

    #[test]
    fn error_kind_execution_broken_pipe_is_transient() {
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe broken");
        assert_eq!(ToolError::Execution(io_err).kind(), ErrorKind::Transient);
    }

    #[test]
    fn error_kind_execution_would_block_is_transient() {
        let io_err = std::io::Error::new(std::io::ErrorKind::WouldBlock, "would block");
        assert_eq!(ToolError::Execution(io_err).kind(), ErrorKind::Transient);
    }

    #[test]
    fn error_kind_execution_connection_aborted_is_transient() {
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "aborted");
        assert_eq!(ToolError::Execution(io_err).kind(), ErrorKind::Transient);
    }

    #[test]
    fn error_kind_execution_not_found_is_permanent() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "not found");
        assert_eq!(ToolError::Execution(io_err).kind(), ErrorKind::Permanent);
    }

    #[test]
    fn error_kind_execution_permission_denied_is_permanent() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        assert_eq!(ToolError::Execution(io_err).kind(), ErrorKind::Permanent);
    }

    #[test]
    fn error_kind_execution_other_is_permanent() {
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "some other error");
        assert_eq!(ToolError::Execution(io_err).kind(), ErrorKind::Permanent);
    }

    #[test]
    fn error_kind_execution_already_exists_is_permanent() {
        let io_err = std::io::Error::new(std::io::ErrorKind::AlreadyExists, "exists");
        assert_eq!(ToolError::Execution(io_err).kind(), ErrorKind::Permanent);
    }

    #[test]
    fn error_kind_display() {
        assert_eq!(ErrorKind::Transient.to_string(), "transient");
        assert_eq!(ErrorKind::Permanent.to_string(), "permanent");
    }

    #[test]
    fn truncate_tool_output_short_passthrough() {
        let short = "hello world";
        assert_eq!(truncate_tool_output(short), short);
    }

    #[test]
    fn truncate_tool_output_exact_limit() {
        let exact = "a".repeat(MAX_TOOL_OUTPUT_CHARS);
        assert_eq!(truncate_tool_output(&exact), exact);
    }

    #[test]
    fn truncate_tool_output_long_split() {
        let long = "x".repeat(MAX_TOOL_OUTPUT_CHARS + 1000);
        let result = truncate_tool_output(&long);
        assert!(result.contains("truncated"));
        assert!(result.len() < long.len());
    }

    #[test]
    fn truncate_tool_output_notice_contains_count() {
        let long = "y".repeat(MAX_TOOL_OUTPUT_CHARS + 2000);
        let result = truncate_tool_output(&long);
        assert!(result.contains("truncated"));
        assert!(result.contains("chars"));
    }

    #[derive(Debug)]
    struct DefaultExecutor;
    impl ToolExecutor for DefaultExecutor {
        async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn execute_tool_call_default_returns_none() {
        let exec = DefaultExecutor;
        let call = ToolCall {
            tool_id: "anything".to_owned(),
            params: serde_json::Map::new(),
        };
        let result = exec.execute_tool_call(&call).await.unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn filter_stats_savings_pct() {
        let fs = FilterStats {
            raw_chars: 1000,
            filtered_chars: 200,
            ..Default::default()
        };
        assert!((fs.savings_pct() - 80.0).abs() < 0.01);
    }

    #[test]
    fn filter_stats_savings_pct_zero() {
        let fs = FilterStats::default();
        assert!((fs.savings_pct()).abs() < 0.01);
    }

    #[test]
    fn filter_stats_estimated_tokens_saved() {
        let fs = FilterStats {
            raw_chars: 1000,
            filtered_chars: 200,
            ..Default::default()
        };
        assert_eq!(fs.estimated_tokens_saved(), 200); // (1000 - 200) / 4
    }

    #[test]
    fn filter_stats_format_inline() {
        let fs = FilterStats {
            raw_chars: 1000,
            filtered_chars: 200,
            raw_lines: 342,
            filtered_lines: 28,
            ..Default::default()
        };
        let line = fs.format_inline("shell");
        assert_eq!(line, "[shell] 342 lines \u{2192} 28 lines, 80.0% filtered");
    }

    #[test]
    fn filter_stats_format_inline_zero() {
        let fs = FilterStats::default();
        let line = fs.format_inline("bash");
        assert_eq!(line, "[bash] 0 lines \u{2192} 0 lines, 0.0% filtered");
    }

    // DynExecutor tests

    struct FixedExecutor {
        tool_id: &'static str,
        output: &'static str,
    }

    impl ToolExecutor for FixedExecutor {
        async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(Some(ToolOutput {
                tool_name: self.tool_id.to_owned(),
                summary: self.output.to_owned(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
            }))
        }

        fn tool_definitions(&self) -> Vec<crate::registry::ToolDef> {
            vec![]
        }

        async fn execute_tool_call(
            &self,
            _call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            Ok(Some(ToolOutput {
                tool_name: self.tool_id.to_owned(),
                summary: self.output.to_owned(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
            }))
        }
    }

    #[tokio::test]
    async fn dyn_executor_execute_delegates() {
        let inner = std::sync::Arc::new(FixedExecutor {
            tool_id: "bash",
            output: "hello",
        });
        let exec = DynExecutor(inner);
        let result = exec.execute("```bash\necho hello\n```").await.unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().summary, "hello");
    }

    #[tokio::test]
    async fn dyn_executor_execute_confirmed_delegates() {
        let inner = std::sync::Arc::new(FixedExecutor {
            tool_id: "bash",
            output: "confirmed",
        });
        let exec = DynExecutor(inner);
        let result = exec.execute_confirmed("...").await.unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().summary, "confirmed");
    }

    #[test]
    fn dyn_executor_tool_definitions_delegates() {
        let inner = std::sync::Arc::new(FixedExecutor {
            tool_id: "my_tool",
            output: "",
        });
        let exec = DynExecutor(inner);
        // FixedExecutor returns empty definitions; verify delegation occurs without panic.
        let defs = exec.tool_definitions();
        assert!(defs.is_empty());
    }

    #[tokio::test]
    async fn dyn_executor_execute_tool_call_delegates() {
        let inner = std::sync::Arc::new(FixedExecutor {
            tool_id: "bash",
            output: "tool_call_result",
        });
        let exec = DynExecutor(inner);
        let call = ToolCall {
            tool_id: "bash".to_owned(),
            params: serde_json::Map::new(),
        };
        let result = exec.execute_tool_call(&call).await.unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().summary, "tool_call_result");
    }

    #[test]
    fn dyn_executor_set_effective_trust_delegates() {
        use std::sync::atomic::{AtomicU8, Ordering};

        struct TrustCapture(AtomicU8);
        impl ToolExecutor for TrustCapture {
            async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
                Ok(None)
            }
            fn set_effective_trust(&self, level: crate::TrustLevel) {
                // encode: Trusted=0, Verified=1, Quarantined=2, Blocked=3
                let v = match level {
                    crate::TrustLevel::Trusted => 0u8,
                    crate::TrustLevel::Verified => 1,
                    crate::TrustLevel::Quarantined => 2,
                    crate::TrustLevel::Blocked => 3,
                };
                self.0.store(v, Ordering::Relaxed);
            }
        }

        let inner = std::sync::Arc::new(TrustCapture(AtomicU8::new(0)));
        let exec =
            DynExecutor(std::sync::Arc::clone(&inner) as std::sync::Arc<dyn ErasedToolExecutor>);
        ToolExecutor::set_effective_trust(&exec, crate::TrustLevel::Quarantined);
        assert_eq!(inner.0.load(Ordering::Relaxed), 2);

        ToolExecutor::set_effective_trust(&exec, crate::TrustLevel::Blocked);
        assert_eq!(inner.0.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn extract_fenced_blocks_no_prefix_match() {
        // ```bashrc must NOT match when searching for "bash"
        assert!(extract_fenced_blocks("```bashrc\nfoo\n```", "bash").is_empty());
        // exact match
        assert_eq!(
            extract_fenced_blocks("```bash\nfoo\n```", "bash"),
            vec!["foo"]
        );
        // trailing space is fine
        assert_eq!(
            extract_fenced_blocks("```bash \nfoo\n```", "bash"),
            vec!["foo"]
        );
    }
}
