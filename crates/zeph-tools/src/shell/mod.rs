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
use crate::execution_context::ExecutionContext;
use crate::executor::{
    ClaimSource, FilterStats, ToolCall, ToolError, ToolEvent, ToolEventTx, ToolExecutor, ToolOutput,
};
use crate::filter::{OutputFilterRegistry, sanitize_output};
use crate::permissions::{PermissionAction, PermissionPolicy};
use crate::sandbox::{Sandbox, SandboxPolicy};

pub mod background;
pub use background::BackgroundRunSnapshot;
use background::{BackgroundCompletion, BackgroundHandle, RunId};

mod transaction;
use transaction::{TransactionSnapshot, affected_paths, build_scope_matchers, is_write_command};

const DEFAULT_BLOCKED: &[&str] = &[
    "rm -rf /", "sudo", "mkfs", "dd if=", "curl", "wget", "nc ", "ncat", "netcat", "shutdown",
    "reboot", "halt",
];

/// Graceful period between SIGTERM and SIGKILL during process escalation.
#[cfg(unix)]
const GRACEFUL_TERM_MS: Duration = Duration::from_millis(250);

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
/// use zeph_tools::{ShellExecutor, ToolExecutor, ShellConfig};
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
    /// Named execution environment registry built from `[execution]` config.
    /// Keys are case-sensitive environment names; values are trusted `ExecutionContext`s.
    environments: Arc<HashMap<String, ExecutionContext>>,
    /// Pre-canonicalized `allowed_paths`. Built once at construction to avoid TOCTOU
    /// between the canonicalize call and the prefix check at `resolve_context` time.
    allowed_paths_canonical: Vec<PathBuf>,
    /// Optional default environment name (from `[execution] default_env`).
    default_env: Option<String>,
}

/// Fully resolved execution context for a single shell invocation.
///
/// Produced by [`ShellExecutor::resolve_context`] and passed to the inner execute
/// functions. The canonical `cwd` is what `cmd.current_dir` receives — identical to
/// the path that was validated against `allowed_paths`.
#[derive(Debug)]
pub(crate) struct ResolvedContext {
    /// Canonical absolute working directory (follows all symlinks).
    pub(crate) cwd: PathBuf,
    /// Final merged environment (post-blocklist filter).
    pub(crate) env: HashMap<String, String>,
    /// Resolved environment name, for logs and audit entries.
    pub(crate) name: Option<String>,
    /// Whether the context originated from a trusted source (operator TOML).
    /// Reserved for future audit log enrichment.
    #[allow(dead_code)]
    pub(crate) trusted: bool,
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

