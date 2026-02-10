use std::fmt;

/// Structured result from tool execution.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub summary: String,
    pub blocks_executed: u32,
}

impl fmt::Display for ToolOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.summary)
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

    #[error("execution failed: {0}")]
    Execution(#[from] std::io::Error),
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
            summary: "$ echo hello\nhello".to_owned(),
            blocks_executed: 1,
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
    fn tool_error_execution_display() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "bash not found");
        let err = ToolError::Execution(io_err);
        assert!(err.to_string().starts_with("execution failed:"));
        assert!(err.to_string().contains("bash not found"));
    }
}
