// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shell executor that parses and runs bash blocks from LLM responses.
//!
//! [`ShellExecutor`] is the primary tool backend for Zeph. It handles both legacy
//! fenced bash blocks and structured `bash` tool calls. Security controls enforced
//! before every command:
//!
//! - **Blocklist** — commands matching any entry in `blocked_commands` (or the built-in
//!   [`DEFAULT_BLOCKED_COMMANDS`]) are rejected with [`ToolError::Blocked`].
//! - **Subshell metacharacters** — `$(`, `` ` ``, `<(`, and `>(` are always blocked
//!   because nested evaluation cannot be safely analysed statically.
//! - **Path sandbox** — the working directory and any file arguments must reside under
//!   the configured `allowed_paths`.
//! - **Confirmation gate** — commands matching `confirm_patterns` are held for user
//!   approval before execution (bypassed by `execute_confirmed`).
//! - **Environment blocklist** — variables in `env_blocklist` are stripped from the
//!   subprocess environment before launch.
//! - **Transactional rollback** — when enabled, file snapshots are taken before execution
//!   and restored on failure or on non-zero exit codes in `auto_rollback_exit_codes`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use schemars::JsonSchema;
use serde::Deserialize;

use arc_swap::ArcSwap;
use parking_lot::{Mutex, RwLock};

use zeph_common::ToolName;

use crate::audit::{AuditEntry, AuditLogger, AuditResult, chrono_now};
use crate::config::ShellConfig;
use crate::executor::{
    ClaimSource, FilterStats, ToolCall, ToolError, ToolEvent, ToolEventTx, ToolExecutor, ToolOutput,
};
use crate::filter::{OutputFilterRegistry, sanitize_output};
use crate::permissions::{PermissionAction, PermissionPolicy};
use crate::sandbox::{Sandbox, SandboxPolicy};

pub mod background;
use background::{BackgroundCompletion, BackgroundHandle, RunId};

mod transaction;
use transaction::{TransactionSnapshot, affected_paths, build_scope_matchers, is_write_command};

const DEFAULT_BLOCKED: &[&str] = &[
    "rm -rf /", "sudo", "mkfs", "dd if=", "curl", "wget", "nc ", "ncat", "netcat", "shutdown",
    "reboot", "halt",
];

/// The default list of blocked command patterns used by [`ShellExecutor`].
///
/// Includes highly destructive commands (`rm -rf /`, `mkfs`, `dd if=`), privilege
/// escalation (`sudo`), and network egress tools (`curl`, `wget`, `nc`, `netcat`).
/// Network commands can be re-enabled via [`ShellConfig::allow_network`].
///
/// Exposed so other executors (e.g. `AcpShellExecutor`) can reuse the same
/// blocklist without duplicating it.
pub const DEFAULT_BLOCKED_COMMANDS: &[&str] = DEFAULT_BLOCKED;

/// Shell interpreters that may execute arbitrary code via `-c` or positional args.
///
/// When [`check_blocklist`] receives a command whose binary matches one of these
/// names, the `-c <script>` argument is extracted and checked against the blocklist
/// instead of the binary name.
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

/// Effective command-restriction policy held inside a `ShellExecutor`.
///
/// Swapped atomically on hot-reload via [`ShellPolicyHandle`].
#[derive(Debug)]
pub(crate) struct ShellPolicy {
    pub(crate) blocked_commands: Vec<String>,
}

/// Clonable handle for live policy rebuilds on hot-reload.
///
/// Obtained from [`ShellExecutor::policy_handle`] at construction time and stored
/// on the agent. Call [`ShellPolicyHandle::rebuild`] to atomically replace the
/// effective `blocked_commands` list without recreating the executor. Reads on
/// the dispatch path are lock-free via `ArcSwap::load_full`.
#[derive(Clone, Debug)]
pub struct ShellPolicyHandle {
    inner: Arc<ArcSwap<ShellPolicy>>,
}

impl ShellPolicyHandle {
    /// Atomically install a new effective blocklist derived from `config`.
    ///
    /// # Rebuild contract
    ///
    /// `config` must be the **already-overlay-merged** `ShellConfig` (i.e. the
    /// value produced by `load_config_with_overlay`). Plugin contributions are
    /// already present in `config.blocked_commands` at this point; this method
    /// does NOT re-apply overlays.
    pub fn rebuild(&self, config: &crate::config::ShellConfig) {
        let policy = Arc::new(ShellPolicy {
            blocked_commands: compute_blocked_commands(config),
        });
        self.inner.store(policy);
    }