        let allowed_paths: Vec<PathBuf> = if config.allowed_paths.is_empty() {
            vec![std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))]
        } else {
            config.allowed_paths.iter().map(PathBuf::from).collect()
        };
        let allowed_paths_canonical: Vec<PathBuf> = allowed_paths
            .iter()
            .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
            .collect();

        Self {
            timeout: Duration::from_secs(config.timeout),
            policy,
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
            environments: Arc::new(HashMap::new()),
            allowed_paths_canonical,
            default_env: None,
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

    /// Build the environment registry from `[execution]` config and wire it in one step.
    ///
    /// Convenience wrapper for agent startup. Converts [`zeph_config::ExecutionConfig`]
    /// entries into trusted [`ExecutionContext`] instances and passes them to
    /// [`Self::with_environments`].
    ///
    /// # Errors
    ///
    /// Returns an error string when any registry entry's `cwd` cannot be canonicalized
    /// or escapes `allowed_paths`.
    pub fn with_execution_config(
        self,
        config: &zeph_config::ExecutionConfig,
    ) -> Result<Self, String> {
        let registry: HashMap<String, ExecutionContext> = config
            .environments
            .iter()
            .map(|e| {
                let ctx = ExecutionContext::trusted_from_parts(
                    Some(e.name.clone()),
                    Some(std::path::PathBuf::from(&e.cwd)),
                    e.env.clone(),
                );
                (e.name.clone(), ctx)
            })
            .collect();
        self.with_environments(registry, config.default_env.clone())
    }

    /// Wire the named execution environment registry from `[execution]` config.
    ///
    /// Builds trusted [`ExecutionContext`] instances from the operator-authored TOML
    /// entries and canonicalizes their `cwd` paths at construction time.
    ///
    /// # Errors
    ///
    /// Returns an error string (surfaced at agent startup) when a registry entry's
    /// `cwd` path does not exist, cannot be canonicalized, or escapes `allowed_paths`.
    pub fn with_environments(
        mut self,
        environments: HashMap<String, ExecutionContext>,
        default_env: Option<String>,
    ) -> Result<Self, String> {
        // Validate that all registered cwds exist and are under allowed_paths.
        for (name, ctx) in &environments {
            if let Some(cwd) = ctx.cwd() {
                let canonical = cwd.canonicalize().map_err(|e| {
                    format!(
                        "execution environment '{name}': cwd '{}' cannot be canonicalized: {e}",
                        cwd.display()
                    )
                })?;
                if !self
                    .allowed_paths_canonical
                    .iter()
                    .any(|p| canonical.starts_with(p))
                {
                    return Err(format!(
                        "execution environment '{name}': cwd '{}' is outside allowed_paths",
                        cwd.display()
                    ));
                }
            }
        }
        self.environments = Arc::new(environments);
        self.default_env = default_env;
        Ok(self)
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

    /// Snapshot all in-flight background runs.
    ///
    /// Acquires the lock once, maps each [`BackgroundHandle`] to a
    /// [`BackgroundRunSnapshot`], then drops the guard before returning.
    /// Safe to call from any thread.
    #[must_use]
    pub fn background_runs_snapshot(&self) -> Vec<background::BackgroundRunSnapshot> {
        let runs = self.background_runs.lock();
        runs.iter()
            .map(|(id, h)| {
                #[allow(clippy::cast_possible_truncation)]
                let elapsed_ms = h.elapsed().as_millis() as u64;
                background::BackgroundRunSnapshot {
                    run_id: id.to_string(),
                    command: h.command.clone(),
                    elapsed_ms,
                }
            })
            .collect()
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

        // Resolve with no call-site context so legacy path gets the same CWD/env
        // treatment as the structured-tool-call path (default_env, skill_env, blocklist).
        let resolved = self.resolve_context(None)?;

        let mut outputs = Vec::with_capacity(blocks.len());
        let mut cumulative_filter_stats: Option<FilterStats> = None;
        let mut last_envelope: Option<ShellOutputEnvelope> = None;
        #[allow(clippy::cast_possible_truncation)]
        let blocks_executed = blocks.len() as u32;

        for block in &blocks {
            let (output_line, per_block_stats, envelope) =
                self.execute_block(block, skip_confirm, &resolved).await?;
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

    async fn execute_block(
        &self,
        block: &str,
        skip_confirm: bool,
        resolved: &ResolvedContext,
    ) -> Result<(String, Option<FilterStats>, ShellOutputEnvelope), ToolError> {
        self.check_permissions(block, skip_confirm).await?;
        self.validate_sandbox_with_cwd(block, &resolved.cwd)?;

        let (snapshot, snapshot_warning) = self.capture_snapshot_for(block)?;

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
                resolved_cwd: Some(resolved.cwd.display().to_string()),
                execution_env: resolved.name.clone(),
            });
        }

        let start = Instant::now();
        let sandbox_pair = self
            .sandbox
            .as_ref()
            .zip(self.sandbox_policy.as_ref())
            .map(|(sb, pol)| (sb.as_ref(), pol));
        let (mut envelope, out) = execute_bash_with_context(
            block,
            self.timeout,
            self.tool_event_tx.as_ref(),
            self.cancel_token.as_ref(),
            resolved,
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

        if let Some(snap) = snapshot {
            self.maybe_rollback(snap, block, exit_code, duration_ms)
                .await;
        }

        if let Some(err) = self
            .classify_and_audit(block, &out, exit_code, duration_ms)
            .await
        {
            self.emit_completed(block, &out, false, None, None).await;
            return Err(err);
        }

        let (filtered, per_block_stats) = self.apply_output_filter(block, &out, exit_code);

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

        let audit_result = if out.contains("[error]") || out.contains("[stderr]") {
            AuditResult::Error {
                message: out.clone(),
            }
        } else {
            AuditResult::Success
        };
        self.log_audit_with_context(
            block,
            audit_result,
            duration_ms,
            None,
            Some(exit_code),
            envelope.truncated,
            resolved,
        )
        .await;

        let output_line = match snapshot_warning {
            Some(warn) => format!("{warn}\n$ {block}\n{filtered}"),
            None => format!("$ {block}\n{filtered}"),
        };
        Ok((output_line, per_block_stats, envelope))
    }

    /// Execute `command` using a pre-resolved [`ResolvedContext`] (from `resolve_context`).
    ///
    /// This is the structured-tool-call path — it uses the resolved CWD and env directly
    /// instead of re-reading process state on every call.
    #[tracing::instrument(name = "tool.shell.execute_block", skip(self, resolved), level = "info",
        fields(cwd = %resolved.cwd.display(), env_name = resolved.name.as_deref().unwrap_or("")))]
    async fn execute_block_with_context(
        &self,
        command: &str,
        skip_confirm: bool,
        resolved: &ResolvedContext,
    ) -> Result<Option<ToolOutput>, ToolError> {
        self.check_permissions(command, skip_confirm).await?;
        self.validate_sandbox_with_cwd(command, &resolved.cwd)?;

        let (snapshot, snapshot_warning) = self.capture_snapshot_for(command)?;

        if let Some(ref tx) = self.tool_event_tx {
            let sandbox_profile = self
                .sandbox_policy
                .as_ref()
                .map(|p| format!("{:?}", p.profile));
            let _ = tx.try_send(ToolEvent::Started {
                tool_name: ToolName::new("bash"),
                command: command.to_owned(),
                sandbox_profile,
                resolved_cwd: Some(resolved.cwd.display().to_string()),
                execution_env: resolved.name.clone(),
            });
        }

        let start = Instant::now();
        let sandbox_pair = self
            .sandbox
            .as_ref()
            .zip(self.sandbox_policy.as_ref())
            .map(|(sb, pol)| (sb.as_ref(), pol));
        let (mut envelope, out) = execute_bash_with_context(
            command,
            self.timeout,
            self.tool_event_tx.as_ref(),
            self.cancel_token.as_ref(),
            resolved,
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

        if let Some(snap) = snapshot {
            self.maybe_rollback(snap, command, exit_code, duration_ms)
                .await;
        }

        if let Some(err) = self
            .classify_and_audit(command, &out, exit_code, duration_ms)
            .await
        {
            self.emit_completed(command, &out, false, None, None).await;
            return Err(err);
        }

        let (filtered, per_block_stats) = self.apply_output_filter(command, &out, exit_code);

        self.emit_completed(
            command,
            &out,
            !out.contains("[error]"),
            per_block_stats.clone(),
            None,
        )
        .await;

        envelope.truncated = filtered.len() < out.len();

        let audit_result = if out.contains("[error]") || out.contains("[stderr]") {
            AuditResult::Error {
                message: out.clone(),
            }
        } else {
            AuditResult::Success
        };
        self.log_audit_with_context(
            command,
            audit_result,
            duration_ms,
            None,
            Some(exit_code),
            envelope.truncated,
            resolved,
        )
        .await;

        let output_line = match snapshot_warning {
            Some(warn) => format!("{warn}\n$ {command}\n{filtered}"),
            None => format!("$ {command}\n{filtered}"),
        };
        Ok(Some(ToolOutput {
            tool_name: ToolName::new("bash"),
            summary: output_line,
            blocks_executed: 1,
            filter_stats: per_block_stats,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: Some(ClaimSource::Shell),
        }))
    }

    fn capture_snapshot_for(
        &self,
        block: &str,
    ) -> Result<(Option<TransactionSnapshot>, Option<String>), ToolError> {
        if !self.transactional || !is_write_command(block) {
            return Ok((None, None));
        }
        let paths = affected_paths(block, &self.transaction_scope_matchers);
        if paths.is_empty() {
            return Ok((None, None));
        }
        match TransactionSnapshot::capture(&paths, self.max_snapshot_bytes) {
            Ok(snap) => {
                tracing::debug!(
                    files = snap.file_count(),
                    bytes = snap.total_bytes(),
                    "transaction snapshot captured"
                );
                Ok((Some(snap), None))
            }
            Err(e) if self.snapshot_required => Err(ToolError::SnapshotFailed {
                reason: e.to_string(),
            }),
            Err(e) => {
                tracing::warn!(err = %e, "transaction snapshot failed, proceeding without rollback");
                Ok((
                    None,
                    Some(format!("[warn] snapshot failed: {e}; rollback unavailable")),
                ))
            }
        }
    }

    async fn maybe_rollback(
        &self,
        snap: TransactionSnapshot,
        block: &str,
        exit_code: i32,
        duration_ms: u64,
    ) {
        let should_rollback = self.auto_rollback
            && if self.auto_rollback_exit_codes.is_empty() {
                exit_code >= 2
            } else {
                self.auto_rollback_exit_codes.contains(&exit_code)
            };
        if !should_rollback {
            // Snapshot dropped here; TempDir auto-cleans.
            return;
        }
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

    async fn classify_and_audit(
        &self,
        block: &str,
        out: &str,
        exit_code: i32,
        duration_ms: u64,
    ) -> Option<ToolError> {
        if out.contains("[error] command timed out") {
            self.log_audit(
                block,
                AuditResult::Timeout,
                duration_ms,
                None,
                Some(exit_code),
                false,
            )
            .await;
            return Some(ToolError::Timeout {
                timeout_secs: self.timeout.as_secs(),
            });
        }

        if let Some(category) = classify_shell_exit(exit_code, out) {
            return Some(ToolError::Shell {
                exit_code,
                category,
                message: out.lines().take(3).collect::<Vec<_>>().join("; "),
            });
        }

        None
    }

    fn apply_output_filter(
        &self,
        block: &str,
        out: &str,
        exit_code: i32,
    ) -> (String, Option<FilterStats>) {
        let sanitized = sanitize_output(out);
        if let Some(ref registry) = self.output_filter_registry {
            match registry.apply(block, &sanitized, exit_code) {
                Some(fr) => {
                    tracing::debug!(
                        command = block,
                        raw = fr.raw_chars,
                        filtered = fr.filtered_chars,
                        savings_pct = fr.savings_pct(),
                        "output filter applied"
                    );
                    let stats = FilterStats {
                        raw_chars: fr.raw_chars,
                        filtered_chars: fr.filtered_chars,
                        raw_lines: fr.raw_lines,
                        filtered_lines: fr.filtered_lines,
                        confidence: Some(fr.confidence),
                        command: Some(block.to_owned()),
                        kept_lines: fr.kept_lines.clone(),
                    };
                    (fr.output, Some(stats))
                }
                None => (sanitized, None),
            }
        } else {
            (sanitized, None)
        }
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

    /// Resolve the effective `(cwd, env, name, trusted)` for a single tool call.
    ///
    /// Implements the 6-step merge defined in the per-turn env spec:
    /// 1. Base = inherited process env.
    /// 2. Filter `env_blocklist`.
    /// 3. Apply `skill_env` overrides.
    /// 4. If `ctx` or `default_env` points to a named registry entry, apply its overrides.
    /// 5. Apply call-site `ctx.env_overrides`.
    /// 6. If context is untrusted, re-apply `env_blocklist` to strip any re-introduced keys.
    ///
    /// CWD precedence (highest wins): call-site `ctx.cwd` → named registry `cwd` → `default_env`
    /// registry `cwd` → `std::env::current_dir()`.
    #[tracing::instrument(name = "tools.shell.resolve_context", skip(self, ctx), level = "info")]
    pub(crate) fn resolve_context(
        &self,
        ctx: Option<&ExecutionContext>,
    ) -> Result<ResolvedContext, ToolError> {
        // Step 1: base env = process env.
        let mut env: HashMap<String, String> = std::env::vars().collect();

        // Step 2: filter env_blocklist (prefix match, consistent with build_bash_command).
        env.retain(|k, _| {
            !self
                .env_blocklist
                .iter()
                .any(|prefix| k.starts_with(prefix.as_str()))
        });

        // Step 3: apply skill_env.
        if let Some(skill) = self.skill_env.read().as_ref() {
            for (k, v) in skill {
                env.insert(k.clone(), v.clone());
            }
        }

        // Determine the resolved name, cwd_override, and trusted flag.
        let mut resolved_name: Option<String> = None;
        let mut cwd_override: Option<PathBuf> = None;
        let mut trusted = false;

        // Resolve via default_env registry entry (lowest priority named layer).
        if let Some(default_name) = &self.default_env
            && let Some(default_ctx) = self.environments.get(default_name.as_str())
        {
            resolved_name.get_or_insert_with(|| default_name.clone());
            if cwd_override.is_none() {
                cwd_override = default_ctx.cwd().map(ToOwned::to_owned);
            }
            trusted = default_ctx.is_trusted();
            for (k, v) in default_ctx.env_overrides() {
                env.insert(k.clone(), v.clone());
            }
        }

        // Step 4: if call-site ctx names a registry entry, apply its overrides.
        if let Some(ctx) = ctx {
            if let Some(name) = ctx.name() {
                if let Some(reg_ctx) = self.environments.get(name) {
                    resolved_name = Some(name.to_owned());
                    if let Some(cwd) = reg_ctx.cwd() {
                        cwd_override = Some(cwd.to_owned());
                    }
                    trusted = reg_ctx.is_trusted();
                    for (k, v) in reg_ctx.env_overrides() {
                        env.insert(k.clone(), v.clone());
                    }
                } else {
                    return Err(ToolError::Execution(std::io::Error::other(format!(
                        "unknown execution environment '{name}'"
                    ))));
                }
            }

            // Step 5: apply call-site cwd and env overrides (highest priority).
            if let Some(cwd) = ctx.cwd() {
                cwd_override = Some(cwd.to_owned());
            }
            if !ctx.is_trusted() {
                trusted = false;
            }
            for (k, v) in ctx.env_overrides() {
                env.insert(k.clone(), v.clone());
            }
        }

        // Step 6: re-apply blocklist for untrusted contexts (prefix match).
        if !trusted {
            env.retain(|k, _| {
                !self
                    .env_blocklist
                    .iter()
                    .any(|prefix| k.starts_with(prefix.as_str()))
            });
        }

        // Resolve final CWD: override (canonicalized) or process CWD.
        let cwd = if let Some(raw) = cwd_override {
            // Make relative paths absolute before canonicalize so they resolve
            // correctly regardless of the process working directory.
            let raw = if raw.is_absolute() {
                raw
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(raw)
            };
            let canonical = raw
                .canonicalize()
                .map_err(|_| ToolError::SandboxViolation {
                    path: raw.display().to_string(),
                })?;
            // Validate against allowed_paths.
            if !self
                .allowed_paths_canonical
                .iter()
                .any(|p| canonical.starts_with(p))
            {
                return Err(ToolError::SandboxViolation {
                    path: canonical.display().to_string(),
                });
            }
            canonical
        } else {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        };

        Ok(ResolvedContext {
            cwd,
            env,
            name: resolved_name,
            trusted,
        })
    }

    fn validate_sandbox_with_cwd(
        &self,
        code: &str,
        cwd: &std::path::Path,
    ) -> Result<(), ToolError> {
        for token in extract_paths(code) {
            if has_traversal(&token) {
                return Err(ToolError::SandboxViolation { path: token });
            }

            if self.allowed_paths_canonical.is_empty() {
                continue;
            }

            let path = if token.starts_with('/') {
                PathBuf::from(&token)
            } else {
                cwd.join(&token)
            };
            // For existing paths, canonicalize to resolve symlinks before the prefix
            // check — `std::path::absolute` does NOT collapse `..` or follow symlinks.
            // For non-existent paths, canonicalize the nearest existing ancestor and
            // reattach the suffix: this rejects `allowed/../../etc/shadow` while
            // allowing references to not-yet-created files within allowed dirs.
            let canonical = if let Ok(c) = path.canonicalize() {
                c
            } else {
                // Collect path components so we can walk up from the full path.
                let components: Vec<_> = path.components().collect();
                let mut base_len = components.len();
                let canonical_base = loop {
                    if base_len == 0 {
                        break PathBuf::new();
                    }
                    let candidate: PathBuf = components[..base_len].iter().collect();
                    if let Ok(c) = candidate.canonicalize() {
                        break c;
                    }
                    base_len -= 1;
                };
                // Reattach the non-existent suffix (components after base_len).
                components[base_len..]
                    .iter()
                    .fold(canonical_base, |acc, c| acc.join(c))
            };
            if !self
                .allowed_paths_canonical
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

    fn validate_sandbox(&self, code: &str) -> Result<(), ToolError> {
        let cwd = std::env::current_dir().unwrap_or_default();
        self.validate_sandbox_with_cwd(code, &cwd)
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
                execution_env: None,
                resolved_cwd: None,
                scope_at_definition: None,
                scope_at_dispatch: None,
            };
            logger.log(&entry).await;
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn log_audit_with_context(
        &self,
        command: &str,
        result: AuditResult,
        duration_ms: u64,
        error: Option<&ToolError>,
        exit_code: Option<i32>,
        truncated: bool,
        resolved: &ResolvedContext,
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
                execution_env: resolved.name.clone(),
                resolved_cwd: Some(resolved.cwd.display().to_string()),
                scope_at_definition: None,
                scope_at_dispatch: None,
            };
            logger.log(&entry).await;
        }
    }
}

impl ToolExecutor for std::sync::Arc<ShellExecutor> {
    async fn execute(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        self.as_ref().execute(response).await
    }

    fn tool_definitions(&self) -> Vec<crate::registry::ToolDef> {
        self.as_ref().tool_definitions()
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        self.as_ref().execute_tool_call(call).await
    }

    fn set_skill_env(&self, env: Option<std::collections::HashMap<String, String>>) {
        self.as_ref().set_skill_env(env);
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

    #[tracing::instrument(name = "tool.shell.execute_tool_call", skip(self, call), level = "info",
        fields(tool_id = %call.tool_id, env = call.context.as_ref().and_then(|c| c.name()).unwrap_or("")))]
    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        if call.tool_id != "bash" {
            return Ok(None);
        }
        let params: BashParams = crate::executor::deserialize_params(&call.params)?;
        if params.command.is_empty() {
            return Ok(None);
        }
        let command = &params.command;

        // Resolve per-turn execution context — done before the background branch so that
        // background tasks also receive the correct env and CWD (spec §6).
        let resolved = self.resolve_context(call.context.as_ref())?;

        if params.background {
            let run_id = self
                .spawn_background_with_context(command, &resolved)
                .await?;
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

        self.execute_block_with_context(command, false, &resolved)
            .await
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

        // Check cap under lock, then register the handle and spawn.
        let run_id = RunId::new();
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

        tokio::spawn(run_background_task(
            run_id,
            command_owned,
            timeout,
            abort,
            background_runs,
            tool_event_tx,
            background_completion_tx,
            skill_env_snapshot,
            env_blocklist,
        ));

        Ok(run_id)
    }

    /// Spawn `command` as a background process using an already-resolved [`ResolvedContext`].
    ///
    /// Like [`spawn_background`](Self::spawn_background) but uses the pre-resolved env and CWD
    /// instead of reading `skill_env`/process-env at spawn time.
    ///
    /// # Errors
    ///
    /// Same as [`spawn_background`](Self::spawn_background).
    async fn spawn_background_with_context(
        &self,
        command: &str,
        resolved: &ResolvedContext,
    ) -> Result<RunId, ToolError> {
        use std::sync::atomic::Ordering;

        if self.shutting_down.load(Ordering::Acquire) {
            return Err(ToolError::Blocked {
                command: command.to_owned(),
            });
        }

        self.check_permissions(command, false).await?;
        self.validate_sandbox_with_cwd(command, &resolved.cwd)?;

        let run_id = RunId::new();
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
        let env = resolved.env.clone();
        let cwd = resolved.cwd.clone();
        let command_owned = command.to_owned();

        tokio::spawn(run_background_task_with_env(
            run_id,
            command_owned,
            timeout,
            abort,
            background_runs,
            tool_event_tx,
            background_completion_tx,
            env,
            cwd,
        ));

        Ok(run_id)
    }

    /// Cancel all in-flight background runs.
    ///
    /// Called during agent shutdown. On Unix, issues SIGTERM/SIGKILL escalation
    /// against each captured process ID before cancelling the token. Each cancelled
    /// run emits a `ToolEvent::Completed { success: false }` event.
    pub async fn shutdown(&self) {
        use std::sync::atomic::Ordering;

        self.shutting_down.store(true, Ordering::Release);

        let handles: Vec<(RunId, String, CancellationToken, Option<u32>)> = {
            let runs = self.background_runs.lock();
            runs.iter()
                .map(|(id, h)| (*id, h.command.clone(), h.abort.clone(), h.child_pid))
                .collect()
        };

        if handles.is_empty() {
            return;
        }

        tracing::info!(
            count = handles.len(),
            "cancelling background shell runs for shutdown"
        );

        for (run_id, command, abort, pid_opt) in &handles {
            abort.cancel();

            #[cfg(unix)]
            if let Some(pid) = pid_opt {
                send_signal_with_escalation(*pid).await;
            }

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

/// Drive a background shell run from spawn to completion.
///
/// This function is the body of the [`tokio::spawn`] task created by
/// [`ShellExecutor::spawn_background`]. It is extracted into a named async fn so
/// the spawner stays within the 100-line limit enforced by `clippy::too_many_lines`.
///
/// The child process is spawned here (not in the caller) so its PID can be written
/// back into the [`BackgroundHandle`] registry before the stream loop starts. This
/// makes the SIGTERM/SIGKILL escalation path in [`ShellExecutor::shutdown`] reachable.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run_background_task(
    run_id: RunId,
    command: String,
    timeout: Duration,
    abort: CancellationToken,
    background_runs: Arc<Mutex<HashMap<RunId, BackgroundHandle>>>,
    tool_event_tx: Option<ToolEventTx>,
    background_completion_tx: Option<tokio::sync::mpsc::Sender<BackgroundCompletion>>,
    skill_env_snapshot: Option<std::collections::HashMap<String, String>>,
    env_blocklist: Vec<String>,
) {
    use std::process::Stdio;

    let started_at = std::time::Instant::now();

    // Build and spawn the child directly so we can capture its PID and write it
    // back into the registry before entering the stream loop. Calling execute_bash
    // would hide the child handle and leave child_pid = None, making the
    // SIGTERM/SIGKILL escalation path in shutdown() unreachable.
    let mut cmd = build_bash_command(&command, skill_env_snapshot.as_ref(), &env_blocklist);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(ref e) => {
            let (_, out) = spawn_error_envelope(e);
            background_runs.lock().remove(&run_id);
            emit_completed(tool_event_tx.as_ref(), &command, out.clone(), false, run_id).await;
            if let Some(ref tx) = background_completion_tx {
                let _ = tx
                    .send(BackgroundCompletion {
                        run_id,
                        exit_code: 1,
                        output: out,
                        success: false,
                        elapsed_ms: 0,
                        command,
                    })
                    .await;
            }
            return;
        }
    };

    // Write PID back so shutdown() can reach the SIGTERM/SIGKILL escalation path.
    if let Some(pid) = child.id()
        && let Some(handle) = background_runs.lock().get_mut(&run_id)
    {
        handle.child_pid = Some(pid);
    }

    // stdout/stderr are guaranteed piped — set above before spawn.
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let mut line_rx = spawn_output_readers(stdout, stderr);

    let mut combined = String::new();
    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();
    let deadline = tokio::time::Instant::now() + timeout;
    let timeout_secs = timeout.as_secs();

    let (_, out) = match run_bash_stream(
        &command,
        deadline,
        Some(&abort),
        tool_event_tx.as_ref(),
        &mut line_rx,
        &mut combined,
        &mut stdout_buf,
        &mut stderr_buf,
        &mut child,
    )
    .await
    {
        BashLoopOutcome::TimedOut => (
            ShellOutputEnvelope {
                stdout: stdout_buf,
                stderr: format!("{stderr_buf}command timed out after {timeout_secs}s"),
                exit_code: 1,
                truncated: false,
            },
            format!("[error] command timed out after {timeout_secs}s"),
        ),
        BashLoopOutcome::Cancelled => (
            ShellOutputEnvelope {
                stdout: stdout_buf,
                stderr: format!("{stderr_buf}operation aborted"),
                exit_code: 130,
                truncated: false,
            },
            "[cancelled] operation aborted".to_string(),
        ),
        BashLoopOutcome::StreamClosed => {
            finalize_envelope(&mut child, combined, stdout_buf, stderr_buf).await
        }
    };

    #[allow(clippy::cast_possible_truncation)]
    let elapsed_ms = started_at.elapsed().as_millis() as u64;
    let success = !out.contains("[error]");
    let exit_code = i32::from(!success);
    let truncated = crate::executor::truncate_tool_output_at(&out, 4096);

    background_runs.lock().remove(&run_id);
    emit_completed(
        tool_event_tx.as_ref(),
        &command,
        truncated.clone(),
        success,
        run_id,
    )
    .await;

    if let Some(ref tx) = background_completion_tx {
        let completion = BackgroundCompletion {
            run_id,
            exit_code,
            output: truncated,
            success,
            elapsed_ms,
            command,
        };
        if tx.send(completion).await.is_err() {
            tracing::warn!(
                run_id = %run_id,
                "background completion channel closed; agent may have shut down"
            );
        }
    }

    tracing::debug!(run_id = %run_id, exit_code, elapsed_ms, "background shell run completed");
}

/// Like [`run_background_task`] but uses a pre-resolved `env` and `cwd` from
/// `resolve_context` instead of reading `skill_env`/process-env at spawn time.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run_background_task_with_env(
    run_id: RunId,
    command: String,
    timeout: Duration,
    abort: CancellationToken,
    background_runs: Arc<Mutex<HashMap<RunId, BackgroundHandle>>>,
    tool_event_tx: Option<ToolEventTx>,
    background_completion_tx: Option<tokio::sync::mpsc::Sender<BackgroundCompletion>>,
    env: HashMap<String, String>,
    cwd: PathBuf,
) {
    use std::process::Stdio;

    let started_at = std::time::Instant::now();

    let mut cmd = build_bash_command_with_context(&command, &env, &cwd);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(ref e) => {
            let (_, out) = spawn_error_envelope(e);
            background_runs.lock().remove(&run_id);
            emit_completed(tool_event_tx.as_ref(), &command, out.clone(), false, run_id).await;
            if let Some(ref tx) = background_completion_tx {
                let _ = tx
                    .send(BackgroundCompletion {
                        run_id,
                        exit_code: 1,
                        output: out,
                        success: false,
                        elapsed_ms: 0,
                        command,
                    })
                    .await;
            }
            return;
        }
    };

    if let Some(pid) = child.id()
        && let Some(handle) = background_runs.lock().get_mut(&run_id)
    {
        handle.child_pid = Some(pid);
    }

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let mut line_rx = spawn_output_readers(stdout, stderr);

    let mut combined = String::new();
    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();
    let deadline = tokio::time::Instant::now() + timeout;
    let timeout_secs = timeout.as_secs();

    let (_, out) = match run_bash_stream(
        &command,
        deadline,
        Some(&abort),
        tool_event_tx.as_ref(),
        &mut line_rx,
        &mut combined,
        &mut stdout_buf,
        &mut stderr_buf,
        &mut child,
    )
    .await
    {
        BashLoopOutcome::TimedOut => (
            ShellOutputEnvelope {
                stdout: stdout_buf,
                stderr: format!("{stderr_buf}command timed out after {timeout_secs}s"),
                exit_code: 1,
                truncated: false,
            },
            format!("[error] command timed out after {timeout_secs}s"),
        ),
        BashLoopOutcome::Cancelled => (
            ShellOutputEnvelope {
                stdout: stdout_buf,
                stderr: stderr_buf,
                exit_code: 130,
                truncated: false,
            },
            "[cancelled] operation aborted".to_string(),
        ),
        BashLoopOutcome::StreamClosed => {
            finalize_envelope(&mut child, combined, stdout_buf, stderr_buf).await
        }
    };

    #[allow(clippy::cast_possible_truncation)]
    let elapsed_ms = started_at.elapsed().as_millis() as u64;
    let success = !out.contains("[error]");
    let exit_code = i32::from(!success);
    let truncated = crate::executor::truncate_tool_output_at(&out, 4096);

    background_runs.lock().remove(&run_id);
    emit_completed(
        tool_event_tx.as_ref(),
        &command,
        truncated.clone(),
        success,
        run_id,
    )
    .await;

    if let Some(ref tx) = background_completion_tx {
        let completion = BackgroundCompletion {
            run_id,
            exit_code,
            output: truncated,
            success,
            elapsed_ms,
            command,
        };
        if tx.send(completion).await.is_err() {
            tracing::warn!(
                run_id = %run_id,
                "background completion channel closed; agent may have shut down"
            );
        }
    }

    tracing::debug!(run_id = %run_id, exit_code, elapsed_ms, "background shell run (with context) completed");
}

