// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt;

use zeph_common::ToolName;

use crate::shell::background::RunId;

/// Data for rendering file diffs in the TUI.
///
/// Produced by [`ShellExecutor`](crate::ShellExecutor) and [`FileExecutor`](crate::FileExecutor)
/// when a tool call modifies a tracked file. The TUI uses this to display a side-by-side diff.
#[derive(Debug, Clone)]
pub struct DiffData {
    /// Relative or absolute path to the file that was modified.
    pub file_path: String,
    /// File content before the tool executed.
    pub old_content: String,
    /// File content after the tool executed.
    pub new_content: String,
}

/// Structured tool invocation from LLM.
///
/// Produced by the agent loop when the LLM emits a structured tool call (as opposed to
/// a legacy fenced code block). Dispatched to [`ToolExecutor::execute_tool_call`].
///
/// # Example
///
/// ```rust
/// use zeph_tools::{ToolCall, ExecutionContext};
/// use zeph_common::ToolName;
///
/// let call = ToolCall {
///     tool_id: ToolName::new("bash"),
///     params: {
///         let mut m = serde_json::Map::new();
///         m.insert("command".to_owned(), serde_json::Value::String("echo hello".to_owned()));
///         m
///     },
///     caller_id: Some("user-42".to_owned()),
///     context: Some(ExecutionContext::new().with_name("repo")),
/// };
/// assert_eq!(call.tool_id, "bash");
/// ```
#[derive(Debug, Clone)]
pub struct ToolCall {
    /// The tool identifier, matching a value from [`ToolExecutor::tool_definitions`].
    pub tool_id: ToolName,
    /// JSON parameters for the tool call, deserialized into the tool's parameter struct.
    pub params: serde_json::Map<String, serde_json::Value>,
    /// Opaque caller identifier propagated from the channel (user ID, session ID, etc.).
    /// `None` for system-initiated calls (scheduler, self-learning, internal).
    pub caller_id: Option<String>,
    /// Per-turn execution environment. `None` means use the executor default (process CWD
    /// and inherited env), which is identical to the behaviour before this field existed.
    pub context: Option<crate::ExecutionContext>,
}

/// Cumulative filter statistics for a single tool execution.
///
/// Populated by [`ShellExecutor`](crate::ShellExecutor) when output filters are configured.
/// Displayed in the TUI to show how much output was compacted before being sent to the LLM.
#[derive(Debug, Clone, Default)]
pub struct FilterStats {
    /// Raw character count before filtering.
    pub raw_chars: usize,
    /// Character count after filtering.
    pub filtered_chars: usize,
    /// Raw line count before filtering.
    pub raw_lines: usize,
    /// Line count after filtering.
    pub filtered_lines: usize,
    /// Worst-case confidence across all applied filters.
    pub confidence: Option<crate::FilterConfidence>,
    /// The shell command that produced this output, for display purposes.
    pub command: Option<String>,
    /// Zero-based line indices that were kept after filtering.
    pub kept_lines: Vec<usize>,
}

impl FilterStats {
    /// Returns the percentage of characters removed by filtering.
    ///
    /// Returns `0.0` when there was no raw output to filter.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn savings_pct(&self) -> f64 {
        if self.raw_chars == 0 {
            return 0.0;
        }
        (1.0 - self.filtered_chars as f64 / self.raw_chars as f64) * 100.0
    }

    /// Estimates the number of LLM tokens saved by filtering.
    ///
    /// Uses the 4-chars-per-token approximation. Suitable for logging and metrics,
    /// not for billing or exact budget calculations.
    #[must_use]
    pub fn estimated_tokens_saved(&self) -> usize {
        self.raw_chars.saturating_sub(self.filtered_chars) / 4
    }

    /// Formats a one-line filter summary for log messages and TUI status.
    ///
    /// # Example
    ///
    /// ```rust
    /// use zeph_tools::FilterStats;
    ///
    /// let stats = FilterStats {
    ///     raw_chars: 1000,
    ///     filtered_chars: 400,
    ///     raw_lines: 50,
    ///     filtered_lines: 20,
    ///     command: Some("cargo build".to_owned()),
    ///     ..Default::default()
    /// };
    /// let summary = stats.format_inline("shell");
    /// assert!(summary.contains("60.0% filtered"));
    /// ```
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

/// Provenance of a tool execution result.
///
/// Set by each executor at `ToolOutput` construction time. Used by the sanitizer bridge
/// in `zeph-core` to select the appropriate `ContentSourceKind` and trust level.
/// `None` means the source is unspecified (pass-through code, mocks, tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimSource {
    /// Local shell command execution.
    Shell,
    /// Local file system read/write.
    FileSystem,
    /// HTTP web scrape.
    WebScrape,
    /// MCP server tool response.
    Mcp,
    /// A2A agent message.
    A2a,
    /// Code search (LSP or semantic).
    CodeSearch,
    /// Agent diagnostics (internal).
    Diagnostics,
    /// Memory retrieval (semantic search).
    Memory,
}

/// Structured result from tool execution.
///
/// Returned by every [`ToolExecutor`] implementation on success. The agent loop uses
/// [`ToolOutput::summary`] as the tool result text injected into the LLM context.
///
/// # Example
///
/// ```rust
/// use zeph_tools::{ToolOutput, executor::ClaimSource};
/// use zeph_common::ToolName;
///
/// let output = ToolOutput {
///     tool_name: ToolName::new("shell"),
///     summary: "hello\n".to_owned(),
///     blocks_executed: 1,
///     filter_stats: None,
///     diff: None,
///     streamed: false,
///     terminal_id: None,
///     locations: None,
///     raw_response: None,
///     claim_source: Some(ClaimSource::Shell),
/// };
/// assert_eq!(output.to_string(), "hello\n");
/// ```
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// Name of the tool that produced this output (e.g. `"shell"`, `"web-scrape"`).
    pub tool_name: ToolName,
    /// Human-readable result text injected into the LLM context.
    pub summary: String,
    /// Number of code blocks processed in this invocation.
    pub blocks_executed: u32,
    /// Output filter statistics when filtering was applied, `None` otherwise.
    pub filter_stats: Option<FilterStats>,
    /// File diff data for TUI display when the tool modified a tracked file.
    pub diff: Option<DiffData>,
    /// Whether this tool already streamed its output via `ToolEvent` channel.
    pub streamed: bool,
    /// Terminal ID when the tool was executed via IDE terminal (ACP terminal/* protocol).
    pub terminal_id: Option<String>,
    /// File paths touched by this tool call, for IDE follow-along (e.g. `ToolCallLocation`).
    pub locations: Option<Vec<String>>,
    /// Structured tool response payload for ACP intermediate `tool_call_update` notifications.
    pub raw_response: Option<serde_json::Value>,
    /// Provenance of this tool result. Set by the executor at construction time.
    /// `None` in pass-through wrappers, mocks, and tests.
    pub claim_source: Option<ClaimSource>,
}

