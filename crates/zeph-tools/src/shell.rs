use std::time::Duration;

use tokio::process::Command;

use crate::config::ShellConfig;
use crate::executor::{ToolError, ToolExecutor, ToolOutput};

/// Bash block extraction and execution via `tokio::process::Command`.
#[derive(Debug)]
pub struct ShellExecutor {
    timeout: Duration,
    blocked_commands: Vec<String>,
}

impl ShellExecutor {
    #[must_use]
    pub fn new(config: &ShellConfig) -> Self {
        Self {
            timeout: Duration::from_secs(config.timeout),
            blocked_commands: config.blocked_commands.clone(),
        }
    }
}

impl ToolExecutor for ShellExecutor {
    async fn execute(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        let blocks = extract_bash_blocks(response);
        if blocks.is_empty() {
            return Ok(None);
        }

        let mut outputs = Vec::with_capacity(blocks.len());
        #[allow(clippy::cast_possible_truncation)]
        let blocks_executed = blocks.len() as u32;

        for block in &blocks {
            if let Some(blocked) = self.find_blocked_command(block) {
                return Err(ToolError::Blocked {
                    command: blocked.to_owned(),
                });
            }

            let out = execute_bash(block, self.timeout).await;
            outputs.push(format!("$ {block}\n{out}"));
        }

        Ok(Some(ToolOutput {
            summary: outputs.join("\n\n"),
            blocks_executed,
        }))
    }
}

impl ShellExecutor {
    fn find_blocked_command(&self, code: &str) -> Option<&str> {
        for blocked in &self.blocked_commands {
            if code.contains(blocked.as_str()) {
                return Some(blocked.as_str());
            }
        }
        None
    }
}

fn extract_bash_blocks(text: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut rest = text;

    while let Some(start) = rest.find("```bash") {
        let code_start = start + 7;
        let after = &rest[code_start..];
        if let Some(end) = after.find("```") {
            blocks.push(after[..end].trim());
            rest = &after[end + 3..];
        } else {
            break;
        }
    }

    blocks
}

async fn execute_bash(code: &str, timeout: Duration) -> String {
    let timeout_secs = timeout.as_secs();
    let result =
        tokio::time::timeout(timeout, Command::new("bash").arg("-c").arg(code).output()).await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let mut combined = String::new();
            if !stdout.is_empty() {
                combined.push_str(&stdout);
            }
            if !stderr.is_empty() {
                if !combined.is_empty() {
                    combined.push('\n');
                }
                combined.push_str("[stderr] ");
                combined.push_str(&stderr);
            }
            if combined.is_empty() {
                combined.push_str("(no output)");
            }
            combined
        }
        Ok(Err(e)) => format!("[error] {e}"),
        Err(_) => format!("[error] command timed out after {timeout_secs}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_single_bash_block() {
        let text = "Here is code:\n```bash\necho hello\n```\nDone.";
        let blocks = extract_bash_blocks(text);
        assert_eq!(blocks, vec!["echo hello"]);
    }

    #[test]
    fn extract_multiple_bash_blocks() {
        let text = "```bash\nls\n```\ntext\n```bash\npwd\n```";
        let blocks = extract_bash_blocks(text);
        assert_eq!(blocks, vec!["ls", "pwd"]);
    }

    #[test]
    fn ignore_non_bash_blocks() {
        let text = "```python\nprint('hi')\n```\n```bash\necho hi\n```";
        let blocks = extract_bash_blocks(text);
        assert_eq!(blocks, vec!["echo hi"]);
    }

    #[test]
    fn no_blocks_returns_none() {
        let text = "Just plain text, no code blocks.";
        let blocks = extract_bash_blocks(text);
        assert!(blocks.is_empty());
    }

    #[test]
    fn unclosed_block_ignored() {
        let text = "```bash\necho hello";
        let blocks = extract_bash_blocks(text);
        assert!(blocks.is_empty());
    }

    #[tokio::test]
    async fn execute_simple_command() {
        let result = execute_bash("echo hello", Duration::from_secs(30)).await;
        assert!(result.contains("hello"));
    }

    #[tokio::test]
    async fn execute_stderr_output() {
        let result = execute_bash("echo err >&2", Duration::from_secs(30)).await;
        assert!(result.contains("[stderr]"));
        assert!(result.contains("err"));
    }

    #[tokio::test]
    async fn execute_stdout_and_stderr_combined() {
        let result = execute_bash("echo out && echo err >&2", Duration::from_secs(30)).await;
        assert!(result.contains("out"));
        assert!(result.contains("[stderr]"));
        assert!(result.contains("err"));
        assert!(result.contains('\n'));
    }

    #[tokio::test]
    async fn execute_empty_output() {
        let result = execute_bash("true", Duration::from_secs(30)).await;
        assert_eq!(result, "(no output)");
    }

    #[tokio::test]
    async fn blocked_command_rejected() {
        let config = ShellConfig {
            timeout: 30,
            blocked_commands: vec!["rm -rf /".to_owned()],
        };
        let executor = ShellExecutor::new(&config);
        let response = "Run:\n```bash\nrm -rf /\n```";
        let result = executor.execute(response).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn timeout_enforced() {
        let config = ShellConfig {
            timeout: 1,
            blocked_commands: Vec::new(),
        };
        let executor = ShellExecutor::new(&config);
        let response = "Run:\n```bash\nsleep 60\n```";
        let result = executor.execute(response).await;
        assert!(result.is_ok());
        let output = result.unwrap().unwrap();
        assert!(output.summary.contains("timed out"));
    }

    #[tokio::test]
    async fn execute_no_blocks_returns_none() {
        let config = ShellConfig {
            timeout: 30,
            blocked_commands: Vec::new(),
        };
        let executor = ShellExecutor::new(&config);
        let result = executor.execute("plain text, no blocks").await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn execute_multiple_blocks_counted() {
        let config = ShellConfig {
            timeout: 30,
            blocked_commands: Vec::new(),
        };
        let executor = ShellExecutor::new(&config);
        let response = "```bash\necho one\n```\n```bash\necho two\n```";
        let result = executor.execute(response).await;
        let output = result.unwrap().unwrap();
        assert_eq!(output.blocks_executed, 2);
        assert!(output.summary.contains("one"));
        assert!(output.summary.contains("two"));
    }
}