/// Emit a `ToolEvent::Completed` to `tool_event_tx` if it is set.
async fn emit_completed(
    tool_event_tx: Option<&ToolEventTx>,
    command: &str,
    output: String,
    success: bool,
    run_id: RunId,
) {
    if let Some(tx) = tool_event_tx {
        let _ = tx
            .send(ToolEvent::Completed {
                tool_name: ToolName::new("bash"),
                command: command.to_owned(),
                output,
                success,
                filter_stats: None,
                diff: None,
                run_id: Some(run_id),
            })
            .await;
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

/// Send SIGTERM to a process, wait [`GRACEFUL_TERM_MS`], then send SIGKILL.
///
/// `pkill -KILL -P <pid>` is issued before the final SIGKILL to reap any
/// child processes that bash may have spawned. Note: `pkill -P` sends SIGKILL
/// to the *children* of `pid`, not to `pid` itself.
///
/// **ESRCH on SIGKILL is safe and expected.** If the process exited voluntarily
/// during the grace period, the OS returns `ESRCH` ("no such process") for the
/// SIGKILL call; this is silently swallowed and not treated as an error.
///
/// **PID reuse caveat.** If bash exits during the 250 ms window and the OS
/// recycles its PID before `kill(SIGKILL)` is issued, the SIGKILL could
/// theoretically reach an unrelated process. In practice the 250 ms window is
/// too short for PID recycling under normal load, so this is treated as an
/// acceptable trade-off for MVP.
#[cfg(unix)]
async fn send_signal_with_escalation(pid: u32) {
    use nix::errno::Errno;
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    let Ok(pid_i32) = i32::try_from(pid) else {
        return;
    };
    let target = Pid::from_raw(pid_i32);

    if let Err(e) = kill(target, Signal::SIGTERM)
        && e != Errno::ESRCH
    {
        tracing::debug!(pid, err = %e, "SIGTERM failed");
    }
    tokio::time::sleep(GRACEFUL_TERM_MS).await;
    // Kill children of pid (not pid itself); ESRCH if none exist is harmless.
    let _ = Command::new("pkill")
        .args(["-KILL", "-P", &pid.to_string()])
        .status()
        .await;
    if let Err(e) = kill(target, Signal::SIGKILL)
        && e != Errno::ESRCH
    {
        tracing::debug!(pid, err = %e, "SIGKILL failed");
    }
}

/// Kill a child process and its descendants.
///
/// On Unix, sends SIGTERM first, waits [`GRACEFUL_TERM_MS`], reaps descendants,
/// then sends SIGKILL. Always finishes with [`tokio::process::Child::kill`] to
/// ensure the `Child` reaper sees the dead process.
async fn kill_process_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        send_signal_with_escalation(pid).await;
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

// Used only in cfg(test) blocks; dead_code analysis does not see test imports.
#[allow(dead_code)]
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

    let timeout_secs = timeout.as_secs();
    let mut cmd = build_bash_command(code, extra_env, env_blocklist);

    if let Err(envelope_err) = apply_sandbox(&mut cmd, sandbox) {
        return envelope_err;
    }

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(ref e) => return spawn_error_envelope(e),
    };

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let mut line_rx = spawn_output_readers(stdout, stderr);

    let mut combined = String::new();
    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();
    let deadline = tokio::time::Instant::now() + timeout;

    match run_bash_stream(
        code,
        deadline,
        cancel_token,
        event_tx,
        &mut line_rx,
        &mut combined,
        &mut stdout_buf,
        &mut stderr_buf,
        &mut child,
    )
    .await
    {
        BashLoopOutcome::TimedOut => {
            let msg = format!("[error] command timed out after {timeout_secs}s");
            (
                ShellOutputEnvelope {
                    stdout: stdout_buf,
                    stderr: format!("{stderr_buf}command timed out after {timeout_secs}s"),
                    exit_code: 1,
                    truncated: false,
                },
                msg,
            )
        }
        BashLoopOutcome::Cancelled => (
            ShellOutputEnvelope {
                stdout: stdout_buf,
                stderr: format!("{stderr_buf}operation aborted"),
                exit_code: 130,
                truncated: false,
            },
            "[cancelled] operation aborted".to_string(),
        ),
        BashLoopOutcome::StreamClosed => {
            finalize_envelope(&mut child, combined, stdout_buf, stderr_buf).await
        }
    }
}