impl fmt::Display for ToolOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.summary)
    }
}

/// Maximum characters of tool output injected into the LLM context without truncation.
///
/// Output that exceeds this limit is split into a head and tail via [`truncate_tool_output`]
/// to keep both the beginning and end of large command outputs.
pub const MAX_TOOL_OUTPUT_CHARS: usize = 30_000;

/// Truncate tool output that exceeds [`MAX_TOOL_OUTPUT_CHARS`] using a head+tail split.
///
/// Equivalent to `truncate_tool_output_at(output, MAX_TOOL_OUTPUT_CHARS)`.
///
/// # Example
///
/// ```rust
/// use zeph_tools::executor::truncate_tool_output;
///
/// let short = "hello world";
/// assert_eq!(truncate_tool_output(short), short);
/// ```
#[must_use]
pub fn truncate_tool_output(output: &str) -> String {
    truncate_tool_output_at(output, MAX_TOOL_OUTPUT_CHARS)
}

/// Truncate tool output that exceeds `max_chars` using a head+tail split.
///
/// Preserves the first and last `max_chars / 2` characters and inserts a truncation
/// marker in the middle. Both boundaries are snapped to valid UTF-8 character boundaries.
///
/// # Example
///
/// ```rust
/// use zeph_tools::executor::truncate_tool_output_at;
///
/// let long = "a".repeat(200);
/// let truncated = truncate_tool_output_at(&long, 100);
/// assert!(truncated.contains("truncated"));
/// assert!(truncated.len() < long.len());
/// ```
#[must_use]
pub fn truncate_tool_output_at(output: &str, max_chars: usize) -> String {
    if output.len() <= max_chars {
        return output.to_string();
    }

    let half = max_chars / 2;
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
///
/// Sent over the [`ToolEventTx`] channel to the TUI or channel adapter.
/// Each event variant corresponds to a phase in the tool execution lifecycle.
#[derive(Debug, Clone)]
pub enum ToolEvent {
    /// The tool has started. Displayed in the TUI as a spinner with the command text.
    Started {
        tool_name: ToolName,
        command: String,
        /// Active sandbox profile, if any. `None` when sandbox is disabled.
        sandbox_profile: Option<String>,
        /// Canonical absolute working directory the command will run in.
        /// `None` for executors that do not resolve a per-turn CWD.
        resolved_cwd: Option<String>,
        /// Name of the resolved execution environment (from `[[execution.environments]]`),
        /// or `None` when no named environment was selected.
        execution_env: Option<String>,
    },
    /// A chunk of streaming output was produced (e.g. from a long-running command).
    OutputChunk {
        tool_name: ToolName,
        command: String,
        chunk: String,
    },
    /// The tool finished. Contains the full output and optional filter/diff data.
    Completed {
        tool_name: ToolName,
        command: String,
        /// Full output text (possibly filtered and truncated).
        output: String,
        /// `true` when the tool exited successfully, `false` on error.
        success: bool,
        filter_stats: Option<FilterStats>,
        diff: Option<DiffData>,
        /// Set when this completion belongs to a background run. `None` for blocking runs.
        run_id: Option<RunId>,
    },
    /// A transactional rollback was performed, restoring or deleting files.
    Rollback {
        tool_name: ToolName,
        command: String,
        /// Number of files restored to their pre-execution content.
        restored_count: usize,
        /// Number of files that did not exist before execution and were deleted.
        deleted_count: usize,
    },
}

/// Sender half of the bounded channel used to stream [`ToolEvent`]s to the UI.
///
/// Capacity is 1024 slots. Streaming variants (`OutputChunk`, `Started`) use
/// `try_send` and drop on full; terminal variants (`Completed`, `Rollback`) use
/// `send().await` to guarantee delivery.
///
/// Created via [`tokio::sync::mpsc::channel`] with capacity `TOOL_EVENT_CHANNEL_CAP`.
pub type ToolEventTx = tokio::sync::mpsc::Sender<ToolEvent>;

/// Receiver half matching [`ToolEventTx`].
pub type ToolEventRx = tokio::sync::mpsc::Receiver<ToolEvent>;

/// Bounded capacity for the tool-event channel.
pub const TOOL_EVENT_CHANNEL_CAP: usize = 1024;

/// Classifies a tool error as transient (retryable) or permanent (abort immediately).
///
/// Transient errors may succeed on retry (network blips, race conditions).
/// Permanent errors will not succeed regardless of retries (policy, bad args, not found).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum ErrorKind {
    Transient,
    Permanent,
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transient => f.write_str("transient"),
            Self::Permanent => f.write_str("permanent"),
        }
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

    #[error("operation cancelled")]
    Cancelled,

    #[error("invalid tool parameters: {message}")]
    InvalidParams { message: String },

    #[error("execution failed: {0}")]
    Execution(#[from] std::io::Error),

    /// HTTP or API error with status code for fine-grained classification.
    ///
    /// Used by `WebScrapeExecutor` and other HTTP-based tools to preserve the status
    /// code for taxonomy classification. Scope: HTTP tools only (MCP uses a separate path).
    #[error("HTTP error {status}: {message}")]
    Http { status: u16, message: String },

    /// Shell execution error with explicit exit code and pre-classified category.
    ///
    /// Used by `ShellExecutor` when the exit code or stderr content maps to a known
    /// taxonomy category (e.g., exit 126 → `PolicyBlocked`, exit 127 → `PermanentFailure`).
    /// Preserves the exit code for audit logging and the category for skill evolution.
    #[error("shell error (exit {exit_code}): {message}")]
    Shell {
        exit_code: i32,
        category: crate::error_taxonomy::ToolErrorCategory,
        message: String,
    },

    #[error("snapshot failed: {reason}")]
    SnapshotFailed { reason: String },

    /// Tool call rejected because the tool id is outside the active capability scope.
    ///
    /// Emitted by `ScopedToolExecutor` before any tool side-effect runs.
    /// The audit log records `error_category = "out_of_scope"`.
    // LLM isolation: task_type is never shown in the error message (P2-OutOfScope).
    #[error("tool call denied by policy")]
    OutOfScope {
        /// Fully-qualified tool id that was rejected.
        tool_id: String,
        /// Active task type at dispatch time, if any.
        task_type: Option<String>,
    },
}

