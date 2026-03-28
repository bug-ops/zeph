// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use schemars::JsonSchema;
use serde::Deserialize;

use std::sync::Arc;

use crate::audit::{AuditEntry, AuditLogger, AuditResult, chrono_now};
use crate::config::ShellConfig;
use crate::executor::{
    ClaimSource, FilterStats, ToolCall, ToolError, ToolEvent, ToolEventTx, ToolExecutor, ToolOutput,
};
use crate::filter::{OutputFilterRegistry, sanitize_output};
use crate::permissions::{PermissionAction, PermissionPolicy};

const DEFAULT_BLOCKED: &[&str] = &[
    "rm -rf /", "sudo", "mkfs", "dd if=", "curl", "wget", "nc ", "ncat", "netcat", "shutdown",
    "reboot", "halt",
];

/// The default list of blocked command patterns used by [`ShellExecutor`].
///
/// Exposed so other executors (e.g. `AcpShellExecutor`) can reuse the same
/// blocklist without duplicating it.
pub const DEFAULT_BLOCKED_COMMANDS: &[&str] = DEFAULT_BLOCKED;

/// Shell interpreters that may execute arbitrary code via `-c` or positional args.
pub const SHELL_INTERPRETERS: &[&str] =
    &["bash", "sh", "zsh", "fish", "dash", "ksh", "csh", "tcsh"];

/// Subshell metacharacters that could embed a blocked command inside a benign wrapper.
/// Commands containing these sequences are rejected outright because safe static
/// analysis of nested shell evaluation is not feasible.
const SUBSHELL_METACHARS: &[&str] = &["$(", "`", "<(", ">("];

/// Check if `command` matches any pattern in `blocklist`.
///
/// Returns the matched pattern string if the command is blocked, `None` otherwise.
/// The check is case-insensitive and handles common shell escape sequences.
///
/// Commands containing subshell metacharacters (`$(` or `` ` ``) are always
/// blocked because nested evaluation cannot be safely analysed statically.
#[must_use]
pub fn check_blocklist(command: &str, blocklist: &[String]) -> Option<String> {
    let lower = command.to_lowercase();
    // Reject commands that embed subshell constructs to prevent blocklist bypass.
    for meta in SUBSHELL_METACHARS {
        if lower.contains(meta) {
            return Some((*meta).to_owned());
        }
    }
    let cleaned = strip_shell_escapes(&lower);
    let commands = tokenize_commands(&cleaned);
    for blocked in blocklist {
        for cmd_tokens in &commands {
            if tokens_match_pattern(cmd_tokens, blocked) {
                return Some(blocked.clone());
            }
        }
    }
    None
}

/// Build the effective command string for blocklist evaluation when the binary is a
/// shell interpreter (bash, sh, zsh, etc.) and args contains a `-c` script.
///
/// Returns `None` if the args do not follow the `-c <script>` pattern.
#[must_use]
pub fn effective_shell_command<'a>(binary: &str, args: &'a [String]) -> Option<&'a str> {
    let base = binary.rsplit('/').next().unwrap_or(binary);
    if !SHELL_INTERPRETERS.contains(&base) {
        return None;
    }
    // Find "-c" and return the next element as the script to check.
    let pos = args.iter().position(|a| a == "-c")?;
    args.get(pos + 1).map(String::as_str)
}

const NETWORK_COMMANDS: &[&str] = &["curl", "wget", "nc ", "ncat", "netcat"];

#[derive(Deserialize, JsonSchema)]
pub(crate) struct BashParams {
    /// The bash command to execute
    command: String,
}