fn build_bash_command(
    code: &str,
    extra_env: Option<&std::collections::HashMap<String, String>>,
    env_blocklist: &[String],
) -> Command {
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
    cmd
}

/// Build a `Command` using a pre-resolved env map and explicit cwd.
///
/// Clears the process env and applies only `resolved_env` — no blocklist re-apply needed
/// because the caller (`resolve_context`) has already done that.
fn build_bash_command_with_context(
    code: &str,
    resolved_env: &HashMap<String, String>,
    cwd: &std::path::Path,
) -> Command {
    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(code);
    cmd.env_clear();
    cmd.envs(resolved_env);
    cmd.current_dir(cwd);
    cmd
}

/// Execute `code` using a pre-resolved [`ResolvedContext`].
///
/// Unlike [`execute_bash`], this function receives the *final merged env* from
/// `resolve_context` and sets `current_dir` to the resolved CWD.
async fn execute_bash_with_context(
    code: &str,
    timeout: Duration,
    event_tx: Option<&ToolEventTx>,
    cancel_token: Option<&CancellationToken>,
    resolved: &ResolvedContext,
    sandbox: Option<(&dyn Sandbox, &SandboxPolicy)>,
) -> (ShellOutputEnvelope, String) {
    use std::process::Stdio;

    let timeout_secs = timeout.as_secs();
    let mut cmd = build_bash_command_with_context(code, &resolved.env, &resolved.cwd);

    if let Err(envelope_err) = apply_sandbox(&mut cmd, sandbox) {
        return envelope_err;
    }

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(ref e) => return spawn_error_envelope(e),
    };

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let mut line_rx = spawn_output_readers(stdout, stderr);

    let mut combined = String::new();
    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();
    let deadline = tokio::time::Instant::now() + timeout;

    match run_bash_stream(
        code,
        deadline,
        cancel_token,
        event_tx,
        &mut line_rx,
        &mut combined,
        &mut stdout_buf,
        &mut stderr_buf,
        &mut child,
    )
    .await
    {
        BashLoopOutcome::TimedOut => {
            let msg = format!("[error] command timed out after {timeout_secs}s");
            (
                ShellOutputEnvelope {
                    stdout: stdout_buf,
                    stderr: format!("{stderr_buf}command timed out after {timeout_secs}s"),
                    exit_code: 1,
                    truncated: false,
                },
                msg,
            )
        }
        BashLoopOutcome::Cancelled => (
            ShellOutputEnvelope {
                stdout: stdout_buf,
                stderr: format!("{stderr_buf}operation aborted"),
                exit_code: 130,
                truncated: false,
            },
            "[cancelled] operation aborted".to_string(),
        ),
        BashLoopOutcome::StreamClosed => {
            finalize_envelope(&mut child, combined, stdout_buf, stderr_buf).await
        }
    }
}