impl ToolError {
    /// Fine-grained error classification using the 12-category taxonomy.
    ///
    /// Prefer `category()` over `kind()` for new code. `kind()` is preserved for
    /// backward compatibility and delegates to `category().error_kind()`.
    #[must_use]
    pub fn category(&self) -> crate::error_taxonomy::ToolErrorCategory {
        use crate::error_taxonomy::{ToolErrorCategory, classify_http_status, classify_io_error};
        match self {
            Self::Blocked { .. } | Self::SandboxViolation { .. } => {
                ToolErrorCategory::PolicyBlocked
            }
            Self::ConfirmationRequired { .. } => ToolErrorCategory::ConfirmationRequired,
            Self::Timeout { .. } => ToolErrorCategory::Timeout,
            Self::Cancelled => ToolErrorCategory::Cancelled,
            Self::InvalidParams { .. } => ToolErrorCategory::InvalidParameters,
            Self::Http { status, .. } => classify_http_status(*status),
            Self::Execution(io_err) => classify_io_error(io_err),
            Self::Shell { category, .. } => *category,
            Self::SnapshotFailed { .. } => ToolErrorCategory::PermanentFailure,
            Self::OutOfScope { .. } => ToolErrorCategory::PolicyBlocked,
        }
    }

    /// Coarse classification for backward compatibility. Delegates to `category().error_kind()`.
    ///
    /// For `Execution(io::Error)`, the classification inspects `io::Error::kind()`:
    /// - Transient: `TimedOut`, `WouldBlock`, `Interrupted`, `ConnectionReset`,
    ///   `ConnectionAborted`, `BrokenPipe` — these may succeed on retry.
    /// - Permanent: `NotFound`, `PermissionDenied`, `AlreadyExists`, and all other
    ///   I/O error kinds — retrying would waste time with no benefit.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        use crate::error_taxonomy::ToolErrorCategoryExt;
        self.category().error_kind()
    }
}

/// Deserialize tool call params from a `serde_json::Map<String, Value>` into a typed struct.
///
/// # Errors
///
/// Returns `ToolError::InvalidParams` when deserialization fails.
pub fn deserialize_params<T: serde::de::DeserializeOwned>(
    params: &serde_json::Map<String, serde_json::Value>,
) -> Result<T, ToolError> {
    let obj = serde_json::Value::Object(params.clone());
    serde_json::from_value(obj).map_err(|e| ToolError::InvalidParams {
        message: e.to_string(),
    })
}

/// Async trait for tool execution backends.
///
/// Implementations include [`ShellExecutor`](crate::ShellExecutor),
/// [`WebScrapeExecutor`](crate::WebScrapeExecutor), [`CompositeExecutor`](crate::CompositeExecutor),
/// and [`FileExecutor`](crate::FileExecutor).
///
/// # Contract
///
/// - [`execute`](ToolExecutor::execute) and [`execute_tool_call`](ToolExecutor::execute_tool_call)
///   return `Ok(None)` when the executor does not handle the given input — callers must not
///   treat `None` as an error.
/// - All methods must be `Send + Sync` and free of blocking I/O.
/// - Implementations must enforce their own security controls (blocklists, sandboxes, SSRF
///   protection) before executing any side-effectful operation.
/// - [`execute_confirmed`](ToolExecutor::execute_confirmed) and
///   [`execute_tool_call_confirmed`](ToolExecutor::execute_tool_call_confirmed) bypass
///   confirmation gates only — all other security controls remain active.
///
/// # Two Invocation Paths
///
/// **Legacy fenced blocks**: The agent loop passes the raw LLM response string to [`execute`](ToolExecutor::execute).
/// The executor parses ` ```bash ` or ` ```scrape ` blocks and executes each one.
///
/// **Structured tool calls**: The agent loop constructs a [`ToolCall`] from the LLM's
/// JSON tool-use response and dispatches it via [`execute_tool_call`](ToolExecutor::execute_tool_call).
/// This is the preferred path for new code.
///
/// # Example
///
/// ```rust
/// use zeph_tools::{ToolExecutor, ToolCall, ToolOutput, ToolError, executor::ClaimSource};
///
/// #[derive(Debug)]
/// struct EchoExecutor;
///
/// impl ToolExecutor for EchoExecutor {
///     async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
///         Ok(None) // not a fenced-block executor
///     }
///
///     async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
///         if call.tool_id != "echo" {
///             return Ok(None);
///         }
///         let text = call.params.get("text")
///             .and_then(|v| v.as_str())
///             .unwrap_or("")
///             .to_owned();
///         Ok(Some(ToolOutput {
///             tool_name: "echo".into(),
///             summary: text,
///             blocks_executed: 1,
///             filter_stats: None,
///             diff: None,
///             streamed: false,
///             terminal_id: None,
///             locations: None,
///             raw_response: None,
///             claim_source: None,
///         }))
///     }
/// }
/// ```
/// # TODO (G3 — deferred: Tower-style tool middleware stack)
///
/// Currently, cross-cutting concerns (audit logging, rate limiting, sandboxing, guardrails)
/// are scattered across individual executor implementations. The planned approach is a
/// composable middleware stack similar to Tower's `Service` trait:
///
/// ```text
/// AuditLayer::new(RateLimitLayer::new(SandboxLayer::new(ShellExecutor::new())))
/// ```
///
/// **Blocked by:** requires D2 (consolidating `ToolExecutor` + `ErasedToolExecutor` into one
/// object-safe trait). See critic review §S3 for the tradeoff between RPIT fast-path and
/// dynamic dispatch overhead before collapsing D2.
///
/// # TODO (D2 — deferred: consolidate `ToolExecutor` and `ErasedToolExecutor`)
///
/// Having two parallel traits creates duplication and confusion. The blanket impl
/// `impl<T: ToolExecutor> ErasedToolExecutor for T` works but every new method must be
/// added to both traits. Use `trait_variant::make` or a single object-safe design.
///
/// **Blocked by:** need to benchmark the RPIT fast-path before removing it. See critic §S3.
pub trait ToolExecutor: Send + Sync {
    /// Parse `response` for fenced tool blocks and execute them.
    ///
    /// Returns `Ok(None)` when no tool blocks are found in `response`.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when a block is found but execution fails (blocked command,
    /// sandbox violation, network error, timeout, etc.).
    fn execute(
        &self,
        response: &str,
    ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send;

    /// Execute bypassing confirmation checks (called after user approves).
    ///
    /// Security controls other than the confirmation gate remain active. Default
    /// implementation delegates to [`execute`](ToolExecutor::execute).
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] on execution failure.
    fn execute_confirmed(
        &self,
        response: &str,
    ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        self.execute(response)
    }

