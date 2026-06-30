// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex as SyncMutex, RwLock as SyncRwLock};
use tokio_util::sync::CancellationToken;

use dashmap::DashMap;
use tokio::sync::RwLock;
use tokio::sync::{mpsc, watch};

type StatusTx = mpsc::UnboundedSender<String>;
/// Per-server trust config: (`trust_level`, `tool_allowlist`, `expected_tools`).
type ServerTrust =
    Arc<tokio::sync::RwLock<HashMap<String, (McpTrustLevel, Option<Vec<String>>, Vec<String>)>>>;

use rmcp::transport::auth::CredentialStore;

use crate::client::{McpClient, ToolRefreshEvent};
use crate::elicitation::ElicitationEvent;
use crate::embedding_guard::EmbeddingAnomalyGuard;
use crate::policy::PolicyEnforcer;
use crate::prober::DefaultMcpProber;
use crate::tool::{McpTool, ToolSecurityMeta};
use crate::trust_score::TrustScoreStore;

fn default_elicitation_timeout() -> u64 {
    120
}

/// Trust level for an MCP server connection.
///
/// Controls SSRF validation and tool filtering on connect and refresh.
pub(crate) use zeph_config::McpTrustLevel;

/// Maximum number of injection penalties applied per tool registration batch.
///
/// Caps the per-registration trust penalty at `MAX * INJECTION_PENALTY` to prevent
/// a single registration with many flagged descriptions (e.g. from false positives)
/// from permanently destroying server trust.
const MAX_INJECTION_PENALTIES_PER_REGISTRATION: usize = 3;

/// Transport type for MCP server connections.
#[non_exhaustive]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum McpTransport {
    /// Stdio: spawn child process with command + args.
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
    /// Streamable HTTP with optional static headers (already resolved, no vault refs).
    Http {
        url: String,
        /// Static headers injected into every request (e.g. `Authorization: Bearer <token>`).
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    /// OAuth 2.1 authenticated HTTP transport.
    OAuth {
        url: String,
        scopes: Vec<String>,
        callback_port: u16,
        client_name: String,
    },
}

/// Connection parameters for a single MCP server consumed by [`McpManager`].
///
/// Deserialized from the `[[mcp.servers]]` TOML config table or constructed
/// programmatically for tests. All fields except `id` and `transport` have
/// reasonable defaults via `#[serde(default)]`.
///
/// # Trust semantics
///
/// The combination of `trust_level`, `tool_allowlist`, and `expected_tools` controls
/// which tools are exposed to the agent:
///
/// - `Trusted` — all tools are exposed; SSRF and data-flow checks are relaxed.
/// - `Untrusted` + no allowlist — all tools exposed with a warning.
/// - `Untrusted` + allowlist — only listed tools are exposed.
/// - `Sandboxed` + allowlist — only listed tools; empty allowlist = no tools.
// `roots: Vec<rmcp::model::Root>` names a type deprecated by SEP-2577 (still functional —
// see `crate::roots`); the derive(Serialize, Deserialize) expansion below also references
// it, which a field-level `#[allow(deprecated)]` does not silence, hence the struct-level
// attribute.
#[allow(deprecated)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ServerEntry {
    pub id: String,
    pub transport: McpTransport,
    pub timeout: Duration,
    /// Trust level for this server. Controls SSRF validation and tool filtering.
    /// `Trusted` skips SSRF checks (for operator-controlled static config).
    #[serde(default)]
    pub trust_level: McpTrustLevel,
    /// Tool allowlist. `None` means no override (inherit from config or deny by default).
    /// `Some(vec![])` is an explicit empty list. See `McpTrustLevel` for per-level semantics.
    #[serde(default)]
    pub tool_allowlist: Option<Vec<String>>,
    /// Expected tool names for attestation. When non-empty, tools outside this
    /// list are filtered (Untrusted/Sandboxed) or warned (Trusted).
    #[serde(default)]
    pub expected_tools: Vec<String>,
    /// Filesystem roots to advertise to the server via `roots/list`.
    ///
    /// `rmcp::model::Root` is deprecated by SEP-2577 but still functional — see
    /// [`crate::roots`] for the construction-helper boundary that isolates this.
    #[serde(default)]
    pub roots: Vec<rmcp::model::Root>,
    /// Per-tool security metadata overrides. Keys are tool names.
    /// When absent for a tool, metadata is inferred from the tool name via heuristics.
    #[serde(default)]
    pub tool_metadata: HashMap<String, ToolSecurityMeta>,
    /// Whether this server is allowed to send elicitation requests.
    /// Overrides the global `elicitation_enabled` config.
    /// Sandboxed servers always have elicitation disabled regardless of this flag.
    #[serde(default)]
    pub elicitation_enabled: bool,
    /// Timeout in seconds for the user to respond to an elicitation request.
    #[serde(default = "default_elicitation_timeout")]
    pub elicitation_timeout_secs: u64,
    /// When `true`, spawn this Stdio server with an isolated environment: only the minimal
    /// base env vars (`PATH`, `HOME`, etc.) plus this server's declared `env` map are passed.
    ///
    /// Default: `false` (backward compatible).
    #[serde(default)]
    pub env_isolation: bool,
}