    /// Snapshot of the current effective blocklist.
    #[must_use]
    pub fn snapshot_blocked(&self) -> Vec<String> {
        self.inner.load().blocked_commands.clone()
    }
}

/// Compute the effective blocklist from an already-overlay-merged `ShellConfig`.
///
/// Invariant: identical to the logic in `ShellExecutor::new`.
pub(crate) fn compute_blocked_commands(config: &crate::config::ShellConfig) -> Vec<String> {
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
    blocked
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct BashParams {
    /// The bash command to execute.
    command: String,
    /// When `true`, spawn the command in the background and return immediately.
    ///
    /// The agent receives a `run_id` in the synchronous tool result. When the
    /// command finishes, a synthetic user-role message is injected at the start
    /// of the next turn carrying the exit code and output.
    #[serde(default)]
    background: bool,
}

/// Bash block extraction and execution via `tokio::process::Command`.
///
/// Parses ` ```bash ` fenced blocks from LLM responses (legacy path) and handles
/// structured `bash` tool calls (modern path). Use [`ShellExecutor::new`] with a
/// [`ShellConfig`] and chain optional builder methods to attach audit logging,
/// event streaming, permission policies, and cancellation.
///
/// # Example
///
/// ```rust,no_run
/// use zeph_tools::{ShellExecutor, ToolExecutor, config::ShellConfig};
///
/// # async fn example() {
/// let executor = ShellExecutor::new(&ShellConfig::default());
///
/// // Execute a fenced bash block.
/// let response = "```bash\npwd\n```";
/// if let Ok(Some(output)) = executor.execute(response).await {
///     println!("{}", output.summary);
/// }
/// # }
/// ```
#[derive(Debug)]
pub struct ShellExecutor {
    timeout: Duration,
    policy: Arc<ArcSwap<ShellPolicy>>,
    allowed_paths: Vec<PathBuf>,
    confirm_patterns: Vec<String>,
    env_blocklist: Vec<String>,
    audit_logger: Option<Arc<AuditLogger>>,
    tool_event_tx: Option<ToolEventTx>,
    permission_policy: Option<PermissionPolicy>,
    output_filter_registry: Option<OutputFilterRegistry>,
    cancel_token: Option<CancellationToken>,
    skill_env: RwLock<Option<std::collections::HashMap<String, String>>>,
    transactional: bool,
    auto_rollback: bool,
    auto_rollback_exit_codes: Vec<i32>,
    snapshot_required: bool,
    max_snapshot_bytes: u64,
    transaction_scope_matchers: Vec<globset::GlobMatcher>,
    sandbox: Option<Arc<dyn Sandbox>>,
    sandbox_policy: Option<SandboxPolicy>,
    /// Registry of in-flight background runs. Bounded by `max_background_runs`.
    background_runs: Arc<Mutex<HashMap<RunId, BackgroundHandle>>>,
    /// Maximum number of concurrent background runs.
    max_background_runs: usize,
    /// Timeout applied to each background run.
    background_timeout: Duration,
    /// Set to `true` during shutdown to prevent new background spawns.
    shutting_down: Arc<AtomicBool>,
    /// Dedicated sender used to forward [`BackgroundCompletion`]s to the agent
    /// (bypasses the UI-facing [`ToolEventTx`] channel). `None` when the agent
    /// has not wired a background completion receiver.
    background_completion_tx: Option<tokio::sync::mpsc::Sender<BackgroundCompletion>>,
}

impl ShellExecutor {
    /// Create a new `ShellExecutor` from configuration.
    ///
    /// Merges the built-in [`DEFAULT_BLOCKED_COMMANDS`] with any additional blocked
    /// commands from `config`, then subtracts any explicitly allowed commands.
    /// No subprocess is spawned at construction time.
    #[must_use]
    pub fn new(config: &ShellConfig) -> Self {
        let policy = Arc::new(ArcSwap::from_pointee(ShellPolicy {
            blocked_commands: compute_blocked_commands(config),
        }));

        let allowed_paths = if config.allowed_paths.is_empty() {
            vec![std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))]
        } else {
            config.allowed_paths.iter().map(PathBuf::from).collect()
        };

        Self {
            timeout: Duration::from_secs(config.timeout),
            policy,
            allowed_paths,
            confirm_patterns: config.confirm_patterns.clone(),
            env_blocklist: config.env_blocklist.clone(),
            audit_logger: None,
            tool_event_tx: None,
            permission_policy: None,
            output_filter_registry: None,
            cancel_token: None,
            skill_env: RwLock::new(None),
            transactional: config.transactional,
            auto_rollback: config.auto_rollback,
            auto_rollback_exit_codes: config.auto_rollback_exit_codes.clone(),
            snapshot_required: config.snapshot_required,
            max_snapshot_bytes: config.max_snapshot_bytes,
            transaction_scope_matchers: build_scope_matchers(&config.transaction_scope),
            sandbox: None,
            sandbox_policy: None,
            background_runs: Arc::new(Mutex::new(HashMap::new())),
            max_background_runs: config.max_background_runs,
            background_timeout: Duration::from_secs(config.background_timeout_secs),
            shutting_down: Arc::new(AtomicBool::new(false)),
            background_completion_tx: None,
        }
    }

    /// Attach an OS-level sandbox backend and a pre-snapshotted policy.
    ///
    /// The policy is snapshotted at construction and never re-resolved per call (no TOCTOU).
    /// If a different policy is needed, create a new `ShellExecutor` via the builder chain.
    #[must_use]
    pub fn with_sandbox(mut self, sandbox: Arc<dyn Sandbox>, policy: SandboxPolicy) -> Self {
        self.sandbox = Some(sandbox);
        self.sandbox_policy = Some(policy);
        self
    }

    /// Set environment variables to inject when executing the active skill's bash blocks.
    pub fn set_skill_env(&self, env: Option<std::collections::HashMap<String, String>>) {
        *self.skill_env.write() = env;
    }

    /// Attach an audit logger. Each shell invocation will emit an [`AuditEntry`].
    #[must_use]
    pub fn with_audit(mut self, logger: Arc<AuditLogger>) -> Self {
        self.audit_logger = Some(logger);
        self
    }

    /// Attach a tool-event sender for streaming output to the TUI or channel adapter.
    ///
    /// When set, [`ToolEvent::Started`], [`ToolEvent::OutputChunk`], and
    /// [`ToolEvent::Completed`] events are sent on `tx` during execution.
    #[must_use]
    pub fn with_tool_event_tx(mut self, tx: ToolEventTx) -> Self {
        self.tool_event_tx = Some(tx);
        self
    }

    /// Attach a dedicated sender for routing [`BackgroundCompletion`] payloads to the agent.
    ///
    /// This channel is separate from [`ToolEventTx`] (which goes to the TUI). The agent holds
    /// the receiver end and drains it at the start of each turn to inject deferred completions
    /// into the message history as a single merged user-role block.
    #[must_use]
    pub fn with_background_completion_tx(
        mut self,
        tx: tokio::sync::mpsc::Sender<BackgroundCompletion>,
    ) -> Self {
        self.background_completion_tx = Some(tx);
        self
    }

    /// Attach a permission policy for confirmation-gate enforcement.
    ///
    /// Commands matching the policy's rules may require user approval before
    /// execution proceeds.
    #[must_use]
    pub fn with_permissions(mut self, policy: PermissionPolicy) -> Self {
        self.permission_policy = Some(policy);
        self
    }

    /// Attach a cancellation token. When the token is cancelled, the running subprocess
    /// is killed and the executor returns [`ToolError::Cancelled`].
    #[must_use]
    pub fn with_cancel_token(mut self, token: CancellationToken) -> Self {
        self.cancel_token = Some(token);
        self
    }

    /// Attach an output filter registry. Filters are applied to stdout+stderr before
    /// the summary is stored in [`ToolOutput`] and sent to the LLM.
    #[must_use]
    pub fn with_output_filters(mut self, registry: OutputFilterRegistry) -> Self {
        self.output_filter_registry = Some(registry);
        self
    }

    /// Return a clonable handle for live policy rebuilds on hot-reload.
    ///
    /// Clone the handle out at construction time and store it on the agent.
    /// Calling [`ShellPolicyHandle::rebuild`] atomically swaps the effective
    /// `blocked_commands` without recreating the executor.
    #[must_use]
    pub fn policy_handle(&self) -> ShellPolicyHandle {
        ShellPolicyHandle {
            inner: Arc::clone(&self.policy),
        }
    }

    /// Execute a bash block bypassing the confirmation check (called after user confirms).
    ///
    /// # Errors
    ///
    /// Returns `ToolError` on blocked commands, sandbox violations, or execution failures.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "tool.shell", skip_all, fields(exit_code = tracing::field::Empty, duration_ms = tracing::field::Empty))
    )]
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
        let mut last_envelope: Option<ShellOutputEnvelope> = None;
        #[allow(clippy::cast_possible_truncation)]
        let blocks_executed = blocks.len() as u32;

        for block in &blocks {
            let (output_line, per_block_stats, envelope) =
                self.execute_block(block, skip_confirm).await?;
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
            last_envelope = Some(envelope);
            outputs.push(output_line);
        }

        let raw_response = last_envelope
            .as_ref()
            .and_then(|e| serde_json::to_value(e).ok());

        Ok(Some(ToolOutput {
            tool_name: ToolName::new("bash"),
            summary: outputs.join("\n\n"),
            blocks_executed,
            filter_stats: cumulative_filter_stats,
            diff: None,
            streamed: self.tool_event_tx.is_some(),
            terminal_id: None,
            locations: None,
            raw_response,
            claim_source: Some(ClaimSource::Shell),
        }))
    }

    #[allow(clippy::too_many_lines)] // long function; decomposition would require extracting state into additional structs — TODO(review): file a tracking issue for this decomposition
    async fn execute_block(
        &self,
        block: &str,
        skip_confirm: bool,
    ) -> Result<(String, Option<FilterStats>, ShellOutputEnvelope), ToolError> {
        self.check_permissions(block, skip_confirm).await?;
        self.validate_sandbox(block)?;

        // Take a transactional snapshot before executing write commands.
        let mut snapshot_warning: Option<String> = None;
        let snapshot = if self.transactional && is_write_command(block) {
            let paths = affected_paths(block, &self.transaction_scope_matchers);
            if paths.is_empty() {
                None
            } else {
                match TransactionSnapshot::capture(&paths, self.max_snapshot_bytes) {
                    Ok(snap) => {
                        tracing::debug!(
                            files = snap.file_count(),
                            bytes = snap.total_bytes(),
                            "transaction snapshot captured"
                        );
                        Some(snap)
                    }
                    Err(e) if self.snapshot_required => {
                        return Err(ToolError::SnapshotFailed {
                            reason: e.to_string(),
                        });
                    }
                    Err(e) => {
                        tracing::warn!(err = %e, "transaction snapshot failed, proceeding without rollback");
                        snapshot_warning =
                            Some(format!("[warn] snapshot failed: {e}; rollback unavailable"));
                        None
                    }
                }
            }
        } else {
            None
        };

        if let Some(ref tx) = self.tool_event_tx {
            let sandbox_profile = self
                .sandbox_policy
                .as_ref()
                .map(|p| format!("{:?}", p.profile));
            // Non-terminal streaming event: use try_send (drop on full).
            let _ = tx.try_send(ToolEvent::Started {
                tool_name: ToolName::new("bash"),
                command: block.to_owned(),
                sandbox_profile,
            });
        }

        let start = Instant::now();
        let skill_env_snapshot: Option<std::collections::HashMap<String, String>> =
            self.skill_env.read().clone();
        let sandbox_pair = self
            .sandbox
            .as_ref()
            .zip(self.sandbox_policy.as_ref())
            .map(|(sb, pol)| (sb.as_ref(), pol));
        let (mut envelope, out) = execute_bash(
            block,
            self.timeout,
            self.tool_event_tx.as_ref(),
            self.cancel_token.as_ref(),
            skill_env_snapshot.as_ref(),
            &self.env_blocklist,
            sandbox_pair,
        )
        .await;
        let exit_code = envelope.exit_code;
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

        // Perform auto-rollback if configured and the exit code qualifies.
        if let Some(snap) = snapshot {
            let should_rollback = self.auto_rollback
                && if self.auto_rollback_exit_codes.is_empty() {
                    exit_code >= 2
                } else {
                    self.auto_rollback_exit_codes.contains(&exit_code)
                };
            if should_rollback {
                match snap.rollback() {
                    Ok(report) => {
                        tracing::info!(
                            restored = report.restored_count,
                            deleted = report.deleted_count,
                            "transaction rollback completed"
                        );
                        self.log_audit(
                            block,
                            AuditResult::Rollback {
                                restored: report.restored_count,
                                deleted: report.deleted_count,
                            },
                            duration_ms,
                            None,
                            Some(exit_code),
                            false,
                        )
                        .await;
                        if let Some(ref tx) = self.tool_event_tx {
                            // Terminal event: must deliver. Use send().await.
                            let _ = tx
                                .send(ToolEvent::Rollback {
                                    tool_name: ToolName::new("bash"),
                                    command: block.to_owned(),
                                    restored_count: report.restored_count,
                                    deleted_count: report.deleted_count,
                                })
                                .await;
                        }
                    }
                    Err(e) => {
                        tracing::error!(err = %e, "transaction rollback failed");
                    }
                }
            }
            // On success (no rollback): snapshot dropped here; TempDir auto-cleans.
        }

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
        if is_timeout {
            self.log_audit(
                block,
                audit_result,
                duration_ms,
                None,
                Some(exit_code),
                false,
            )
            .await;
            self.emit_completed(block, &out, false, None, None).await;
            return Err(ToolError::Timeout {
                timeout_secs: self.timeout.as_secs(),
            });
        }

        if let Some(category) = classify_shell_exit(exit_code, &out) {
            self.emit_completed(block, &out, false, None, None).await;
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
            None,
        )
        .await;

        // Mark truncated if output was shortened during filtering.
        envelope.truncated = filtered.len() < out.len();

        self.log_audit(
            block,
            audit_result,
            duration_ms,
            None,
            Some(exit_code),
            envelope.truncated,
        )
        .await;

        let output_line = if let Some(warn) = snapshot_warning {
            format!("{warn}\n$ {block}\n{filtered}")
        } else {
            format!("$ {block}\n{filtered}")
        };
        Ok((output_line, per_block_stats, envelope))
    }

    async fn emit_completed(
        &self,
        command: &str,
        output: &str,
        success: bool,
        filter_stats: Option<FilterStats>,
        run_id: Option<RunId>,
    ) {
        if let Some(ref tx) = self.tool_event_tx {
            // Terminal event: must deliver. Use send().await (never dropped).
            let _ = tx
                .send(ToolEvent::Completed {
                    tool_name: ToolName::new("bash"),
                    command: command.to_owned(),
                    output: output.to_owned(),
                    success,
                    filter_stats,
                    diff: None,
                    run_id,
                })
                .await;
        }
    }

    /// Check blocklist, permission policy, and confirmation requirements for `block`.
    async fn check_permissions(&self, block: &str, skip_confirm: bool) -> Result<(), ToolError> {
        // Always check the blocklist first — it is a hard security boundary
        // that must not be bypassed by the PermissionPolicy layer.
        if let Some(blocked) = self.find_blocked_command(block) {
            let err = ToolError::Blocked {
                command: blocked.clone(),
            };
            self.log_audit(
                block,
                AuditResult::Blocked {
                    reason: format!("blocked command: {blocked}"),
                },
                0,
                Some(&err),
                None,
                false,
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
                        None,
                        false,
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
    /// Scan `code` for commands that match the configured blocklist.
    ///
    /// Returns an owned `String` because the backing `Vec<String>` lives inside an
    /// `ArcSwap` that may be replaced between calls — borrowing from the snapshot
    /// guard would be unsound after the guard drops.
    fn find_blocked_command(&self, code: &str) -> Option<String> {
        let snapshot = self.policy.load_full();
        let cleaned = strip_shell_escapes(&code.to_lowercase());
        let commands = tokenize_commands(&cleaned);
        for blocked in &snapshot.blocked_commands {
            for cmd_tokens in &commands {
                if tokens_match_pattern(cmd_tokens, blocked) {
                    return Some(blocked.clone());
                }
            }
        }
        // Also check commands embedded inside subshell constructs.
        for inner in extract_subshell_contents(&cleaned) {
            let inner_commands = tokenize_commands(&inner);
            for blocked in &snapshot.blocked_commands {
                for cmd_tokens in &inner_commands {
                    if tokens_match_pattern(cmd_tokens, blocked) {
                        return Some(blocked.clone());
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
        exit_code: Option<i32>,
        truncated: bool,
    ) {
        if let Some(ref logger) = self.audit_logger {
            let (error_category, error_domain, error_phase) =
                error.map_or((None, None, None), |e| {
                    let cat = e.category();
                    (
                        Some(cat.label().to_owned()),
                        Some(cat.domain().label().to_owned()),
                        Some(cat.phase().label().to_owned()),
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
                error_phase,
                claim_source: Some(ClaimSource::Shell),
                mcp_server_id: None,
                injection_flagged: false,
                embedding_anomalous: false,
                cross_boundary_mcp_to_acp: false,
                adversarial_policy_decision: None,
                exit_code,
                truncated,
                caller_id: None,
                policy_match: None,
                correlation_id: None,
                vigil_risk: None,
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
            output_schema: None,
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

        if params.background {
            let run_id = self.spawn_background(command).await?;
            let id_short = &run_id.to_string()[..8];
            return Ok(Some(ToolOutput {
                tool_name: ToolName::new("bash"),
                summary: format!(
                    "[background] started run_id={run_id} — command: {command}\n\
                     The command is running in the background. When it completes, \
                     results will appear at the start of the next turn (run_id_short={id_short})."
                ),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: true,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: Some(ClaimSource::Shell),
            }));
        }

        // Wrap as a fenced block so execute_inner can extract and run it.
        let synthetic = format!("```bash\n{command}\n```");
        self.execute_inner(&synthetic, false).await
    }

    fn set_skill_env(&self, env: Option<std::collections::HashMap<String, String>>) {
        ShellExecutor::set_skill_env(self, env);
    }
}

impl ShellExecutor {
    /// Spawn `command` as a background shell process and return its [`RunId`].
    ///
    /// All security checks (blocklist, sandbox, permissions) are performed synchronously
    /// before spawning. When the cap (`max_background_runs`) is already reached, this
    /// returns [`ToolError::Blocked`] immediately without spawning.
    ///
    /// On completion the spawned task emits a
    /// `ToolEvent::Completed { run_id: Some(..), .. }` via `tool_event_tx`.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Blocked`] when the background run cap is reached or the command
    /// is blocked by policy. Returns other [`ToolError`] variants on sandbox/permission
    /// failures.
    pub async fn spawn_background(&self, command: &str) -> Result<RunId, ToolError> {
        use std::sync::atomic::Ordering;

        // Reject new spawns while shutting down.
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(ToolError::Blocked {
                command: command.to_owned(),
            });
        }

        // Enforce security checks — same as blocking mode.
        self.check_permissions(command, false).await?;
        self.validate_sandbox(command)?;

        // Check cap under lock.
        let run_id = RunId::new();
        {
            let mut runs = self.background_runs.lock();
            if runs.len() >= self.max_background_runs {
                return Err(ToolError::Blocked {
                    command: format!(
                        "background run cap reached (max_background_runs={})",
                        self.max_background_runs
                    ),
                });
            }
            let abort = CancellationToken::new();
            runs.insert(
                run_id,
                BackgroundHandle {
                    command: command.to_owned(),
                    started_at: std::time::Instant::now(),
                    abort: abort.clone(),
                    child_pid: None,
                },
            );
            drop(runs);

            let tool_event_tx = self.tool_event_tx.clone();
            let background_completion_tx = self.background_completion_tx.clone();
            let background_runs = Arc::clone(&self.background_runs);
            let timeout = self.background_timeout;
            let env_blocklist = self.env_blocklist.clone();
            let skill_env_snapshot: Option<std::collections::HashMap<String, String>> =
                self.skill_env.read().clone();
            let command_owned = command.to_owned();

            tokio::spawn(async move {
                let started_at = std::time::Instant::now();
                let (_, out) = execute_bash(
                    &command_owned,
                    timeout,
                    tool_event_tx.as_ref(),
                    Some(&abort),
                    skill_env_snapshot.as_ref(),
                    &env_blocklist,
                    None,
                )
                .await;

                #[allow(clippy::cast_possible_truncation)]
                let elapsed_ms = started_at.elapsed().as_millis() as u64;
                let success = !out.contains("[error]");
                let exit_code = i32::from(!success);
                let truncated = crate::executor::truncate_tool_output_at(&out, 4096);

                // Remove from registry.
                background_runs.lock().remove(&run_id);

                // Deliver terminal event to the TUI/channel adapter.
                if let Some(ref tx) = tool_event_tx {
                    let _ = tx
                        .send(ToolEvent::Completed {
                            tool_name: ToolName::new("bash"),
                            command: command_owned.clone(),
                            output: truncated.clone(),
                            success,
                            filter_stats: None,
                            diff: None,
                            run_id: Some(run_id),
                        })
                        .await;
                }

                // Deliver completion to the agent for injection into the next turn.
                if let Some(ref tx) = background_completion_tx {
                    let completion = BackgroundCompletion {
                        run_id,
                        exit_code,
                        output: truncated,
                        success,
                        elapsed_ms,
                        command: command_owned,
                    };
                    if tx.send(completion).await.is_err() {
                        tracing::warn!(
                            run_id = %run_id,
                            "background completion channel closed; agent may have shut down"
                        );
                    }
                }

                tracing::debug!(
                    run_id = %run_id,
                    exit_code,
                    elapsed_ms,
                    "background shell run completed"
                );
            });
        }

        Ok(run_id)
    }

    /// Cancel all in-flight background runs.
    ///
    /// Called during agent shutdown. Each cancelled run emits a
    /// `ToolEvent::Completed { success: false }` event. Cancellation is cooperative:
    /// the spawned tasks detect the token and exit on the next check point.
    ///
    /// # Note
    ///
    /// SIGTERM/SIGKILL escalation is deferred to a future enhancement
    /// (requires a safe OS-signal abstraction). The `CancellationToken` is
    /// sufficient for the process-local case.
    // TODO(review): add SIGTERM+SIGKILL escalation via a safe signal wrapper (e.g. nix crate).
    pub async fn shutdown(&self) {
        use std::sync::atomic::Ordering;

        self.shutting_down.store(true, Ordering::Release);

        let handles: Vec<(RunId, String, CancellationToken)> = {
            let runs = self.background_runs.lock();
            runs.iter()
                .map(|(id, h)| (*id, h.command.clone(), h.abort.clone()))
                .collect()
        };

        if handles.is_empty() {
            return;
        }

        tracing::info!(
            count = handles.len(),
            "cancelling background shell runs for shutdown"
        );

        for (run_id, command, abort) in &handles {
            abort.cancel();

            if let Some(ref tx) = self.tool_event_tx {
                let _ = tx
                    .send(ToolEvent::Completed {
                        tool_name: ToolName::new("bash"),
                        command: command.clone(),
                        output: "[terminated by shutdown]".to_owned(),
                        success: false,
                        filter_stats: None,
                        diff: None,
                        run_id: Some(*run_id),
                    })
                    .await;
            }
        }

        self.background_runs.lock().clear();
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
            || (trimmed.starts_with('.') && trimmed.contains('/'))
            || is_relative_path_token(&trimmed)
        {
            result.push(trimmed);
        }
    }
    result
}

/// Returns `true` if `token` looks like a relative path of the form `word/more`
/// (contains `/` but does not start with `/` or `.`).
///
/// Excluded:
/// - URL schemes (`scheme://`)
/// - Shell variable assignments (`KEY=value`)
fn is_relative_path_token(token: &str) -> bool {
    // Must contain a slash but not start with `/` (absolute) or `.` (handled above).
    if !token.contains('/') || token.starts_with('/') || token.starts_with('.') {
        return false;
    }
    // Reject URLs: anything with `://`
    if token.contains("://") {
        return false;
    }
    // Reject shell variable assignments: `IDENTIFIER=...`
    if let Some(eq_pos) = token.find('=') {
        let key = &token[..eq_pos];
        if key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return false;
        }
    }
    // First character must be an identifier-start (letter, digit, or `_`).
    token
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
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

/// Structured output from a shell command execution.
///
/// Produced by the internal `execute_bash` function and included in the final
/// [`ToolOutput`] and [`AuditEntry`] for the invocation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ShellOutputEnvelope {
    /// Captured standard output, possibly truncated.
    pub stdout: String,
    /// Captured standard error, possibly truncated.
    pub stderr: String,
    /// Process exit code. `0` indicates success by convention.
    pub exit_code: i32,
    /// `true` when the combined output exceeded the configured max and was truncated.
    pub truncated: bool,
}

#[allow(clippy::too_many_lines)] // long function; decomposition would require extracting state into additional structs — TODO(review): file a tracking issue for this decomposition
async fn execute_bash(
    code: &str,
    timeout: Duration,
    event_tx: Option<&ToolEventTx>,
    cancel_token: Option<&CancellationToken>,
    extra_env: Option<&std::collections::HashMap<String, String>>,
    env_blocklist: &[String],
    sandbox: Option<(&dyn Sandbox, &SandboxPolicy)>,
) -> (ShellOutputEnvelope, String) {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};

    let timeout_secs = timeout.as_secs();

    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(code);

    for (key, _) in std::env::vars() {
        if env_blocklist
            .iter()
            .any(|prefix| key.starts_with(prefix.as_str()))
        {
            cmd.env_remove(&key);
        }
    }

    if let Some(env) = extra_env {
        cmd.envs(env);
    }

    // Apply OS sandbox before setting stdio so the rewritten program is sandboxed.
    if let Some((sb, policy)) = sandbox
        && let Err(err) = sb.wrap(&mut cmd, policy)
    {
        let msg = format!("[error] sandbox setup failed: {err}");
        return (
            ShellOutputEnvelope {
                stdout: String::new(),
                stderr: msg.clone(),
                exit_code: 1,
                truncated: false,
            },
            msg,
        );
    }

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let child_result = cmd.spawn();

    let mut child = match child_result {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("[error] {e}");
            return (
                ShellOutputEnvelope {
                    stdout: String::new(),
                    stderr: msg.clone(),
                    exit_code: 1,
                    truncated: false,
                },
                msg,
            );
        }
    };

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    // Channel carries (is_stderr, line) so we can accumulate separate buffers
    // while still building a combined interleaved string for streaming and LLM context.
    let (line_tx, mut line_rx) = tokio::sync::mpsc::channel::<(bool, String)>(64);

    let stdout_tx = line_tx.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut buf = String::new();
        while reader.read_line(&mut buf).await.unwrap_or(0) > 0 {
            let _ = stdout_tx.send((false, buf.clone())).await;
            buf.clear();
        }
    });

    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut buf = String::new();
        while reader.read_line(&mut buf).await.unwrap_or(0) > 0 {
            let _ = line_tx.send((true, buf.clone())).await;
            buf.clear();
        }
    });

    let mut combined = String::new();
    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        tokio::select! {
            line = line_rx.recv() => {
                match line {
                    Some((is_stderr, chunk)) => {
                        let interleaved = if is_stderr {
                            format!("[stderr] {chunk}")
                        } else {
                            chunk.clone()
                        };
                        if let Some(tx) = event_tx {
                            // Non-terminal streaming event: use try_send (drop on full).
                            let _ = tx.try_send(ToolEvent::OutputChunk {
                                tool_name: ToolName::new("bash"),
                                command: code.to_owned(),
                                chunk: interleaved.clone(),
                            });
                        }
                        combined.push_str(&interleaved);
                        if is_stderr {
                            stderr_buf.push_str(&chunk);
                        } else {
                            stdout_buf.push_str(&chunk);
                        }
                    }
                    None => break,
                }
            }
            () = tokio::time::sleep_until(deadline) => {
                kill_process_tree(&mut child).await;
                let msg = format!("[error] command timed out after {timeout_secs}s");
                return (
                    ShellOutputEnvelope {
                        stdout: stdout_buf,
                        stderr: format!("{stderr_buf}command timed out after {timeout_secs}s"),
                        exit_code: 1,
                        truncated: false,
                    },
                    msg,
                );
            }
            () = async {
                match cancel_token {
                    Some(t) => t.cancelled().await,
                    None => std::future::pending().await,
                }
            } => {
                kill_process_tree(&mut child).await;
                return (
                    ShellOutputEnvelope {
                        stdout: stdout_buf,
                        stderr: format!("{stderr_buf}operation aborted"),
                        exit_code: 130,
                        truncated: false,
                    },
                    "[cancelled] operation aborted".to_string(),
                );
            }
        }
    }

    let status = child.wait().await;
    let exit_code = status.ok().and_then(|s| s.code()).unwrap_or(1);

    let (envelope, combined) = if combined.is_empty() {
        (
            ShellOutputEnvelope {
                stdout: String::new(),
                stderr: String::new(),
                exit_code,
                truncated: false,
            },
            "(no output)".to_string(),
        )
    } else {
        (
            ShellOutputEnvelope {
                stdout: stdout_buf.trim_end().to_owned(),
                stderr: stderr_buf.trim_end().to_owned(),
                exit_code,
                truncated: false,
            },
            combined,
        )
    };
    (envelope, combined)
}

#[cfg(test)]
mod tests;