    /// Return the tool definitions this executor can handle.
    ///
    /// Used to populate the LLM's tool schema at context-assembly time.
    /// Returns an empty `Vec` by default (for executors that only handle fenced blocks).
    fn tool_definitions(&self) -> Vec<crate::registry::ToolDef> {
        vec![]
    }

    /// Execute a structured tool call. Returns `Ok(None)` if `call.tool_id` is not handled.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when the tool ID is handled but execution fails.
    fn execute_tool_call(
        &self,
        _call: &ToolCall,
    ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        std::future::ready(Ok(None))
    }

    /// Execute a structured tool call bypassing confirmation checks.
    ///
    /// Called after the user has explicitly approved the tool invocation.
    /// Default implementation delegates to [`execute_tool_call`](ToolExecutor::execute_tool_call).
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] on execution failure.
    fn execute_tool_call_confirmed(
        &self,
        call: &ToolCall,
    ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        self.execute_tool_call(call)
    }

    /// Inject environment variables for the currently active skill. No-op by default.
    ///
    /// Called by the agent loop before each turn when the active skill specifies env vars.
    /// Implementations that ignore this (e.g. `WebScrapeExecutor`) may leave the default.
    fn set_skill_env(&self, _env: Option<std::collections::HashMap<String, String>>) {}

    /// Set the effective trust level for the currently active skill. No-op by default.
    ///
    /// Trust level affects which operations are permitted (e.g. network access, file writes).
    fn set_effective_trust(&self, _level: crate::SkillTrustLevel) {}

    /// Whether the executor can safely retry this tool call on a transient error.
    ///
    /// Only idempotent operations (e.g. read-only HTTP GET) should return `true`.
    /// Shell commands and other non-idempotent operations must keep the default `false`
    /// to prevent double-execution of side-effectful commands.
    fn is_tool_retryable(&self, _tool_id: &str) -> bool {
        false
    }

    /// Whether a tool call can be safely dispatched speculatively (before the LLM finishes).
    ///
    /// Speculative execution requires the tool to be:
    /// 1. Idempotent — repeated execution with the same args produces the same result.
    /// 2. Side-effect-free or cheaply reversible.
    /// 3. Not subject to user confirmation (`needs_confirmation` must be false at call time).
    ///
    /// Default: `false` (safe). Override to `true` only for tools that satisfy all three
    /// properties. The engine additionally gates on trust level and confirmation status
    /// regardless of this flag.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_tools::ToolExecutor;
    ///
    /// struct ReadOnlyExecutor;
    /// impl ToolExecutor for ReadOnlyExecutor {
    ///     async fn execute(&self, _: &str) -> Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError> {
    ///         Ok(None)
    ///     }
    ///     fn is_tool_speculatable(&self, _tool_id: &str) -> bool {
    ///         true // read-only, idempotent
    ///     }
    /// }
    /// ```
    fn is_tool_speculatable(&self, _tool_id: &str) -> bool {
        false
    }

    /// Return `true` when `call` would require user confirmation before execution.
    ///
    /// This is a pure metadata/policy query — implementations must **not** execute the tool.
    /// Used by the speculative engine to gate dispatch without causing double side-effects.
    ///
    /// Default: `false`. Executors that enforce a confirmation policy (e.g. `TrustGateExecutor`)
    /// must override this to reflect their actual policy without executing the tool.
    fn requires_confirmation(&self, _call: &ToolCall) -> bool {
        false
    }
}

/// Object-safe erased version of [`ToolExecutor`] using boxed futures.
///
/// Because [`ToolExecutor`] uses `impl Future` return types, it is not object-safe and
/// cannot be used as `dyn ToolExecutor`. This trait provides the same interface with
/// `Pin<Box<dyn Future>>` returns, enabling dynamic dispatch.
///
/// Implemented automatically for all `T: ToolExecutor + 'static` via the blanket impl below.
/// Use [`DynExecutor`] or `Box<dyn ErasedToolExecutor>` when runtime polymorphism is needed.
pub trait ErasedToolExecutor: Send + Sync {
    fn execute_erased<'a>(
        &'a self,
        response: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>>;

    fn execute_confirmed_erased<'a>(
        &'a self,
        response: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>>;

    fn tool_definitions_erased(&self) -> Vec<crate::registry::ToolDef>;

    fn execute_tool_call_erased<'a>(
        &'a self,
        call: &'a ToolCall,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>>;

    fn execute_tool_call_confirmed_erased<'a>(
        &'a self,
        call: &'a ToolCall,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>>
    {
        // TrustGateExecutor overrides ToolExecutor::execute_tool_call_confirmed; the blanket
        // impl for T: ToolExecutor routes this call through it via execute_tool_call_confirmed_erased.
        // Other implementors fall back to execute_tool_call_erased (normal enforcement path).
        self.execute_tool_call_erased(call)
    }

    /// Inject environment variables for the currently active skill. No-op by default.
    fn set_skill_env(&self, _env: Option<std::collections::HashMap<String, String>>) {}

    /// Set the effective trust level for the currently active skill. No-op by default.
    fn set_effective_trust(&self, _level: crate::SkillTrustLevel) {}

    /// Whether the executor can safely retry this tool call on a transient error.
    fn is_tool_retryable_erased(&self, tool_id: &str) -> bool;

    /// Whether a tool call can be safely dispatched speculatively.
    ///
    /// Default: `false`. Override to `true` in read-only executors.
    fn is_tool_speculatable_erased(&self, _tool_id: &str) -> bool {
        false
    }

    /// Return `true` when `call` would require user confirmation before execution.
    ///
    /// This is a pure metadata/policy query — implementations must **not** execute the tool.
    /// Used by the speculative engine to gate dispatch without causing double side-effects.
    ///
    /// Default: `true` (confirmation required). Implementors that want to allow speculative
    /// dispatch must explicitly return `false`. The blanket impl for `T: ToolExecutor`
    /// delegates to [`ToolExecutor::requires_confirmation`].
    fn requires_confirmation_erased(&self, _call: &ToolCall) -> bool {
        true
    }
}