fn apply_sandbox(
    cmd: &mut Command,
    sandbox: Option<(&dyn Sandbox, &SandboxPolicy)>,
) -> Result<(), (ShellOutputEnvelope, String)> {
    // Apply OS sandbox before setting stdio so the rewritten program is sandboxed.
    if let Some((sb, policy)) = sandbox
        && let Err(err) = sb.wrap(cmd, policy)
    {
        let msg = format!("[error] sandbox setup failed: {err}");
        return Err((
            ShellOutputEnvelope {
                stdout: String::new(),
                stderr: msg.clone(),
                exit_code: 1,
                truncated: false,
            },
            msg,
        ));
    }
    Ok(())
}

fn spawn_error_envelope(e: &std::io::Error) -> (ShellOutputEnvelope, String) {
    let msg = format!("[error] {e}");
    (
        ShellOutputEnvelope {
            stdout: String::new(),
            stderr: msg.clone(),
            exit_code: 1,
            truncated: false,
        },
        msg,
    )
}

// Channel carries (is_stderr, line) so we can accumulate separate buffers
// while still building a combined interleaved string for streaming and LLM context.
fn spawn_output_readers(
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
) -> tokio::sync::mpsc::Receiver<(bool, String)> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let (line_tx, line_rx) = tokio::sync::mpsc::channel::<(bool, String)>(64);

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

    line_rx
}