/// Configurable byte caps applied during tool ingestion and server-instructions storage.
#[derive(Debug, Clone, Copy)]
struct IngestLimits {
    description_bytes: usize,
    instructions_bytes: usize,
}

/// Owned output produced by a single [`McpManager::handle_connect_result`] call.
///
/// Accumulates the data that must be inserted into shared maps after all async work
/// completes, so write guards are never held across `.await` points.
struct ConnectOutput {
    /// `Some((server_id, client))` on success, `None` on failure.
    client_entry: Option<(String, McpClient)>,
    /// `Some((server_id, tools))` on success, `None` on failure.
    tools_entry: Option<(String, Vec<McpTool>)>,
    /// Flattened tool list to extend `all_tools` (empty on failure).
    tools: Vec<McpTool>,
    /// Per-server outcome (both success and failure).
    outcome: ServerConnectOutcome,
    /// `Some((server_id, truncated_instructions))` when the server sent instructions.
    instructions: Option<(String, String)>,
}

/// Outcome of a single server connection attempt from [`McpManager::connect_all`].
///
/// One `ServerConnectOutcome` is returned per configured server. Inspect `connected`
/// to distinguish success from failure; `error` is empty when `connected` is `true`.
#[derive(Debug, Clone)]
pub struct ServerConnectOutcome {
    /// Server ID from [`ServerEntry::id`].
    pub id: String,
    /// `true` if the connection and tool list retrieval succeeded.
    pub connected: bool,
    /// Number of tools registered after sanitization and trust filtering.
    pub tool_count: usize,
    /// Human-readable failure reason. Empty when `connected` is `true`.
    pub error: String,
}