impl<T: ToolExecutor> ErasedToolExecutor for T {
    fn execute_erased<'a>(
        &'a self,
        response: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>>
    {
        Box::pin(self.execute(response))
    }

    fn execute_confirmed_erased<'a>(
        &'a self,
        response: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>>
    {
        Box::pin(self.execute_confirmed(response))
    }

    fn tool_definitions_erased(&self) -> Vec<crate::registry::ToolDef> {
        self.tool_definitions()
    }

    fn execute_tool_call_erased<'a>(
        &'a self,
        call: &'a ToolCall,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>>
    {
        Box::pin(self.execute_tool_call(call))
    }

    fn execute_tool_call_confirmed_erased<'a>(
        &'a self,
        call: &'a ToolCall,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>>
    {
        Box::pin(self.execute_tool_call_confirmed(call))
    }

    fn set_skill_env(&self, env: Option<std::collections::HashMap<String, String>>) {
        ToolExecutor::set_skill_env(self, env);
    }

    fn set_effective_trust(&self, level: crate::SkillTrustLevel) {
        ToolExecutor::set_effective_trust(self, level);
    }

    fn is_tool_retryable_erased(&self, tool_id: &str) -> bool {
        ToolExecutor::is_tool_retryable(self, tool_id)
    }

    fn is_tool_speculatable_erased(&self, tool_id: &str) -> bool {
        ToolExecutor::is_tool_speculatable(self, tool_id)
    }

    fn requires_confirmation_erased(&self, call: &ToolCall) -> bool {
        ToolExecutor::requires_confirmation(self, call)
    }
}

/// Wraps `Arc<dyn ErasedToolExecutor>` so it can be used as a concrete `ToolExecutor`.
///
/// Enables dynamic composition of tool executors at runtime without static type chains.
pub struct DynExecutor(pub std::sync::Arc<dyn ErasedToolExecutor>);

impl ToolExecutor for DynExecutor {
    fn execute(
        &self,
        response: &str,
    ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        // Clone data to satisfy the 'static-ish bound: erased futures must not borrow self.
        let inner = std::sync::Arc::clone(&self.0);
        let response = response.to_owned();
        async move { inner.execute_erased(&response).await }
    }

    fn execute_confirmed(
        &self,
        response: &str,
    ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        let inner = std::sync::Arc::clone(&self.0);
        let response = response.to_owned();
        async move { inner.execute_confirmed_erased(&response).await }
    }

    fn tool_definitions(&self) -> Vec<crate::registry::ToolDef> {
        self.0.tool_definitions_erased()
    }

    fn execute_tool_call(
        &self,
        call: &ToolCall,
    ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        let inner = std::sync::Arc::clone(&self.0);
        let call = call.clone();
        async move { inner.execute_tool_call_erased(&call).await }
    }

    fn execute_tool_call_confirmed(
        &self,
        call: &ToolCall,
    ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        let inner = std::sync::Arc::clone(&self.0);
        let call = call.clone();
        async move { inner.execute_tool_call_confirmed_erased(&call).await }
    }

    fn set_skill_env(&self, env: Option<std::collections::HashMap<String, String>>) {
        ErasedToolExecutor::set_skill_env(self.0.as_ref(), env);
    }

    fn set_effective_trust(&self, level: crate::SkillTrustLevel) {
        ErasedToolExecutor::set_effective_trust(self.0.as_ref(), level);
    }

    fn is_tool_retryable(&self, tool_id: &str) -> bool {
        self.0.is_tool_retryable_erased(tool_id)
    }

    fn is_tool_speculatable(&self, tool_id: &str) -> bool {
        self.0.is_tool_speculatable_erased(tool_id)
    }

    fn requires_confirmation(&self, call: &ToolCall) -> bool {
        self.0.requires_confirmation_erased(call)
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

    let mut search_from = 0;
    while let Some(rel) = rest[search_from..].find(&marker) {
        let start = search_from + rel;
        let after = &rest[start + marker_len..];
        // Word-boundary check: the character immediately after the marker must be
        // whitespace, end-of-string, or a non-word character (not alphanumeric / _ / -).
        // This prevents "```bash" from matching "```bashrc".
        let boundary_ok = after
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_' && c != '-');
        if !boundary_ok {
            search_from = start + marker_len;
            continue;
        }
        if let Some(end) = after.find("```") {
            blocks.push(after[..end].trim());
            rest = &after[end + 3..];
            search_from = 0;
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
            tool_name: ToolName::new("bash"),
            summary: "$ echo hello\nhello".to_owned(),
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: None,
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
        let mut map = serde_json::Map::new();
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
        #[allow(dead_code)]
        struct P {
            name: String,
        }
        let map = serde_json::Map::new();
        let err = deserialize_params::<P>(&map).unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams { .. }));
    }