/// Terminal condition of the streaming select loop.
///
/// `kill_process_tree` is called inside this function before returning `TimedOut`
/// or `Cancelled`, so the caller's envelope helpers can stay side-effect-free.
enum BashLoopOutcome {
    StreamClosed,
    TimedOut,
    Cancelled,
}

#[allow(clippy::too_many_arguments)]
async fn run_bash_stream(
    code: &str,
    deadline: tokio::time::Instant,
    cancel_token: Option<&CancellationToken>,
    event_tx: Option<&ToolEventTx>,
    line_rx: &mut tokio::sync::mpsc::Receiver<(bool, String)>,
    combined: &mut String,
    stdout_buf: &mut String,
    stderr_buf: &mut String,
    child: &mut tokio::process::Child,
) -> BashLoopOutcome {
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
                    None => return BashLoopOutcome::StreamClosed,
                }
            }
            () = tokio::time::sleep_until(deadline) => {
                kill_process_tree(child).await;
                return BashLoopOutcome::TimedOut;
            }
            () = async {
                match cancel_token {
                    Some(t) => t.cancelled().await,
                    None => std::future::pending().await,
                }
            } => {
                kill_process_tree(child).await;
                return BashLoopOutcome::Cancelled;
            }
        }
    }
}

async fn finalize_envelope(
    child: &mut tokio::process::Child,
    combined: String,
    stdout_buf: String,
    stderr_buf: String,
) -> (ShellOutputEnvelope, String) {
    let status = child.wait().await;
    let exit_code = status.ok().and_then(|s| s.code()).unwrap_or(1);

    if combined.is_empty() {
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
    }
}

#[cfg(test)]
mod tests;