/// Bash block extraction and execution via `tokio::process::Command`.
#[derive(Debug)]
pub struct ShellExecutor {
    timeout: Duration,
    blocked_commands: Vec<String>,
    allowed_paths: Vec<PathBuf>,
    confirm_patterns: Vec<String>,
    audit_logger: Option<Arc<AuditLogger>>,
    tool_event_tx: Option<ToolEventTx>,
    permission_policy: Option<PermissionPolicy>,
    output_filter_registry: Option<OutputFilterRegistry>,
    cancel_token: Option<CancellationToken>,
    skill_env: std::sync::RwLock<Option<std::collections::HashMap<String, String>>>,
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
            output_filter_registry: None,
            cancel_token: None,
            skill_env: std::sync::RwLock::new(None),
        }
    }

    /// Set environment variables to inject when executing the active skill's bash blocks.
    pub fn set_skill_env(&self, env: Option<std::collections::HashMap<String, String>>) {
        match self.skill_env.write() {
            Ok(mut guard) => *guard = env,
            Err(e) => tracing::error!("skill_env RwLock poisoned: {e}"),
        }
    }

    #[must_use]
    pub fn with_audit(mut self, logger: Arc<AuditLogger>) -> Self {
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

    #[must_use]
    pub fn with_cancel_token(mut self, token: CancellationToken) -> Self {
        self.cancel_token = Some(token);
        self
    }

    #[must_use]
    pub fn with_output_filters(mut self, registry: OutputFilterRegistry) -> Self {
        self.output_filter_registry = Some(registry);
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
        let mut cumulative_filter_stats: Option<FilterStats> = None;
        #[allow(clippy::cast_possible_truncation)]
        let blocks_executed = blocks.len() as u32;

        for block in &blocks {
            let (output_line, per_block_stats) = self.execute_block(block, skip_confirm).await?;
            if let Some(fs) = per_block_stats {
                let stats = cumulative_filter_stats.get_or_insert_with(FilterStats::default);
                stats.raw_chars += fs.raw_chars;
                stats.filtered_chars += fs.filtered_chars;
                stats.raw_lines += fs.raw_lines;
                stats.filtered_lines += fs.filtered_lines;
                stats.confidence = Some(match (stats.confidence, fs.confidence) {
                    (Some(prev), Some(cur)) => crate::filter::worse_confidence(prev, cur),
                    (Some(prev), None) => prev,
                    (None, Some(cur)) => cur,
                    (None, None) => unreachable!(),
                });
                if stats.command.is_none() {
                    stats.command = fs.command;
                }
                if stats.kept_lines.is_empty() && !fs.kept_lines.is_empty() {
                    stats.kept_lines = fs.kept_lines;
                }
            }
            outputs.push(output_line);
        }

        Ok(Some(ToolOutput {
            tool_name: "bash".to_owned(),
            summary: outputs.join("\n\n"),
            blocks_executed,
            filter_stats: cumulative_filter_stats,
            diff: None,
            streamed: self.tool_event_tx.is_some(),
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: Some(ClaimSource::Shell),
        }))
    }

    async fn execute_block(
        &self,
        block: &str,
        skip_confirm: bool,
    ) -> Result<(String, Option<FilterStats>), ToolError> {
        self.check_permissions(block, skip_confirm).await?;
        self.validate_sandbox(block)?;

        if let Some(ref tx) = self.tool_event_tx {
            let _ = tx.send(ToolEvent::Started {
                tool_name: "bash".to_owned(),
                command: block.to_owned(),
            });
        }

        let start = Instant::now();
        let skill_env_snapshot: Option<std::collections::HashMap<String, String>> =
            self.skill_env.read().ok().and_then(|g| g.clone());
        let (out, exit_code) = execute_bash(
            block,
            self.timeout,
            self.tool_event_tx.as_ref(),
            self.cancel_token.as_ref(),
            skill_env_snapshot.as_ref(),
        )
        .await;
        if exit_code == 130
            && self
                .cancel_token
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(ToolError::Cancelled);
        }
        #[allow(clippy::cast_possible_truncation)]
        let duration_ms = start.elapsed().as_millis() as u64;

        let is_timeout = out.contains("[error] command timed out");
        let audit_result = if is_timeout {
            AuditResult::Timeout
        } else if out.contains("[error]") || out.contains("[stderr]") {
            AuditResult::Error {
                message: out.clone(),
            }
        } else {
            AuditResult::Success
        };
        self.log_audit(block, audit_result, duration_ms, None).await;

        if is_timeout {
            self.emit_completed(block, &out, false, None);
            return Err(ToolError::Timeout {
                timeout_secs: self.timeout.as_secs(),
            });
        }

        if let Some(category) = classify_shell_exit(exit_code, &out) {
            self.emit_completed(block, &out, false, None);
            return Err(ToolError::Shell {
                exit_code,
                category,
                message: out.lines().take(3).collect::<Vec<_>>().join("; "),
            });
        }

        let sanitized = sanitize_output(&out);
        let mut per_block_stats: Option<FilterStats> = None;
        let filtered = if let Some(ref registry) = self.output_filter_registry {
            match registry.apply(block, &sanitized, exit_code) {
                Some(fr) => {
                    tracing::debug!(
                        command = block,
                        raw = fr.raw_chars,
                        filtered = fr.filtered_chars,
                        savings_pct = fr.savings_pct(),
                        "output filter applied"
                    );
                    per_block_stats = Some(FilterStats {
                        raw_chars: fr.raw_chars,
                        filtered_chars: fr.filtered_chars,
                        raw_lines: fr.raw_lines,
                        filtered_lines: fr.filtered_lines,
                        confidence: Some(fr.confidence),
                        command: Some(block.to_owned()),
                        kept_lines: fr.kept_lines.clone(),
                    });
                    fr.output
                }
                None => sanitized,
            }
        } else {
            sanitized
        };

        self.emit_completed(
            block,
            &out,
            !out.contains("[error]"),
            per_block_stats.clone(),
        );

        Ok((format!("$ {block}\n{filtered}"), per_block_stats))
    }

    fn emit_completed(
        &self,
        command: &str,
        output: &str,
        success: bool,
        filter_stats: Option<FilterStats>,
    ) {
        if let Some(ref tx) = self.tool_event_tx {
            let _ = tx.send(ToolEvent::Completed {
                tool_name: "bash".to_owned(),
                command: command.to_owned(),
                output: output.to_owned(),
                success,
                filter_stats,
                diff: None,
            });
        }
    }

    /// Check blocklist, permission policy, and confirmation requirements for `block`.
    async fn check_permissions(&self, block: &str, skip_confirm: bool) -> Result<(), ToolError> {
        // Always check the blocklist first — it is a hard security boundary
        // that must not be bypassed by the PermissionPolicy layer.
        if let Some(blocked) = self.find_blocked_command(block) {
            let err = ToolError::Blocked {
                command: blocked.to_owned(),
            };
            self.log_audit(
                block,
                AuditResult::Blocked {
                    reason: format!("blocked command: {blocked}"),
                },
                0,
                Some(&err),
            )
            .await;
            return Err(err);
        }

        if let Some(ref policy) = self.permission_policy {
            match policy.check("bash", block) {
                PermissionAction::Deny => {
                    let err = ToolError::Blocked {
                        command: block.to_owned(),
                    };
                    self.log_audit(
                        block,
                        AuditResult::Blocked {
                            reason: "denied by permission policy".to_owned(),
                        },
                        0,
                        Some(&err),
                    )
                    .await;
                    return Err(err);
                }
                PermissionAction::Ask if !skip_confirm => {
                    return Err(ToolError::ConfirmationRequired {
                        command: block.to_owned(),
                    });
                }
                _ => {}
            }
        } else if !skip_confirm && let Some(pattern) = self.find_confirm_command(block) {
            return Err(ToolError::ConfirmationRequired {
                command: pattern.to_owned(),
            });
        }

        Ok(())
    }

    fn validate_sandbox(&self, code: &str) -> Result<(), ToolError> {
        let cwd = std::env::current_dir().unwrap_or_default();

        for token in extract_paths(code) {
            if has_traversal(&token) {
                return Err(ToolError::SandboxViolation { path: token });
            }

            let path = if token.starts_with('/') {
                PathBuf::from(&token)
            } else {
                cwd.join(&token)
            };
            let canonical = path
                .canonicalize()
                .or_else(|_| std::path::absolute(&path))
                .unwrap_or(path);
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

    /// Scan `code` for commands that match the configured blocklist.
    ///
    /// The function normalizes input via [`strip_shell_escapes`] (decoding `$'\xNN'`,
    /// `$'\NNN'`, backslash escapes, and quote-splitting) and then splits on shell
    /// metacharacters (`||`, `&&`, `;`, `|`, `\n`) via [`tokenize_commands`].  Each
    /// resulting token sequence is tested against every entry in `blocked_commands`
    /// through [`tokens_match_pattern`], which handles transparent prefixes (`env`,
    /// `command`, `exec`, etc.), absolute paths, and dot-suffixed variants.
    ///
    /// # Known limitations
    ///
    /// The following constructs are **not** detected by this function:
    ///
    /// - **Here-strings** `<<<` with a shell interpreter: the outer command is the
    ///   shell (`bash`, `sh`), which is not blocked by default; the payload string is
    ///   opaque to this filter.
    ///   Example: `bash <<< 'sudo rm -rf /'` — inner payload is not parsed.
    ///
    /// - **`eval` and `bash -c` / `sh -c`**: the string argument is not parsed; any
    ///   blocked command embedded as a string argument passes through undetected.
    ///   Example: `eval 'sudo rm -rf /'`.
    ///
    /// - **Variable expansion**: `strip_shell_escapes` does not resolve variable
    ///   references, so `cmd=sudo; $cmd rm` bypasses the blocklist.
    ///
    /// `$(...)`, backtick, `<(...)`, and `>(...)` substitutions are detected by
    /// [`extract_subshell_contents`], which extracts the inner command string and
    /// checks it against the blocklist separately.  The default `confirm_patterns`
    /// in [`ShellConfig`] additionally include `"$("`, `` "`" ``, `"<("`, `">("`,
    /// `"<<<"`, and `"eval "`, so those constructs also trigger a confirmation
    /// request via [`find_confirm_command`] before execution.
    ///
    /// For high-security deployments, complement this filter with OS-level sandboxing
    /// (Linux namespaces, seccomp, or similar) to enforce hard execution boundaries.
    fn find_blocked_command(&self, code: &str) -> Option<&str> {
        let cleaned = strip_shell_escapes(&code.to_lowercase());
        let commands = tokenize_commands(&cleaned);
        for blocked in &self.blocked_commands {
            for cmd_tokens in &commands {
                if tokens_match_pattern(cmd_tokens, blocked) {
                    return Some(blocked.as_str());
                }
            }
        }
        // Also check commands embedded inside subshell constructs.
        for inner in extract_subshell_contents(&cleaned) {
            let inner_commands = tokenize_commands(&inner);
            for blocked in &self.blocked_commands {
                for cmd_tokens in &inner_commands {
                    if tokens_match_pattern(cmd_tokens, blocked) {
                        return Some(blocked.as_str());
                    }
                }
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

    async fn log_audit(
        &self,
        command: &str,
        result: AuditResult,
        duration_ms: u64,
        error: Option<&ToolError>,
    ) {
        if let Some(ref logger) = self.audit_logger {
            let (error_category, error_domain) = error.map_or((None, None), |e| {
                let cat = e.category();
                (
                    Some(cat.label().to_owned()),
                    Some(cat.domain().label().to_owned()),
                )
            });
            let entry = AuditEntry {
                timestamp: chrono_now(),
                tool: "shell".into(),
                command: command.into(),
                result,
                duration_ms,
                error_category,
                error_domain,
                error_phase: None,
                claim_source: Some(ClaimSource::Shell),
                mcp_server_id: None,
                injection_flagged: false,
                embedding_anomalous: false,
            };
            logger.log(&entry).await;
        }
    }
}

impl ToolExecutor for ShellExecutor {
    async fn execute(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        self.execute_inner(response, false).await
    }

    fn tool_definitions(&self) -> Vec<crate::registry::ToolDef> {
        use crate::registry::{InvocationHint, ToolDef};
        vec![ToolDef {
            id: "bash".into(),
            description: "Execute a shell command and return stdout/stderr.\n\nParameters: command (string, required) - shell command to run\nReturns: stdout and stderr combined, prefixed with exit code\nErrors: Blocked if command matches security policy; Timeout after configured seconds; SandboxViolation if path outside allowed dirs\nExample: {\"command\": \"ls -la /tmp\"}".into(),
            schema: schemars::schema_for!(BashParams),
            invocation: InvocationHint::FencedBlock("bash"),
        }]
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        if call.tool_id != "bash" {
            return Ok(None);
        }
        let params: BashParams = crate::executor::deserialize_params(&call.params)?;
        if params.command.is_empty() {
            return Ok(None);
        }
        let command = &params.command;
        // Wrap as a fenced block so execute_inner can extract and run it
        let synthetic = format!("```bash\n{command}\n```");
        self.execute_inner(&synthetic, false).await
    }

    fn set_skill_env(&self, env: Option<std::collections::HashMap<String, String>>) {
        ShellExecutor::set_skill_env(self, env);
    }
}

/// Strip shell escape sequences that could bypass command detection.
/// Handles: backslash insertion (`su\do` -> `sudo`), `$'\xNN'` hex and `$'\NNN'` octal
/// escapes, adjacent quoted segments (`"su""do"` -> `sudo`), backslash-newline continuations.
pub(crate) fn strip_shell_escapes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // $'...' ANSI-C quoting: decode \xNN hex and \NNN octal escapes
        if i + 1 < bytes.len() && bytes[i] == b'$' && bytes[i + 1] == b'\'' {
            let mut j = i + 2; // points after $'
            let mut decoded = String::new();
            let mut valid = false;
            while j < bytes.len() && bytes[j] != b'\'' {
                if bytes[j] == b'\\' && j + 1 < bytes.len() {
                    let next = bytes[j + 1];
                    if next == b'x' && j + 3 < bytes.len() {
                        // \xNN hex escape
                        let hi = (bytes[j + 2] as char).to_digit(16);
                        let lo = (bytes[j + 3] as char).to_digit(16);
                        if let (Some(h), Some(l)) = (hi, lo) {
                            #[allow(clippy::cast_possible_truncation)]
                            let byte = ((h << 4) | l) as u8;
                            decoded.push(byte as char);
                            j += 4;
                            valid = true;
                            continue;
                        }
                    } else if next.is_ascii_digit() {
                        // \NNN octal escape (up to 3 digits)
                        let mut val = u32::from(next - b'0');
                        let mut len = 2; // consumed \N so far
                        if j + 2 < bytes.len() && bytes[j + 2].is_ascii_digit() {
                            val = val * 8 + u32::from(bytes[j + 2] - b'0');
                            len = 3;
                            if j + 3 < bytes.len() && bytes[j + 3].is_ascii_digit() {
                                val = val * 8 + u32::from(bytes[j + 3] - b'0');
                                len = 4;
                            }
                        }
                        #[allow(clippy::cast_possible_truncation)]
                        decoded.push((val & 0xFF) as u8 as char);
                        j += len;
                        valid = true;
                        continue;
                    }
                    // other \X escape: emit X literally
                    decoded.push(next as char);
                    j += 2;
                } else {
                    decoded.push(bytes[j] as char);
                    j += 1;
                }
            }
            if j < bytes.len() && bytes[j] == b'\'' && valid {
                out.push_str(&decoded);
                i = j + 1;
                continue;
            }
            // not a decodable $'...' sequence — fall through to handle as regular chars
        }
        // backslash-newline continuation: remove both
        if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
            i += 2;
            continue;
        }
        // intra-word backslash: skip the backslash, keep next char (e.g. su\do -> sudo)
        if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] != b'\n' {
            i += 1;
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        // quoted segment stripping: collapse adjacent quoted segments
        if bytes[i] == b'"' || bytes[i] == b'\'' {
            let quote = bytes[i];
            i += 1;
            while i < bytes.len() && bytes[i] != quote {
                out.push(bytes[i] as char);
                i += 1;
            }
            if i < bytes.len() {
                i += 1; // skip closing quote
            }
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Extract inner command strings from subshell constructs in `s`.
///
/// Recognises:
/// - Backtick: `` `cmd` `` → `cmd`
/// - Dollar-paren: `$(cmd)` → `cmd`
/// - Process substitution (lt): `<(cmd)` → `cmd`
/// - Process substitution (gt): `>(cmd)` → `cmd`
///
/// Depth counting handles nested parentheses correctly.
pub(crate) fn extract_subshell_contents(s: &str) -> Vec<String> {
    let mut results = Vec::new();
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Backtick substitution: `...`
        if chars[i] == '`' {
            let start = i + 1;
            let mut j = start;
            while j < len && chars[j] != '`' {
                j += 1;
            }
            if j < len {
                results.push(chars[start..j].iter().collect());
            }
            i = j + 1;
            continue;
        }

        // $(...), <(...), >(...)
        let next_is_open_paren = i + 1 < len && chars[i + 1] == '(';
        let is_paren_subshell = next_is_open_paren && matches!(chars[i], '$' | '<' | '>');

        if is_paren_subshell {
            let start = i + 2;
            let mut depth: usize = 1;
            let mut j = start;
            while j < len && depth > 0 {
                match chars[j] {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                if depth > 0 {
                    j += 1;
                } else {
                    break;
                }
            }
            if depth == 0 {
                results.push(chars[start..j].iter().collect());
            }
            i = j + 1;
            continue;
        }

        i += 1;
    }

    results
}

/// Split normalized shell code into sub-commands on `|`, `||`, `&&`, `;`, `\n`.
/// Returns list of sub-commands, each as `Vec<String>` of tokens.
pub(crate) fn tokenize_commands(normalized: &str) -> Vec<Vec<String>> {
    // Replace two-char operators with a single separator, then split on single-char separators
    let replaced = normalized.replace("||", "\n").replace("&&", "\n");
    replaced
        .split([';', '|', '\n'])
        .map(|seg| {
            seg.split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<String>>()
        })
        .filter(|tokens| !tokens.is_empty())
        .collect()
}

/// Transparent prefix commands that invoke the next argument as a command.
/// Skipped when determining the "real" command name being invoked.
const TRANSPARENT_PREFIXES: &[&str] = &["env", "command", "exec", "nice", "nohup", "time", "xargs"];

/// Return the basename of a token (last path component after '/').
fn cmd_basename(tok: &str) -> &str {
    tok.rsplit('/').next().unwrap_or(tok)
}

/// Check if the first tokens of a sub-command match a blocked pattern.
/// Handles:
/// - Transparent prefix commands (`env sudo rm` -> checks `sudo`)
/// - Absolute paths (`/usr/bin/sudo rm` -> basename `sudo` is checked)
/// - Dot-suffixed variants (`mkfs` matches `mkfs.ext4`)
/// - Multi-word patterns (`rm -rf /` joined prefix check)
pub(crate) fn tokens_match_pattern(tokens: &[String], pattern: &str) -> bool {
    if tokens.is_empty() || pattern.is_empty() {
        return false;
    }
    let pattern = pattern.trim();
    let pattern_tokens: Vec<&str> = pattern.split_whitespace().collect();
    if pattern_tokens.is_empty() {
        return false;
    }

    // Skip transparent prefix tokens to reach the real command
    let start = tokens
        .iter()
        .position(|t| !TRANSPARENT_PREFIXES.contains(&cmd_basename(t)))
        .unwrap_or(0);
    let effective = &tokens[start..];
    if effective.is_empty() {
        return false;
    }

    if pattern_tokens.len() == 1 {
        let pat = pattern_tokens[0];
        let base = cmd_basename(&effective[0]);
        // Exact match OR dot-suffixed variant (e.g. "mkfs" matches "mkfs.ext4")
        base == pat || base.starts_with(&format!("{pat}."))
    } else {
        // Multi-word: join first N tokens (using basename for first) and check prefix
        let n = pattern_tokens.len().min(effective.len());
        let mut parts: Vec<&str> = vec![cmd_basename(&effective[0])];
        parts.extend(effective[1..n].iter().map(String::as_str));
        let joined = parts.join(" ");
        if joined.starts_with(pattern) {
            return true;
        }
        if effective.len() > n {
            let mut parts2: Vec<&str> = vec![cmd_basename(&effective[0])];
            parts2.extend(effective[1..=n].iter().map(String::as_str));
            parts2.join(" ").starts_with(pattern)
        } else {
            false
        }
    }
}

fn extract_paths(code: &str) -> Vec<String> {
    let mut result = Vec::new();

    // Tokenize respecting single/double quotes
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut chars = code.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' | '\'' => {
                let quote = c;
                while let Some(&nc) = chars.peek() {
                    if nc == quote {
                        chars.next();
                        break;
                    }
                    current.push(chars.next().unwrap());
                }
            }
            c if c.is_whitespace() || matches!(c, ';' | '|' | '&') => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }

    for token in tokens {
        let trimmed = token.trim_end_matches([';', '&', '|']).to_owned();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('/')
            || trimmed.starts_with("./")
            || trimmed.starts_with("../")
            || trimmed == ".."
        {
            result.push(trimmed);
        }
    }
    result
}

/// Classify shell exit codes and stderr patterns into `ToolErrorCategory`.
///
/// Returns `Some(category)` only for well-known failure modes that benefit from
/// structured feedback (exit 126/127, recognisable stderr patterns). All other
/// non-zero exits are left as `Ok` output so they surface verbatim to the LLM.
fn classify_shell_exit(
    exit_code: i32,
    output: &str,
) -> Option<crate::error_taxonomy::ToolErrorCategory> {
    use crate::error_taxonomy::ToolErrorCategory;
    match exit_code {
        // exit 126: command found but not executable (OS-level permission/policy)
        126 => Some(ToolErrorCategory::PolicyBlocked),
        // exit 127: command not found in PATH
        127 => Some(ToolErrorCategory::PermanentFailure),
        _ => {
            let lower = output.to_lowercase();
            if lower.contains("permission denied") {
                Some(ToolErrorCategory::PolicyBlocked)
            } else if lower.contains("no such file or directory") {
                Some(ToolErrorCategory::PermanentFailure)
            } else {
                None
            }
        }
    }
}

fn has_traversal(path: &str) -> bool {
    path.split('/').any(|seg| seg == "..")
}

fn extract_bash_blocks(text: &str) -> Vec<&str> {
    crate::executor::extract_fenced_blocks(text, "bash")
}

/// Kill a child process and its descendants.
/// On unix, sends SIGKILL to child processes via `pkill -KILL -P <pid>` before
/// killing the parent, preventing zombie subprocesses.
async fn kill_process_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let _ = Command::new("pkill")
            .args(["-KILL", "-P", &pid.to_string()])
            .status()
            .await;
    }
    let _ = child.kill().await;
}

async fn execute_bash(
    code: &str,
    timeout: Duration,
    event_tx: Option<&ToolEventTx>,
    cancel_token: Option<&CancellationToken>,
    extra_env: Option<&std::collections::HashMap<String, String>>,
) -> (String, i32) {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};

    let timeout_secs = timeout.as_secs();

    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(code)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(env) = extra_env {
        cmd.envs(env);
    }
    let child_result = cmd.spawn();

    let mut child = match child_result {
        Ok(c) => c,
        Err(e) => return (format!("[error] {e}"), 1),
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
                kill_process_tree(&mut child).await;
                return (format!("[error] command timed out after {timeout_secs}s"), 1);
            }
            () = async {
                match cancel_token {
                    Some(t) => t.cancelled().await,
                    None => std::future::pending().await,
                }
            } => {
                kill_process_tree(&mut child).await;
                return ("[cancelled] operation aborted".to_string(), 130);
            }
        }
    }

    let status = child.wait().await;
    let exit_code = status.ok().and_then(|s| s.code()).unwrap_or(1);

    if combined.is_empty() {
        ("(no output)".to_string(), exit_code)
    } else {
        (combined, exit_code)
    }
}

#[cfg(test)]
mod tests;