    #[test]
    fn deserialize_params_wrong_type() {
        #[derive(Debug, serde::Deserialize)]
        #[allow(dead_code)]
        struct P {
            count: u32,
        }
        let mut map = serde_json::Map::new();
        map.insert("count".to_owned(), serde_json::json!("not a number"));
        let err = deserialize_params::<P>(&map).unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams { .. }));
    }

    #[test]
    fn deserialize_params_all_optional_empty() {
        #[derive(Debug, serde::Deserialize, PartialEq)]
        struct P {
            name: Option<String>,
        }
        let map = serde_json::Map::new();
        let p: P = deserialize_params(&map).unwrap();
        assert_eq!(p, P { name: None });
    }

    #[test]
    fn deserialize_params_ignores_extra_fields() {
        #[derive(Debug, serde::Deserialize, PartialEq)]
        struct P {
            name: String,
        }
        let mut map = serde_json::Map::new();
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

    // ErrorKind classification tests
    #[test]
    fn error_kind_timeout_is_transient() {
        let err = ToolError::Timeout { timeout_secs: 30 };
        assert_eq!(err.kind(), ErrorKind::Transient);
    }

    #[test]
    fn error_kind_blocked_is_permanent() {
        let err = ToolError::Blocked {
            command: "rm -rf /".to_owned(),
        };
        assert_eq!(err.kind(), ErrorKind::Permanent);
    }

    #[test]
    fn error_kind_sandbox_violation_is_permanent() {
        let err = ToolError::SandboxViolation {
            path: "/etc/shadow".to_owned(),
        };
        assert_eq!(err.kind(), ErrorKind::Permanent);
    }

    #[test]
    fn error_kind_cancelled_is_permanent() {
        assert_eq!(ToolError::Cancelled.kind(), ErrorKind::Permanent);
    }

    #[test]
    fn error_kind_invalid_params_is_permanent() {
        let err = ToolError::InvalidParams {
            message: "bad arg".to_owned(),
        };
        assert_eq!(err.kind(), ErrorKind::Permanent);
    }

    #[test]
    fn error_kind_confirmation_required_is_permanent() {
        let err = ToolError::ConfirmationRequired {
            command: "rm /tmp/x".to_owned(),
        };
        assert_eq!(err.kind(), ErrorKind::Permanent);
    }

    #[test]
    fn error_kind_execution_timed_out_is_transient() {
        let io_err = std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout");
        assert_eq!(ToolError::Execution(io_err).kind(), ErrorKind::Transient);
    }

    #[test]
    fn error_kind_execution_interrupted_is_transient() {
        let io_err = std::io::Error::new(std::io::ErrorKind::Interrupted, "interrupted");
        assert_eq!(ToolError::Execution(io_err).kind(), ErrorKind::Transient);
    }

    #[test]
    fn error_kind_execution_connection_reset_is_transient() {
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        assert_eq!(ToolError::Execution(io_err).kind(), ErrorKind::Transient);
    }

    #[test]
    fn error_kind_execution_broken_pipe_is_transient() {
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe broken");
        assert_eq!(ToolError::Execution(io_err).kind(), ErrorKind::Transient);
    }

    #[test]
    fn error_kind_execution_would_block_is_transient() {
        let io_err = std::io::Error::new(std::io::ErrorKind::WouldBlock, "would block");
        assert_eq!(ToolError::Execution(io_err).kind(), ErrorKind::Transient);
    }

    #[test]
    fn error_kind_execution_connection_aborted_is_transient() {
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "aborted");
        assert_eq!(ToolError::Execution(io_err).kind(), ErrorKind::Transient);
    }

    #[test]
    fn error_kind_execution_not_found_is_permanent() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "not found");
        assert_eq!(ToolError::Execution(io_err).kind(), ErrorKind::Permanent);
    }

    #[test]
    fn error_kind_execution_permission_denied_is_permanent() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        assert_eq!(ToolError::Execution(io_err).kind(), ErrorKind::Permanent);
    }

    #[test]
    fn error_kind_execution_other_is_permanent() {
        let io_err = std::io::Error::other("some other error");
        assert_eq!(ToolError::Execution(io_err).kind(), ErrorKind::Permanent);
    }

    #[test]
    fn error_kind_execution_already_exists_is_permanent() {
        let io_err = std::io::Error::new(std::io::ErrorKind::AlreadyExists, "exists");
        assert_eq!(ToolError::Execution(io_err).kind(), ErrorKind::Permanent);
    }

    #[test]
    fn error_kind_display() {
        assert_eq!(ErrorKind::Transient.to_string(), "transient");
        assert_eq!(ErrorKind::Permanent.to_string(), "permanent");
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
            tool_id: ToolName::new("anything"),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,
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

    // DynExecutor tests

    struct FixedExecutor {
        tool_id: &'static str,
        output: &'static str,
    }

    impl ToolExecutor for FixedExecutor {
        async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(Some(ToolOutput {
                tool_name: ToolName::new(self.tool_id),
                summary: self.output.to_owned(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
            }))
        }

        fn tool_definitions(&self) -> Vec<crate::registry::ToolDef> {
            vec![]
        }

        async fn execute_tool_call(
            &self,
            _call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            Ok(Some(ToolOutput {
                tool_name: ToolName::new(self.tool_id),
                summary: self.output.to_owned(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
            }))
        }
    }

    #[tokio::test]
    async fn dyn_executor_execute_delegates() {
        let inner = std::sync::Arc::new(FixedExecutor {
            tool_id: "bash",
            output: "hello",
        });
        let exec = DynExecutor(inner);
        let result = exec.execute("```bash\necho hello\n```").await.unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().summary, "hello");
    }

    #[tokio::test]
    async fn dyn_executor_execute_confirmed_delegates() {
        let inner = std::sync::Arc::new(FixedExecutor {
            tool_id: "bash",
            output: "confirmed",
        });
        let exec = DynExecutor(inner);
        let result = exec.execute_confirmed("...").await.unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().summary, "confirmed");
    }

    #[test]
    fn dyn_executor_tool_definitions_delegates() {
        let inner = std::sync::Arc::new(FixedExecutor {
            tool_id: "my_tool",
            output: "",
        });
        let exec = DynExecutor(inner);
        // FixedExecutor returns empty definitions; verify delegation occurs without panic.
        let defs = exec.tool_definitions();
        assert!(defs.is_empty());
    }

    #[tokio::test]
    async fn dyn_executor_execute_tool_call_delegates() {
        let inner = std::sync::Arc::new(FixedExecutor {
            tool_id: "bash",
            output: "tool_call_result",
        });
        let exec = DynExecutor(inner);
        let call = ToolCall {
            tool_id: ToolName::new("bash"),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,
        };
        let result = exec.execute_tool_call(&call).await.unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().summary, "tool_call_result");
    }

    #[test]
    fn dyn_executor_set_effective_trust_delegates() {
        use std::sync::atomic::{AtomicU8, Ordering};

        struct TrustCapture(AtomicU8);
        impl ToolExecutor for TrustCapture {
            async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
                Ok(None)
            }
            fn set_effective_trust(&self, level: crate::SkillTrustLevel) {
                // encode: Trusted=0, Verified=1, Quarantined=2, Blocked=3
                let v = match level {
                    crate::SkillTrustLevel::Trusted => 0u8,
                    crate::SkillTrustLevel::Verified => 1,
                    crate::SkillTrustLevel::Quarantined => 2,
                    crate::SkillTrustLevel::Blocked => 3,
                };
                self.0.store(v, Ordering::Relaxed);
            }
        }

        let inner = std::sync::Arc::new(TrustCapture(AtomicU8::new(0)));
        let exec =
            DynExecutor(std::sync::Arc::clone(&inner) as std::sync::Arc<dyn ErasedToolExecutor>);
        ToolExecutor::set_effective_trust(&exec, crate::SkillTrustLevel::Quarantined);
        assert_eq!(inner.0.load(Ordering::Relaxed), 2);

        ToolExecutor::set_effective_trust(&exec, crate::SkillTrustLevel::Blocked);
        assert_eq!(inner.0.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn extract_fenced_blocks_no_prefix_match() {
        // ```bashrc must NOT match when searching for "bash"
        assert!(extract_fenced_blocks("```bashrc\nfoo\n```", "bash").is_empty());
        // exact match
        assert_eq!(
            extract_fenced_blocks("```bash\nfoo\n```", "bash"),
            vec!["foo"]
        );
        // trailing space is fine
        assert_eq!(
            extract_fenced_blocks("```bash \nfoo\n```", "bash"),
            vec!["foo"]
        );
    }

    // ── ToolError::category() delegation tests ────────────────────────────────

    #[test]
    fn tool_error_http_400_category_is_invalid_parameters() {
        use crate::error_taxonomy::ToolErrorCategory;
        let err = ToolError::Http {
            status: 400,
            message: "bad request".to_owned(),
        };
        assert_eq!(err.category(), ToolErrorCategory::InvalidParameters);
    }

    #[test]
    fn tool_error_http_401_category_is_policy_blocked() {
        use crate::error_taxonomy::ToolErrorCategory;
        let err = ToolError::Http {
            status: 401,
            message: "unauthorized".to_owned(),
        };
        assert_eq!(err.category(), ToolErrorCategory::PolicyBlocked);
    }

    #[test]
    fn tool_error_http_403_category_is_policy_blocked() {
        use crate::error_taxonomy::ToolErrorCategory;
        let err = ToolError::Http {
            status: 403,
            message: "forbidden".to_owned(),
        };
        assert_eq!(err.category(), ToolErrorCategory::PolicyBlocked);
    }

    #[test]
    fn tool_error_http_404_category_is_permanent_failure() {
        use crate::error_taxonomy::ToolErrorCategory;
        let err = ToolError::Http {
            status: 404,
            message: "not found".to_owned(),
        };
        assert_eq!(err.category(), ToolErrorCategory::PermanentFailure);
    }

    #[test]
    fn tool_error_http_429_category_is_rate_limited() {
        use crate::error_taxonomy::ToolErrorCategory;
        let err = ToolError::Http {
            status: 429,
            message: "too many requests".to_owned(),
        };
        assert_eq!(err.category(), ToolErrorCategory::RateLimited);
    }

    #[test]
    fn tool_error_http_500_category_is_server_error() {
        use crate::error_taxonomy::ToolErrorCategory;
        let err = ToolError::Http {
            status: 500,
            message: "internal server error".to_owned(),
        };
        assert_eq!(err.category(), ToolErrorCategory::ServerError);
    }

    #[test]
    fn tool_error_http_502_category_is_server_error() {
        use crate::error_taxonomy::ToolErrorCategory;
        let err = ToolError::Http {
            status: 502,
            message: "bad gateway".to_owned(),
        };
        assert_eq!(err.category(), ToolErrorCategory::ServerError);
    }

    #[test]
    fn tool_error_http_503_category_is_server_error() {
        use crate::error_taxonomy::ToolErrorCategory;
        let err = ToolError::Http {
            status: 503,
            message: "service unavailable".to_owned(),
        };
        assert_eq!(err.category(), ToolErrorCategory::ServerError);
    }

    #[test]
    fn tool_error_http_503_is_transient_triggers_phase2_retry() {
        // Phase 2 retry fires when err.kind() == ErrorKind::Transient.
        // Verify the full chain: Http{503} -> ServerError -> is_retryable() -> Transient.
        let err = ToolError::Http {
            status: 503,
            message: "service unavailable".to_owned(),
        };
        assert_eq!(
            err.kind(),
            ErrorKind::Transient,
            "HTTP 503 must be Transient so Phase 2 retry fires"
        );
    }

    #[test]
    fn tool_error_blocked_category_is_policy_blocked() {
        use crate::error_taxonomy::ToolErrorCategory;
        let err = ToolError::Blocked {
            command: "rm -rf /".to_owned(),
        };
        assert_eq!(err.category(), ToolErrorCategory::PolicyBlocked);
    }

    #[test]
    fn tool_error_sandbox_violation_category_is_policy_blocked() {
        use crate::error_taxonomy::ToolErrorCategory;
        let err = ToolError::SandboxViolation {
            path: "/etc/shadow".to_owned(),
        };
        assert_eq!(err.category(), ToolErrorCategory::PolicyBlocked);
    }

    #[test]
    fn tool_error_confirmation_required_category() {
        use crate::error_taxonomy::ToolErrorCategory;
        let err = ToolError::ConfirmationRequired {
            command: "rm /tmp/x".to_owned(),
        };
        assert_eq!(err.category(), ToolErrorCategory::ConfirmationRequired);
    }

    #[test]
    fn tool_error_timeout_category() {
        use crate::error_taxonomy::ToolErrorCategory;
        let err = ToolError::Timeout { timeout_secs: 30 };
        assert_eq!(err.category(), ToolErrorCategory::Timeout);
    }

    #[test]
    fn tool_error_cancelled_category() {
        use crate::error_taxonomy::ToolErrorCategory;
        assert_eq!(
            ToolError::Cancelled.category(),
            ToolErrorCategory::Cancelled
        );
    }

    #[test]
    fn tool_error_invalid_params_category() {
        use crate::error_taxonomy::ToolErrorCategory;
        let err = ToolError::InvalidParams {
            message: "missing field".to_owned(),
        };
        assert_eq!(err.category(), ToolErrorCategory::InvalidParameters);
    }

    // B2 regression: Execution(NotFound) must NOT produce ToolNotFound.
    #[test]
    fn tool_error_execution_not_found_category_is_permanent_failure() {
        use crate::error_taxonomy::ToolErrorCategory;
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "bash: not found");
        let err = ToolError::Execution(io_err);
        let cat = err.category();
        assert_ne!(
            cat,
            ToolErrorCategory::ToolNotFound,
            "Execution(NotFound) must NOT map to ToolNotFound"
        );
        assert_eq!(cat, ToolErrorCategory::PermanentFailure);
    }

    #[test]
    fn tool_error_execution_timed_out_category_is_timeout() {
        use crate::error_taxonomy::ToolErrorCategory;
        let io_err = std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out");
        assert_eq!(
            ToolError::Execution(io_err).category(),
            ToolErrorCategory::Timeout
        );
    }

    #[test]
    fn tool_error_execution_connection_refused_category_is_network_error() {
        use crate::error_taxonomy::ToolErrorCategory;
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        assert_eq!(
            ToolError::Execution(io_err).category(),
            ToolErrorCategory::NetworkError
        );
    }

    // B4 regression: Http/network/transient categories must NOT be quality failures.
    #[test]
    fn b4_tool_error_http_429_not_quality_failure() {
        let err = ToolError::Http {
            status: 429,
            message: "rate limited".to_owned(),
        };
        assert!(
            !err.category().is_quality_failure(),
            "RateLimited must not be a quality failure"
        );
    }

    #[test]
    fn b4_tool_error_http_503_not_quality_failure() {
        let err = ToolError::Http {
            status: 503,
            message: "service unavailable".to_owned(),
        };
        assert!(
            !err.category().is_quality_failure(),
            "ServerError must not be a quality failure"
        );
    }

    #[test]
    fn b4_tool_error_execution_timed_out_not_quality_failure() {
        let io_err = std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout");
        assert!(
            !ToolError::Execution(io_err).category().is_quality_failure(),
            "Timeout must not be a quality failure"
        );
    }

    // ── ToolError::Shell category tests ──────────────────────────────────────

    #[test]
    fn tool_error_shell_exit126_is_policy_blocked() {
        use crate::error_taxonomy::ToolErrorCategory;
        let err = ToolError::Shell {
            exit_code: 126,
            category: ToolErrorCategory::PolicyBlocked,
            message: "permission denied".to_owned(),
        };
        assert_eq!(err.category(), ToolErrorCategory::PolicyBlocked);
    }

    #[test]
    fn tool_error_shell_exit127_is_permanent_failure() {
        use crate::error_taxonomy::ToolErrorCategory;
        let err = ToolError::Shell {
            exit_code: 127,
            category: ToolErrorCategory::PermanentFailure,
            message: "command not found".to_owned(),
        };
        assert_eq!(err.category(), ToolErrorCategory::PermanentFailure);
        assert!(!err.category().is_retryable());
    }

    #[test]
    fn tool_error_shell_not_quality_failure() {
        use crate::error_taxonomy::ToolErrorCategory;
        let err = ToolError::Shell {
            exit_code: 127,
            category: ToolErrorCategory::PermanentFailure,
            message: "command not found".to_owned(),
        };
        // Shell exit errors are not attributable to LLM output quality.
        assert!(!err.category().is_quality_failure());
    }

    // ── requires_confirmation / requires_confirmation_erased tests (#3644) ───

    /// Stub implementing only `ToolExecutor` without overriding `requires_confirmation`.
    struct StubExecutor;
    impl ToolExecutor for StubExecutor {
        async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }
    }

    /// Stub that always signals confirmation is required via `ToolExecutor::requires_confirmation`.
    struct ConfirmingExecutor;
    impl ToolExecutor for ConfirmingExecutor {
        async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }
        fn requires_confirmation(&self, _call: &ToolCall) -> bool {
            true
        }
    }

    fn dummy_call() -> ToolCall {
        ToolCall {
            tool_id: ToolName::new("test"),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,
        }
    }

    #[test]
    fn requires_confirmation_default_is_false_on_tool_executor() {
        let exec = StubExecutor;
        assert!(
            !exec.requires_confirmation(&dummy_call()),
            "ToolExecutor default requires_confirmation must be false"
        );
    }

    #[test]
    fn requires_confirmation_erased_delegates_to_tool_executor_default() {
        // blanket impl routes erased → ToolExecutor::requires_confirmation (= false)
        let exec = StubExecutor;
        assert!(
            !ErasedToolExecutor::requires_confirmation_erased(&exec, &dummy_call()),
            "requires_confirmation_erased via blanket impl must return false for stub executor"
        );
    }

    #[test]
    fn requires_confirmation_erased_delegates_override() {
        // ConfirmingExecutor overrides requires_confirmation → true;
        // blanket impl must propagate this.
        let exec = ConfirmingExecutor;
        assert!(
            ErasedToolExecutor::requires_confirmation_erased(&exec, &dummy_call()),
            "requires_confirmation_erased must return true when ToolExecutor override returns true"
        );
    }

    #[test]
    fn requires_confirmation_erased_default_on_erased_trait_is_true() {
        // ErasedToolExecutor's own default (trait method body) returns true.
        // We construct a DynExecutor wrapping ConfirmingExecutor and verify via the erased path.
        // (We cannot instantiate ErasedToolExecutor directly without a concrete type.)
        // Instead verify via a type that only implements ErasedToolExecutor manually:
        struct ManualErased;
        impl ErasedToolExecutor for ManualErased {
            fn execute_erased<'a>(
                &'a self,
                _response: &'a str,
            ) -> std::pin::Pin<
                Box<dyn Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>,
            > {
                Box::pin(std::future::ready(Ok(None)))
            }
            fn execute_confirmed_erased<'a>(
                &'a self,
                _response: &'a str,
            ) -> std::pin::Pin<
                Box<dyn Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>,
            > {
                Box::pin(std::future::ready(Ok(None)))
            }
            fn tool_definitions_erased(&self) -> Vec<crate::registry::ToolDef> {
                vec![]
            }
            fn execute_tool_call_erased<'a>(
                &'a self,
                _call: &'a ToolCall,
            ) -> std::pin::Pin<
                Box<dyn Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>,
            > {
                Box::pin(std::future::ready(Ok(None)))
            }
            fn is_tool_retryable_erased(&self, _tool_id: &str) -> bool {
                false
            }
            // requires_confirmation_erased NOT overridden → trait default returns true
        }
        let exec = ManualErased;
        assert!(
            exec.requires_confirmation_erased(&dummy_call()),
            "ErasedToolExecutor trait-level default for requires_confirmation_erased must be true"
        );
    }

    // ── DynExecutor::requires_confirmation delegation tests (#3650) ──────────

    #[test]
    fn dyn_executor_requires_confirmation_delegates() {
        let inner = std::sync::Arc::new(ConfirmingExecutor);
        let exec =
            DynExecutor(std::sync::Arc::clone(&inner) as std::sync::Arc<dyn ErasedToolExecutor>);
        assert!(
            ToolExecutor::requires_confirmation(&exec, &dummy_call()),
            "DynExecutor must delegate requires_confirmation to inner executor"
        );
    }

    #[test]
    fn dyn_executor_requires_confirmation_default_false() {
        let inner = std::sync::Arc::new(StubExecutor);
        let exec =
            DynExecutor(std::sync::Arc::clone(&inner) as std::sync::Arc<dyn ErasedToolExecutor>);
        assert!(
            !ToolExecutor::requires_confirmation(&exec, &dummy_call()),
            "DynExecutor must return false when inner executor does not require confirmation"
        );
    }
}
