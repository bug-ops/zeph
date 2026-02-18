use std::collections::HashMap;
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
    pub params: HashMap<String, serde_json::Value>,
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
    if output.len() <= MAX_TOOL_OUTPUT_CHARS {
        return output.to_string();
    }

    let half = MAX_TOOL_OUTPUT_CHARS / 2;
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

/// Deserialize tool call params from a `HashMap<String, Value>` into a typed struct.
///
/// # Errors
///
/// Returns `ToolError::InvalidParams` when deserialization fails.
pub fn deserialize_params<T: serde::de::DeserializeOwned, S: std::hash::BuildHasher>(
    params: &HashMap<String, serde_json::Value, S>,
) -> Result<T, ToolError> {
    let obj =
        serde_json::Value::Object(params.iter().map(|(k, v)| (k.clone(), v.clone())).collect());
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

    while let Some(start) = rest.find(&marker) {
        let after = &rest[start + marker_len..];
        if let Some(end) = after.find("```") {
            blocks.push(after[..end].trim());
            rest = &after[end + 3..];
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
        let mut map = HashMap::new();
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
        struct P {
            #[allow(dead_code)]
            name: String,
        }
        let map: HashMap<String, serde_json::Value> = HashMap::new();
        let err = deserialize_params::<P, _>(&map).unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams { .. }));
    }

    #[test]
    fn deserialize_params_wrong_type() {
        #[derive(Debug, serde::Deserialize)]
        struct P {
            #[allow(dead_code)]
            count: u32,
        }
        let mut map = HashMap::new();
        map.insert("count".to_owned(), serde_json::json!("not a number"));
        let err = deserialize_params::<P, _>(&map).unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams { .. }));
    }

    #[test]
    fn deserialize_params_all_optional_empty() {
        #[derive(Debug, serde::Deserialize, PartialEq)]
        struct P {
            name: Option<String>,
        }
        let map: HashMap<String, serde_json::Value> = HashMap::new();
        let p: P = deserialize_params(&map).unwrap();
        assert_eq!(p, P { name: None });
    }

    #[test]
    fn deserialize_params_ignores_extra_fields() {
        #[derive(Debug, serde::Deserialize, PartialEq)]
        struct P {
            name: String,
        }
        let mut map = HashMap::new();
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
            params: std::collections::HashMap::new(),
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
}