/// Multi-server MCP lifecycle manager.
///
/// `McpManager` owns connections to all configured MCP servers. It drives the full
/// security pipeline (command allowlist, SSRF, attestation, sanitization, data-flow
/// policy, trust scoring, embedding anomaly detection) and exposes a single
/// `call_tool()` entry point for tool execution.
///
/// # Lifecycle
///
/// 1. Construct with [`McpManager::new`] (or [`McpManager::with_elicitation_capacity`]).
/// 2. Chain builder methods (`with_prober`, `with_trust_store`, `with_lock_tool_list`, …).
/// 3. Call [`McpManager::connect_all`] to establish connections; receives initial tool list.
/// 4. Call [`McpManager::spawn_refresh_task`] to start the background refresh handler.
/// 5. Use [`McpManager::call_tool`] to invoke tools during agent turns.
/// 6. Call [`McpManager::shutdown_all_shared`] on exit.
///
/// # Sharing across tasks
///
/// `McpManager` is cheaply cloneable via `Arc` wrapping of its internal maps, making it
/// safe to share across async tasks. Most methods take `&self`.
pub struct McpManager {
    configs: Vec<ServerEntry>,
    allowed_commands: Vec<String>,
    clients: Arc<RwLock<HashMap<String, McpClient>>>,
    connected_server_ids: SyncRwLock<HashSet<String>>,
    enforcer: Arc<PolicyEnforcer>,
    suppress_stderr: bool,
    /// Per-server tool lists; updated by the refresh task.
    server_tools: Arc<RwLock<HashMap<String, Vec<McpTool>>>>,
    /// Sender half of the refresh event channel; cloned into each `ToolListChangedHandler`.
    /// Wrapped in Mutex<Option<...>> so `shutdown_all_shared()` can drop it while holding `&self`.
    /// When this sender and all handler senders are dropped, the refresh task terminates.
    /// Bounded at 16: on `TrySendError::Full` the notification is dropped — latest-wins semantics.
    refresh_tx: SyncMutex<Option<mpsc::Sender<ToolRefreshEvent>>>,
    /// Receiver half; taken once by `spawn_refresh_task()`.
    refresh_rx: SyncMutex<Option<mpsc::Receiver<ToolRefreshEvent>>>,
    /// Broadcasts the full flattened tool list after any server refresh.
    tools_watch_tx: watch::Sender<Vec<McpTool>>,
    /// Shared rate-limit state across all `ToolListChangedHandler` instances.
    last_refresh: Arc<DashMap<String, Instant>>,
    /// Per-server OAuth credential stores. Keyed by server ID.
    /// Set via `with_oauth_credential_store` before `connect_all()`.
    oauth_credentials: HashMap<String, Arc<dyn CredentialStore>>,
    /// Optional status sender for OAuth authorization messages.
    /// When set, the authorization URL is sent as a status message instead of
    /// (or in addition to) printing to stderr — required for TUI and Telegram modes.
    status_tx: Option<StatusTx>,
    /// Per-server trust configuration for tool filtering.
    /// Behind `Arc<RwLock>` because refresh tasks read it from spawned closures
    /// and `add_server()` writes to it.
    server_trust: ServerTrust,
    /// Optional pre-connect prober. When set, called on every new server connection.
    prober: Option<DefaultMcpProber>,
    /// Optional persistent trust score store. When set, probe results are persisted.
    trust_store: Option<Arc<TrustScoreStore>>,
    /// Optional embedding anomaly guard. When set, called after every successful tool call.
    embedding_guard: Option<EmbeddingAnomalyGuard>,
    /// Per-server tool metadata overrides. Immutable after construction.
    server_tool_metadata: Arc<HashMap<String, HashMap<String, ToolSecurityMeta>>>,
    /// Configurable cap for tool description length (bytes). Default: 2048.
    max_description_bytes: usize,
    /// Configurable cap for server instructions length (bytes). Default: 2048.
    max_instructions_bytes: usize,
    /// Server instructions collected after handshake, keyed by server ID.
    server_instructions: Arc<RwLock<HashMap<String, String>>>,
    /// Sender half of the bounded elicitation event channel; cloned into each
    /// `ToolListChangedHandler` that has elicitation enabled.
    elicitation_tx: SyncMutex<Option<mpsc::Sender<ElicitationEvent>>>,
    /// Receiver half; taken once by `take_elicitation_rx()` and wired into the agent loop.
    elicitation_rx: SyncMutex<Option<mpsc::Receiver<ElicitationEvent>>>,
    /// Per-server elicitation enabled flags (populated from `ServerEntry`).
    server_elicitation: HashMap<String, bool>,
    /// Per-server elicitation timeout in seconds.
    server_elicitation_timeout: HashMap<String, u64>,
    /// Serializes all add/remove operations to prevent the `commit_added_server` + `remove_server` race.
    ///
    /// Without this lock a concurrent `remove_server` could remove a client from `clients` after
    /// `commit_added_server` releases the `clients` guard but before it writes to `server_trust`
    /// and `server_tools`, leaving orphaned trust/tools entries that persist until restart.
    add_remove_lock: tokio::sync::Mutex<()>,
    /// Cancellation token broadcast to all in-flight startup retry tasks.
    ///
    /// Cancelled in `shutdown_all_shared` before any other shutdown work so that retry
    /// sleeps are interrupted immediately rather than contributing tail latency.
    shutdown_token: CancellationToken,
    /// Maximum number of connection attempts per server at startup.
    ///
    /// `1` = no retry, `3` = two retries. Validated at config-parse time: `1..=10`.
    max_connect_attempts: u8,
    /// Base delay in milliseconds for exponential backoff between startup retry attempts.
    ///
    /// The actual delay is `min(startup_retry_backoff_ms * 2^(attempt-1), 8_000) ms`.
    /// Default: 1 000 ms.
    startup_retry_backoff_ms: u64,
    /// Per-call timeout applied to each `tools/call` request after connection is established.
    ///
    /// When `Some`, overrides the per-server `ServerEntry.timeout` for tool calls only.
    /// When `None`, the per-server `ServerEntry.timeout` is used for all operations.
    /// Default: `None` (uses per-server timeout).
    tool_timeout_secs: Option<u64>,
    /// When `true`, `tools/list_changed` refresh events are rejected for servers whose
    /// initial tool list has been committed (i.e. their ID is in `tool_list_locked`).
    ///
    /// This prevents a server from smuggling new tools mid-session after attestation.
    lock_tool_list: bool,
    /// Set of server IDs whose tool lists are locked. A server is added here atomically
    /// before `connect_entry` is called so the lock is in place before the server can
    /// send a `tools/list_changed` notification (MF-2: no TOCTOU window).
    tool_list_locked: Arc<DashMap<String, ()>>,
}

impl std::fmt::Debug for McpManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpManager")
            .field("server_count", &self.configs.len())
            .finish_non_exhaustive()
    }
}

/// Always sanitizes first (security invariant), then assigns security metadata,
/// then runs attestation against `expected_tools`, then applies allowlist filtering.
///
/// Returns the filtered tool list and the sanitization result (for injection feedback).
// TODO(critic): ingest_tools has both too_many_arguments and too_many_lines
// suppressed; not in scope for #3451. File a separate issue to decompose
// with an IngestParams struct.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
// complex algorithm function; both suppressions justified until the function is decomposed in a future refactor
/// Configuration bundle passed to [`ingest_tools`].
///
/// Consolidates all per-server policy parameters so call sites pass a single
/// reference instead of eight positional arguments.
struct IngestConfig<'a> {
    /// Stable identifier of the MCP server being ingested.
    server_id: &'a str,
    /// Trust classification that governs allowlist and attestation enforcement.
    trust_level: McpTrustLevel,
    /// Explicit tool allow-list from operator config (`None` = not configured).
    allowlist: Option<&'a [String]>,
    /// Operator-declared set of expected tool names used for attestation.
    expected_tools: &'a [String],
    /// Channel for surfacing warnings to the user-facing status bar.
    status_tx: Option<&'a StatusTx>,
    /// Maximum byte length for tool descriptions; longer descriptions are truncated.
    max_description_bytes: usize,
    /// Per-tool security metadata overrides keyed by tool name.
    tool_metadata: &'a HashMap<String, ToolSecurityMeta>,
}

mod builder;
mod call;
mod connect;
mod ingest;
mod retry;
mod server;

#[cfg(test)]
mod tests;
