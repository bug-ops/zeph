// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! ACP agent implementation — session management and IDE capability proxying.
//!
//! [`ZephAcpAgentState`] manages multiple concurrent ACP sessions. Each session creates
//! an isolated agent loop via the [`AgentSpawner`] factory, runs it on a
//! [`LoopbackChannel`], and shuttles messages between the loop and the IDE over the ACP
//! connection. Use [`run_agent`] to drive the dispatch loop over a given transport.
//!
//! IDE capabilities (filesystem, terminal, LSP) are detected during `initialize()` and
//! surfaced to the agent loop through [`AcpContext`].

use std::path::{Component, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::{Mutex, RwLock};

use agent_client_protocol as acp;
use futures::StreamExt as _;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use zeph_core::channel::{ChannelMessage, LoopbackChannel, LoopbackHandle};
use zeph_core::text::truncate_to_chars;
use zeph_core::{LoopbackEvent, StopHint};
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider as _;
use zeph_mcp::McpManager;
use zeph_mcp::manager::ServerEntry;
use zeph_memory::ConversationId;
use zeph_memory::store::SqliteStore;

use tracing::Instrument as _;
use zeph_tools::is_private_ip;

use crate::fs::AcpFileExecutor;
use crate::lsp::DiagnosticsCache;
use crate::permission::AcpPermissionGate;
use crate::terminal::AcpShellExecutor;
use crate::transport::SharedAvailableModels;

/// Factory that creates a provider by `{provider}:{model}` key.
///
/// Called when the IDE sends `set_session_config_option` with a new model selection.
/// Returns `None` when the requested key is not recognized.
///
/// # Examples
///
/// ```rust,no_run
/// use std::sync::Arc;
/// use zeph_acp::agent::ProviderFactory;
///
/// let factory: ProviderFactory = Arc::new(|key| {
///     // key format: "openai:gpt-4o" or "ollama:llama3"
///     let _key = key;
///     None // return Some(provider) for known keys
/// });
/// ```
pub type ProviderFactory = Arc<dyn Fn(&str) -> Option<AnyProvider> + Send + Sync>;

/// Per-session context passed to the agent spawner.
///
/// Provides the session identity and persistence handles needed to bootstrap
/// an agent loop for an individual ACP session.
///
/// `conversation_id` is `Some` when a SQLite-backed [`ConversationId`] was
/// successfully created or retrieved for this session. `None` means the store
/// was unavailable at session creation time; the agent operates without
/// persistent history in that case.
pub struct SessionContext {
    /// ACP-assigned session identifier.
    pub session_id: acp::schema::SessionId,
    /// `SQLite` conversation ID for persisting message history, if available.
    pub conversation_id: Option<ConversationId>,
    /// Working directory reported by the IDE for this session.
    pub working_dir: PathBuf,
}

const MAX_PROMPT_BYTES: usize = 1_048_576; // 1 MiB
const MAX_IMAGE_BASE64_BYTES: usize = 20 * 1_048_576; // 20 MiB base64-encoded

const SUPPORTED_IMAGE_MIMES: &[&str] = &[
    "image/jpeg",
    "image/jpg",
    "image/png",
    "image/gif",
    "image/webp",
];
const LOOPBACK_CHANNEL_CAPACITY: usize = 64;
/// Maximum bytes fetched from an HTTP resource link.
const MAX_RESOURCE_BYTES: usize = 1_048_576; // 1 MiB
/// Timeout for HTTP resource link fetch.
const RESOURCE_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Pseudo-filesystem path components that expose secrets or kernel internals.
const BLOCKED_PATH_COMPONENTS: &[&str] = &["proc", "sys", "dev", ".ssh", ".gnupg", ".aws"];

/// Resolve a `ResourceLink` URI to its text content.
///
/// Supports `file://` and `http(s)://` URIs. Returns an error for unsupported
/// schemes or security violations (SSRF, path traversal, binary content).
///
/// `session_cwd` is used as the allowed root for `file://` URIs. Only paths
/// that are descendants of `session_cwd` are permitted.
async fn resolve_resource_link(
    link: &acp::schema::ResourceLink,
    session_cwd: &std::path::Path,
) -> Result<String, crate::error::AcpError> {
    let uri = &link.uri;

    if let Some(path_str) = uri.strip_prefix("file://") {
        // Canonicalize to resolve symlinks and `..` — single syscall, no TOCTOU.
        let path = std::path::Path::new(path_str);

        // Pre-check size to avoid loading large files into memory before rejection.
        let meta = tokio::time::timeout(RESOURCE_FETCH_TIMEOUT, tokio::fs::metadata(path))
            .await
            .map_err(|_| {
                crate::error::AcpError::ResourceLink(format!("file:// metadata timed out: {uri}"))
            })?
            .map_err(|e| {
                crate::error::AcpError::ResourceLink(format!("file:// stat failed: {e}"))
            })?;

        if meta.len() > MAX_RESOURCE_BYTES as u64 {
            return Err(crate::error::AcpError::ResourceLink(format!(
                "file:// content exceeds size limit ({MAX_RESOURCE_BYTES} bytes): {uri}"
            )));
        }

        let canonical = tokio::fs::canonicalize(path).await.map_err(|e| {
            crate::error::AcpError::ResourceLink(format!("file:// resolution failed: {e}"))
        })?;

        // Enforce cwd boundary: only files inside the session working directory are allowed.
        if !canonical.starts_with(session_cwd) {
            return Err(crate::error::AcpError::ResourceLink(format!(
                "file:// path outside session working directory: {uri}"
            )));
        }

        // Reject pseudo-filesystems and sensitive directories.
        for component in canonical.components() {
            if let Component::Normal(name) = component {
                let name_str = name.to_string_lossy();
                if BLOCKED_PATH_COMPONENTS
                    .iter()
                    .any(|blocked| name_str == *blocked)
                {
                    return Err(crate::error::AcpError::ResourceLink(format!(
                        "file:// path blocked: {uri}"
                    )));
                }
            }
        }

        let bytes = tokio::time::timeout(RESOURCE_FETCH_TIMEOUT, tokio::fs::read(&canonical))
            .await
            .map_err(|_| {
                crate::error::AcpError::ResourceLink(format!("file:// read timed out: {uri}"))
            })?
            .map_err(|e| {
                crate::error::AcpError::ResourceLink(format!("file:// read failed: {e}"))
            })?;

        // Reject binary files (null byte check — S-1).
        if bytes.contains(&0u8) {
            return Err(crate::error::AcpError::ResourceLink(format!(
                "binary file not supported as ResourceLink content: {uri}"
            )));
        }

        String::from_utf8(bytes).map_err(|_| {
            crate::error::AcpError::ResourceLink(format!(
                "file:// content is not valid UTF-8: {uri}"
            ))
        })
    } else if uri.starts_with("http://") || uri.starts_with("https://") {
        // No-redirect policy prevents redirect-based SSRF bypass.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(RESOURCE_FETCH_TIMEOUT)
            .build()
            .map_err(|e| crate::error::AcpError::ResourceLink(format!("HTTP client error: {e}")))?;

        let resp = client
            .get(uri.as_str())
            .header(reqwest::header::ACCEPT, "text/*")
            .send()
            .await
            .map_err(|e| crate::error::AcpError::ResourceLink(format!("HTTP fetch failed: {e}")))?;

        // Post-fetch IP check: eliminates DNS rebinding TOCTOU window (RC-1).
        // Fail-closed: if remote_addr() is unavailable (e.g. rustls), reject the response.
        match resp.remote_addr() {
            None => {
                return Err(crate::error::AcpError::ResourceLink(format!(
                    "SSRF check failed: remote address unavailable for {uri}"
                )));
            }
            Some(remote_addr) if is_private_ip(remote_addr.ip()) => {
                return Err(crate::error::AcpError::ResourceLink(format!(
                    "SSRF blocked: {uri} resolved to private address {remote_addr}"
                )));
            }
            Some(_) => {}
        }

        if !resp.status().is_success() {
            return Err(crate::error::AcpError::ResourceLink(format!(
                "HTTP fetch returned {}: {uri}",
                resp.status()
            )));
        }

        // Reject non-text content types.
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !content_type.is_empty() && !content_type.starts_with("text/") {
            return Err(crate::error::AcpError::ResourceLink(format!(
                "non-text MIME type rejected for ResourceLink: {content_type}"
            )));
        }

        // Stream up to MAX_RESOURCE_BYTES to avoid unbounded memory use.
        let mut body = resp.bytes_stream();
        let mut buf = Vec::with_capacity(4096);
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|e| {
                crate::error::AcpError::ResourceLink(format!("HTTP read error: {e}"))
            })?;
            if buf.len() + chunk.len() > MAX_RESOURCE_BYTES {
                buf.extend_from_slice(&chunk[..MAX_RESOURCE_BYTES.saturating_sub(buf.len())]);
                break;
            }
            buf.extend_from_slice(&chunk);
        }

        String::from_utf8(buf).map_err(|_| {
            crate::error::AcpError::ResourceLink(format!(
                "HTTP response body is not valid UTF-8: {uri}"
            ))
        })
    } else {
        Err(crate::error::AcpError::ResourceLink(format!(
            "unsupported URI scheme in ResourceLink: {uri}"
        )))
    }
}

/// IDE-proxied capabilities passed to the agent loop per session.
///
/// Each field is `None` when the IDE did not advertise the corresponding capability
/// during the ACP `initialize()` handshake. The agent loop should degrade gracefully
/// when optional capabilities are absent.
pub struct AcpContext {
    /// IDE-proxied filesystem executor (`fs.readTextFile` / `fs.writeTextFile`).
    ///
    /// `None` when the IDE did not advertise filesystem capability.
    pub file_executor: Option<AcpFileExecutor>,
    /// IDE-proxied shell executor (`terminal.create` / `terminal.execute`).
    ///
    /// `None` when the IDE did not advertise terminal capability.
    pub shell_executor: Option<AcpShellExecutor>,
    /// Permission gate for tool-call approval requests sent to the IDE.
    ///
    /// `None` when the IDE did not advertise permission capability.
    pub permission_gate: Option<AcpPermissionGate>,
    /// Shared cancellation signal.
    ///
    /// Notify this to interrupt the currently running agent operation (e.g. on user cancel).
    pub cancel_signal: std::sync::Arc<tokio::sync::Notify>,
    /// Shared slot for runtime model switching via `set_session_config_option`.
    ///
    /// When `Some`, the agent should swap its provider before the next turn.
    pub provider_override: Arc<RwLock<Option<AnyProvider>>>,
    /// Tool call ID of the parent agent's tool call that spawned this subagent session.
    ///
    /// `None` for top-level (non-subagent) sessions.
    pub parent_tool_use_id: Option<String>,
    /// LSP provider when the IDE advertised `meta["lsp"]` capability.
    ///
    /// `None` when the IDE does not support LSP extension methods.
    pub lsp_provider: Option<crate::lsp::AcpLspProvider>,
    /// Shared diagnostics cache — written by the LSP notification handler in `ZephAcpAgent`
    /// and read by the agent loop context builder to inject diagnostics into the system prompt.
    pub diagnostics_cache: Arc<RwLock<DiagnosticsCache>>,
}

/// Factory that receives a [`LoopbackChannel`], optional [`AcpContext`], and [`SessionContext`],
/// then drives the agent loop to completion.
///
/// Each invocation creates an independent agent with its own conversation history,
/// enabling true multi-session isolation. The future is `'static` but not `Send`
/// (`Agent<LoopbackChannel>` holds non-`Send` references across `.await`); scheduled
/// via `tokio::task::spawn_local` inside a `LocalSet`. The ACP transport runtime
/// (`serve_stdio`/`serve_connection`) already wraps the dispatcher in a `LocalSet`,
/// so handler code may call `spawn_local` directly without additional setup.
///
/// # Examples
///
/// ```rust,no_run
/// use std::sync::Arc;
/// use zeph_acp::{AgentSpawner, AcpContext, SessionContext};
/// use zeph_core::channel::LoopbackChannel;
///
/// let spawner: AgentSpawner = Arc::new(|channel, ctx, session| {
///     Box::pin(async move {
///         // drive your agent loop here
///         drop((channel, ctx, session));
///     })
/// });
/// ```
pub type AgentSpawner = Arc<
    dyn Fn(
            LoopbackChannel,
            Option<AcpContext>,
            SessionContext,
        ) -> Pin<Box<dyn std::future::Future<Output = ()> + 'static>>
        + Send
        + Sync
        + 'static,
>;

/// Thread-safe variant of [`AgentSpawner`] required by the HTTP transport.
///
/// Used with [`AcpHttpState`](crate::transport::http::AcpHttpState) to satisfy
/// `axum::State` requirements (`Send + Sync`). In practice this is the same type
/// alias — the distinction exists to make the intent clear at call sites.
#[cfg(feature = "acp-http")]
pub type SendAgentSpawner = AgentSpawner;

/// Sender half for delivering session notifications to the per-session drainer.
pub(crate) type NotifySender =
    mpsc::Sender<(acp::schema::SessionNotification, oneshot::Sender<()>)>;

/// Receiver half paired with [`NotifySender`].
pub(crate) type NotifyReceiver =
    mpsc::Receiver<(acp::schema::SessionNotification, oneshot::Sender<()>)>;

pub(crate) struct SessionEntry {
    pub(crate) input_tx: mpsc::Sender<ChannelMessage>,
    /// Receiver is owned solely by the `prompt()` handler.
    /// `Mutex` instead of `RefCell` so `SessionEntry` is `Send`.
    pub(crate) output_rx: Mutex<Option<mpsc::Receiver<LoopbackEvent>>>,
    pub(crate) cancel_signal: Arc<tokio::sync::Notify>,
    /// Epoch milliseconds; updated on every prompt.
    pub(crate) last_active_ms: AtomicU64,
    pub(crate) created_at: chrono::DateTime<chrono::Utc>,
    pub(crate) working_dir: Mutex<Option<std::path::PathBuf>>,
    /// Channel for sending notifications to the per-session drainer task.
    pub(crate) notify_tx: NotifySender,
    /// Receiver consumed by the drainer task spawned in `new_session` / `load_session`.
    /// Wrapped in `Mutex` so it can be `take()`-n exactly once.
    pub(crate) notify_rx: Mutex<Option<NotifyReceiver>>,
    /// Shared provider override slot; written by `set_session_config_option`, read by agent loop.
    provider_override: Arc<RwLock<Option<AnyProvider>>>,
    /// Currently selected model identifier (display / tracking only).
    current_model: Mutex<String>,
    /// Current session mode (ask / architect / code).
    current_mode: Mutex<acp::schema::SessionModeId>,
    /// Set after the first successful prompt so title generation fires only once.
    first_prompt_done: AtomicBool,
    /// Auto-generated session title; populated after first prompt via `SessionTitle` event.
    title: Mutex<Option<String>>,
    /// Whether extended thinking is enabled for this session.
    thinking_enabled: AtomicBool,
    /// Auto-approve level for this session ("suggest" | "auto-edit" | "full-auto").
    auto_approve_level: Mutex<String>,
    /// Shell executor for this session, retained so the event loop can release terminals
    /// after `tool_call_update` notifications are sent (ACP requires the terminal to
    /// remain alive until after the notification that embeds it).
    pub(crate) shell_executor: Option<AcpShellExecutor>,
    /// Message-id captured at the start of a `do_prompt` turn.
    ///
    /// The existing one-in-flight-prompt invariant (enforced by `output_rx.lock().take()` at
    /// line ~1142) guarantees at most one concurrent writer, so a plain `Mutex<Option<String>>`
    /// is sufficient without `parking_lot`.
    #[cfg(feature = "unstable-message-id")]
    pub(crate) current_message_id: std::sync::Mutex<Option<String>>,
}

impl SessionEntry {
    #[allow(dead_code)]
    fn last_active(&self) -> std::time::Instant {
        let ms = self.last_active_ms.load(Ordering::Relaxed);
        let now_ms = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        )
        .unwrap_or(u64::MAX);
        let elapsed_ms = now_ms.saturating_sub(ms);
        std::time::Instant::now()
            .checked_sub(std::time::Duration::from_millis(elapsed_ms))
            .unwrap_or_else(std::time::Instant::now)
    }

    fn touch(&self) {
        let ms = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        )
        .unwrap_or(u64::MAX);
        self.last_active_ms.store(ms, Ordering::Relaxed);
    }
}

type SessionMap = Arc<Mutex<std::collections::HashMap<acp::schema::SessionId, SessionEntry>>>;

/// ACP agent state shared across all connections.
///
/// Wraps session management, configuration, and per-session tool executors.
/// Pass an `Arc<ZephAcpAgentState>` to [`run_agent`] to drive the dispatch loop.
pub struct ZephAcpAgentState {
    pub(crate) spawner: AgentSpawner,
    pub(crate) sessions: SessionMap,
    pub(crate) agent_name: String,
    agent_version: String,
    max_sessions: usize,
    idle_timeout: std::time::Duration,
    pub(crate) store: Option<SqliteStore>,
    permission_file: Option<std::path::PathBuf>,
    /// IDE capabilities received during `initialize()`; used by `build_acp_context`.
    pub(crate) client_caps: RwLock<acp::schema::ClientCapabilities>,
    /// Factory for creating a new provider by `{provider}:{model}` key.
    pub(crate) provider_factory: Option<ProviderFactory>,
    /// Available model identifiers advertised in `new_session` `config_options`.
    available_models: SharedAvailableModels,
    /// Shared MCP manager for `ext_method` add/remove/list.
    pub(crate) mcp_manager: Option<Arc<McpManager>>,
    /// Project rule file paths advertised in `new_session` `_meta`.
    project_rules: Vec<std::path::PathBuf>,
    /// Maximum characters for auto-generated session titles.
    title_max_chars: usize,
    /// Maximum number of sessions returned by `list_sessions` (0 = unlimited).
    max_history: usize,
    /// LSP extension configuration (from `[acp.lsp]`).
    pub(crate) lsp_config: zeph_core::config::AcpLspConfig,
    /// Per-agent diagnostics cache, shared between the agent (writer) and `AcpContext` (reader).
    pub(crate) diagnostics_cache: Arc<RwLock<DiagnosticsCache>>,
    /// Cancellation token for the idle reaper task.
    reaper_cancel: CancellationToken,
    /// Canonicalized allowlist of directories ACP clients may reference in session requests.
    additional_directories_allow: Vec<std::path::PathBuf>,
    /// Auth methods to advertise in the `initialize` response. MVP: always `[Agent]`.
    auth_methods_config: Vec<zeph_core::config::AcpAuthMethod>,
    /// When `true`, echo `PromptRequest.message_id` through responses and chunks.
    message_ids_enabled: bool,
}

/// Backward-compatible alias.
pub type ZephAcpAgent = ZephAcpAgentState;

impl ZephAcpAgentState {
    pub fn new(
        spawner: AgentSpawner,
        max_sessions: usize,
        session_idle_timeout_secs: u64,
        permission_file: Option<std::path::PathBuf>,
    ) -> Self {
        let lsp_config = zeph_core::config::AcpLspConfig::default();
        let max_diag_files = lsp_config.max_diagnostic_files;
        Self {
            spawner,
            sessions: Arc::new(Mutex::new(std::collections::HashMap::new())),
            agent_name: "zeph".to_owned(),
            agent_version: env!("CARGO_PKG_VERSION").to_owned(),
            max_sessions,
            idle_timeout: std::time::Duration::from_secs(session_idle_timeout_secs),
            store: None,
            permission_file,
            client_caps: RwLock::new(acp::schema::ClientCapabilities::default()),
            provider_factory: None,
            available_models: Arc::new(RwLock::new(Vec::new())),
            mcp_manager: None,
            project_rules: Vec::new(),
            title_max_chars: 60,
            max_history: 100,
            lsp_config,
            diagnostics_cache: Arc::new(RwLock::new(DiagnosticsCache::new(max_diag_files))),
            reaper_cancel: CancellationToken::new(),
            additional_directories_allow: Vec::new(),
            auth_methods_config: vec![zeph_core::config::AcpAuthMethod::Agent],
            message_ids_enabled: true,
        }
    }

    /// Configure the additional-directories allowlist policy.
    #[must_use]
    pub fn with_additional_directories(
        mut self,
        dirs: Vec<zeph_core::config::AdditionalDir>,
    ) -> Self {
        self.additional_directories_allow = dirs
            .into_iter()
            .map(|d| d.as_path().to_path_buf())
            .collect();
        self
    }

    /// Configure auth methods advertised in `initialize`.
    #[must_use]
    pub fn with_auth_methods(mut self, methods: Vec<zeph_core::config::AcpAuthMethod>) -> Self {
        self.auth_methods_config = methods;
        self
    }

    /// Configure message-id echo behaviour.
    #[must_use]
    pub fn with_message_ids_enabled(mut self, enabled: bool) -> Self {
        self.message_ids_enabled = enabled;
        self
    }

    /// Configure LSP extension settings.
    #[must_use]
    pub fn with_lsp_config(mut self, config: zeph_core::config::AcpLspConfig) -> Self {
        let max_files = config.max_diagnostic_files;
        self.lsp_config = config;
        self.diagnostics_cache = Arc::new(RwLock::new(DiagnosticsCache::new(max_files)));
        self
    }

    #[must_use]
    pub fn with_store(mut self, store: SqliteStore) -> Self {
        self.store = Some(store);
        self
    }

    #[must_use]
    pub fn with_agent_info(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        self.agent_name = name.into();
        self.agent_version = version.into();
        self
    }

    #[must_use]
    pub fn with_provider_factory(
        mut self,
        factory: ProviderFactory,
        available_models: SharedAvailableModels,
    ) -> Self {
        self.provider_factory = Some(factory);
        self.available_models = available_models;
        self
    }

    fn available_models_snapshot(&self) -> Vec<String> {
        self.available_models.read().clone()
    }

    fn initial_model(&self) -> String {
        self.available_models_snapshot()
            .into_iter()
            .next()
            .unwrap_or_default()
    }

    #[must_use]
    pub fn with_mcp_manager(mut self, manager: Arc<McpManager>) -> Self {
        self.mcp_manager = Some(manager);
        self
    }

    #[must_use]
    pub fn with_project_rules(mut self, rules: Vec<std::path::PathBuf>) -> Self {
        self.project_rules = rules;
        self
    }

    #[must_use]
    pub fn with_title_max_chars(mut self, max_chars: usize) -> Self {
        self.title_max_chars = max_chars;
        self
    }

    #[must_use]
    pub fn with_max_history(mut self, max_history: usize) -> Self {
        self.max_history = max_history;
        self
    }

    /// Spawn a background task that periodically evicts idle sessions.
    ///
    /// The task runs until the agent's `reaper_cancel` token is cancelled.
    /// Tracked via a `tokio::spawn` (not `cx.spawn`) because it must survive
    /// individual connection teardowns in HTTP/WS mode.
    pub fn start_idle_reaper(&self) {
        let sessions = Arc::clone(&self.sessions);
        let idle_timeout = self.idle_timeout;
        let cancel = self.reaper_cancel.clone();
        let span = tracing::info_span!("acp.session.reap");
        tokio::spawn(
            async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_mins(1));
                interval.tick().await; // skip first tick
                loop {
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => break,
                        _ = interval.tick() => {}
                    }
                    let now_ms = u64::try_from(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis(),
                    )
                    .unwrap_or(u64::MAX);
                    let idle_timeout_ms =
                        u64::try_from(idle_timeout.as_millis()).unwrap_or(u64::MAX);
                    let expired: Vec<acp::schema::SessionId> = sessions
                        .lock()
                        .iter()
                        .filter(|(_, e)| {
                            let idle_ms =
                                now_ms.saturating_sub(e.last_active_ms.load(Ordering::Relaxed));
                            e.output_rx.lock().is_some() && idle_ms > idle_timeout_ms
                        })
                        .map(|(id, _)| id.clone())
                        .collect();
                    for id in expired {
                        if let Some(entry) = sessions.lock().remove(&id) {
                            entry.cancel_signal.notify_one();
                            tracing::debug!(session_id = %id, "evicted idle ACP session (timeout)");
                        }
                    }
                }
            }
            .instrument(span),
        );
    }

    /// Cancel the idle reaper task.
    pub fn shutdown(&self) {
        self.reaper_cancel.cancel();
    }

    pub(crate) fn build_acp_context(
        &self,
        session_id: &acp::schema::SessionId,
        cx: &acp::ConnectionTo<acp::Client>,
        cancel_signal: Arc<tokio::sync::Notify>,
        provider_override: Arc<RwLock<Option<AnyProvider>>>,
        cwd: PathBuf,
    ) -> AcpContext {
        // Use actual IDE capabilities from initialize(); default to false (deny by default).
        let (can_read, can_write, ide_supports_lsp) = {
            let caps = self.client_caps.read();
            let r = caps.fs.read_text_file;
            let w = caps.fs.write_text_file;
            let lsp = self.lsp_config.enabled
                && caps.meta.as_ref().is_some_and(|m| m.contains_key("lsp"));
            (r, w, lsp)
        };

        let conn = Arc::new(cx.clone());

        let (perm_gate, perm_handler) =
            AcpPermissionGate::new(Arc::clone(&conn), self.permission_file.clone());
        tokio::spawn(perm_handler);

        let (fs_exec, fs_handler) = AcpFileExecutor::new(
            Arc::clone(&conn),
            session_id.clone(),
            can_read,
            can_write,
            cwd,
            Some(perm_gate.clone()),
        );
        tokio::spawn(fs_handler);

        let (shell_exec, shell_handler) = AcpShellExecutor::new(
            Arc::clone(&conn),
            session_id.clone(),
            Some(perm_gate.clone()),
            120,
        );
        tokio::spawn(shell_handler);

        let lsp_provider = if ide_supports_lsp {
            let (provider, lsp_handler) = crate::lsp::AcpLspProvider::new(
                Arc::clone(&conn),
                true,
                self.lsp_config.request_timeout_secs,
                self.lsp_config.max_references,
                self.lsp_config.max_workspace_symbols,
            );
            tokio::spawn(lsp_handler);
            Some(provider)
        } else {
            None
        };

        AcpContext {
            file_executor: Some(fs_exec),
            shell_executor: Some(shell_exec),
            permission_gate: Some(perm_gate),
            cancel_signal,
            provider_override,
            parent_tool_use_id: None,
            lsp_provider,
            diagnostics_cache: Arc::clone(&self.diagnostics_cache),
        }
    }

    pub(crate) async fn send_notification(
        &self,
        session_id: &acp::schema::SessionId,
        notification: acp::schema::SessionNotification,
    ) -> acp::Result<()> {
        let tx = self
            .sessions
            .lock()
            .get(session_id)
            .map(|e| e.notify_tx.clone());
        let Some(tx) = tx else {
            return Err(acp::Error::internal_error().data("session not found"));
        };
        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send((notification, ack_tx))
            .await
            .map_err(|_| acp::Error::internal_error().data("notification channel closed"))?;
        ack_rx
            .await
            .map_err(|_| acp::Error::internal_error().data("notification ack lost"))
    }

    /// Fire-and-forget notification via the session's notify channel (no ack).
    pub(crate) fn send_notification_nowait(
        &self,
        session_id: &acp::schema::SessionId,
        notification: acp::schema::SessionNotification,
    ) {
        let tx = self
            .sessions
            .lock()
            .get(session_id)
            .map(|e| e.notify_tx.clone());
        if let Some(tx) = tx {
            let (ack_tx, _) = oneshot::channel();
            if let Err(e) = tx.try_send((notification, ack_tx)) {
                tracing::warn!(error = %e, "session notification dropped: channel full or closed");
            }
        }
    }

    fn handle_lsp_publish_diagnostics(&self, params: &str) {
        #[derive(serde::Deserialize)]
        struct PublishDiagnosticsParams {
            uri: String,
            #[serde(default)]
            diagnostics: Vec<crate::lsp::LspDiagnostic>,
        }

        match serde_json::from_str::<PublishDiagnosticsParams>(params) {
            Ok(p) => {
                let max = self.lsp_config.max_diagnostics_per_file;
                let mut diags = p.diagnostics;
                diags.truncate(max);
                tracing::debug!(
                    uri = %p.uri,
                    count = diags.len(),
                    "lsp/publishDiagnostics: cached"
                );
                self.diagnostics_cache.write().update(p.uri, diags);
            }
            Err(e) => {
                tracing::warn!(error = %e, "lsp/publishDiagnostics: failed to parse params");
            }
        }
    }

    #[allow(clippy::unused_async)]
    async fn handle_lsp_did_save(&self, params: &str, cx: &acp::ConnectionTo<acp::Client>) {
        #[derive(serde::Deserialize)]
        struct DidSaveParams {
            uri: String,
        }

        if !self.lsp_config.auto_diagnostics_on_save {
            return;
        }

        let uri = match serde_json::from_str::<DidSaveParams>(params) {
            Ok(p) => p.uri,
            Err(e) => {
                tracing::warn!(error = %e, "lsp/didSave: failed to parse params");
                return;
            }
        };

        let params_json = serde_json::json!({ "uri": &uri });
        let raw = match serde_json::value::to_raw_value(&params_json) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "lsp/didSave: failed to serialize params");
                return;
            }
        };
        let params_value =
            serde_json::from_str::<serde_json::Value>(raw.get()).unwrap_or(serde_json::Value::Null);
        let req = acp::UntypedMessage::new("lsp/diagnostics", params_value).unwrap_or_else(|_| {
            acp::UntypedMessage {
                method: "lsp/diagnostics".to_owned(),
                params: serde_json::Value::Null,
            }
        });
        let timeout = std::time::Duration::from_secs(self.lsp_config.request_timeout_secs);
        // Outbound round-trip inside a notification handler: must use cx.spawn to avoid blocking dispatch.
        let diagnostics_cache = Arc::clone(&self.diagnostics_cache);
        let max = self.lsp_config.max_diagnostics_per_file;
        let cx_inner = cx.clone();
        let uri_clone = uri.clone();
        cx.spawn(async move {
            match tokio::time::timeout(timeout, cx_inner.send_request(req).block_task()).await {
                Ok(Ok(resp)) => {
                    match serde_json::from_value::<Vec<crate::lsp::LspDiagnostic>>(resp) {
                        Ok(mut diags) => {
                            diags.truncate(max);
                            tracing::debug!(
                                uri = %uri_clone,
                                count = diags.len(),
                                "lsp/didSave: fetched diagnostics"
                            );
                            diagnostics_cache.write().update(uri_clone, diags);
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "lsp/didSave: failed to parse diagnostics response");
                        }
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "lsp/didSave: diagnostics request failed");
                }
                Err(_) => {
                    tracing::warn!(uri = %uri_clone, "lsp/didSave: diagnostics request timed out");
                }
            }
            Ok(())
        }).ok();
    }
}

#[derive(serde::Deserialize)]
struct McpRemoveParams {
    id: String,
}

/// Look up the `ConversationId` for an existing ACP session, creating one for legacy
/// sessions that predate migration 026 (where `conversation_id` is `NULL`).
///
/// Returns `None` when the store is unavailable or all creation attempts fail, allowing
/// the caller to proceed in ephemeral (no-history) mode rather than failing the session.
async fn resolve_conversation_id(
    store: &zeph_memory::store::SqliteStore,
    session_id: &acp::schema::SessionId,
) -> Option<ConversationId> {
    match store
        .get_acp_session_conversation_id(&session_id.to_string())
        .await
    {
        Ok(Some(cid)) => Some(cid),
        Ok(None) => {
            // Legacy session (conversation_id IS NULL): create and persist.
            match store.create_conversation().await {
                Ok(cid) => {
                    if let Err(e) = store
                        .set_acp_session_conversation_id(&session_id.to_string(), cid)
                        .await
                    {
                        tracing::warn!(error = %e, "failed to set conversation_id for legacy session");
                    }
                    Some(cid)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to create conversation for legacy session; session will have no persistent history");
                    None
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to look up conversation_id; session will have no persistent history");
            None
        }
    }
}

/// Handler implementations — called from `run_agent` handler closures.
impl ZephAcpAgentState {
    #[allow(clippy::unused_async)]
    #[tracing::instrument(skip_all, name = "acp.handler.initialize")]
    pub(crate) async fn do_initialize(
        &self,
        args: acp::schema::InitializeRequest,
    ) -> acp::Result<acp::schema::InitializeResponse> {
        tracing::debug!("ACP initialize");
        *self.client_caps.write() = args.client_capabilities;
        let title = format!("{} AI Agent", self.agent_name);

        // stdio transport implies a trusted local client; do not expose internal
        // configuration details. Provide only a generic authentication hint.
        let mut meta = serde_json::Map::new();
        meta.insert(
            "auth_hint".to_owned(),
            serde_json::json!("authentication required"),
        );

        let mut caps = acp::schema::AgentCapabilities::new()
            .load_session(true)
            .prompt_capabilities(
                acp::schema::PromptCapabilities::new()
                    .image(true)
                    .embedded_context(true),
            )
            .meta({
                let mut cap_meta = serde_json::Map::new();
                cap_meta.insert("config_options".to_owned(), serde_json::json!(true));
                cap_meta.insert("ext_methods".to_owned(), serde_json::json!(true));
                if self.lsp_config.enabled {
                    cap_meta.insert(
                        "lsp".to_owned(),
                        serde_json::json!({
                            "methods": crate::lsp::LSP_METHODS,
                            "notifications": crate::lsp::LSP_NOTIFICATIONS,
                        }),
                    );
                }
                cap_meta
            });
        // Advertise MCP transport capabilities when McpManager is present.
        // Only StreamableHTTP (http=true) is supported; SSE is deprecated in MCP spec 2025-11-25.
        if self.mcp_manager.is_some() {
            caps = caps.mcp_capabilities(acp::schema::McpCapabilities::new().http(true).sse(false));
        }
        #[cfg(any(
            feature = "unstable-session-close",
            feature = "unstable-session-fork",
            feature = "unstable-session-resume",
        ))]
        let caps = {
            let mut session_caps = acp::schema::SessionCapabilities::new();
            session_caps = session_caps.list(acp::schema::SessionListCapabilities::default());
            #[cfg(feature = "unstable-session-close")]
            {
                session_caps = session_caps.close(acp::schema::SessionCloseCapabilities::default());
            }
            #[cfg(feature = "unstable-session-fork")]
            {
                session_caps = session_caps.fork(acp::schema::SessionForkCapabilities::default());
            }
            #[cfg(feature = "unstable-session-resume")]
            {
                session_caps =
                    session_caps.resume(acp::schema::SessionResumeCapabilities::default());
            }
            caps.session_capabilities(session_caps)
        };

        #[cfg(feature = "unstable-logout")]
        let caps = caps.auth(
            acp::schema::AgentAuthCapabilities::default()
                .logout(acp::schema::LogoutCapabilities::default()),
        );

        let auth_methods: Vec<acp::schema::AuthMethod> = self
            .auth_methods_config
            .iter()
            .map(|m| match m {
                zeph_core::config::AcpAuthMethod::Agent => acp::schema::AuthMethod::Agent(
                    acp::schema::AuthMethodAgent::new("zeph", "Zeph"),
                ),
            })
            .collect();

        Ok(
            acp::schema::InitializeResponse::new(acp::schema::ProtocolVersion::LATEST)
                .auth_methods(auth_methods)
                .agent_info(
                    acp::schema::Implementation::new(&self.agent_name, &self.agent_version)
                        .title(title),
                )
                .agent_capabilities(caps)
                .meta(meta),
        )
    }

    #[tracing::instrument(skip_all, name = "acp.handler.dispatch")]
    pub(crate) async fn do_ext_method(
        &self,
        args: acp::schema::ExtRequest,
    ) -> acp::Result<acp::schema::ExtResponse> {
        if let Some(fut) = crate::custom::dispatch(self, &args) {
            return fut.await;
        }
        self.ext_method_mcp(&args).await
    }

    pub(crate) async fn do_ext_notification(
        &self,
        args: acp::schema::ExtNotification,
        cx: &acp::ConnectionTo<acp::Client>,
    ) -> acp::Result<()> {
        tracing::debug!(method = %args.method, "received ext_notification");
        match args.method.as_ref() {
            "lsp/publishDiagnostics" => {
                self.handle_lsp_publish_diagnostics(args.params.get());
            }
            "lsp/didSave" => {
                self.handle_lsp_did_save(args.params.get(), cx).await;
            }
            _ => {}
        }
        Ok(())
    }

    #[allow(clippy::unused_async)]
    #[tracing::instrument(skip_all, name = "acp.handler.authenticate")]
    pub(crate) async fn do_authenticate(
        &self,
        _args: acp::schema::AuthenticateRequest,
    ) -> acp::Result<acp::schema::AuthenticateResponse> {
        Ok(acp::schema::AuthenticateResponse::default())
    }

    #[cfg(feature = "unstable-logout")]
    #[allow(clippy::unused_async, dead_code)]
    #[tracing::instrument(skip_all, name = "acp.handler.logout")]
    pub(crate) async fn do_logout(
        &self,
        _args: acp::schema::LogoutRequest,
    ) -> acp::Result<acp::schema::LogoutResponse> {
        tracing::debug!("ACP logout (no-op: vault-based auth)");
        Ok(acp::schema::LogoutResponse::default())
    }

    /// Evict the oldest idle session when the session limit is reached.
    ///
    /// Idle is defined as: `output_rx` is `Some` (no prompt in flight).
    /// The lock-drop-and-reacquire pattern is intentional: the first lock
    /// guard must be released before removing the entry to avoid a potential
    /// deadlock if `cancel_signal.notify_one()` ever triggers reentrant
    /// session-map access.
    fn evict_oldest_idle_session_if_full(&self) -> acp::Result<()> {
        if self.sessions.lock().len() < self.max_sessions {
            return Ok(());
        }
        let evict_id = {
            let sessions = self.sessions.lock();
            sessions
                .iter()
                .filter(|(_, e)| e.output_rx.lock().is_some())
                .min_by_key(|(_, e)| e.last_active_ms.load(Ordering::Relaxed))
                .map(|(id, _)| id.clone())
        };
        match evict_id {
            Some(id) => {
                if let Some(entry) = self.sessions.lock().remove(&id) {
                    entry.cancel_signal.notify_one();
                    tracing::debug!(session_id = %id, "evicted idle ACP session (LRU)");
                }
                Ok(())
            }
            None => Err(acp::Error::internal_error().data("session limit reached")),
        }
    }

    /// Spawn the per-session notification drainer bound to `cx`.
    ///
    /// # Invariant
    ///
    /// Must be called **exactly once** per session entry. `notify_rx` is
    /// consumed here; a second call would panic on the `expect`.
    fn spawn_notify_drainer(
        entry: &SessionEntry,
        cx: &acp::ConnectionTo<acp::Client>,
    ) -> acp::Result<()> {
        let mut notify_rx = entry
            .notify_rx
            .lock()
            .take()
            .expect("notify_rx consumed once");
        let cx_drain = cx.clone();
        cx.spawn(async move {
            while let Some((notif, ack)) = notify_rx.recv().await {
                let _enter = tracing::info_span!("acp.session.notify").entered();
                if cx_drain.send_notification(notif).is_err() {
                    tracing::warn!("session_notification send failed; drainer exiting");
                    break;
                }
                ack.send(()).ok();
            }
            Ok(())
        })
    }

    /// Assemble the `NewSessionResponse` with config options and project rule metadata.
    fn build_new_session_response(
        &self,
        session_id: acp::schema::SessionId,
        initial_model: &str,
    ) -> acp::schema::NewSessionResponse {
        let available_models = self.available_models_snapshot();
        let config_options =
            build_config_options(&available_models, initial_model, false, "suggest");
        let default_mode_id = acp::schema::SessionModeId::new(DEFAULT_MODE_ID);
        let mut resp = acp::schema::NewSessionResponse::new(session_id)
            .modes(build_mode_state(&default_mode_id));
        if !config_options.is_empty() {
            resp = resp.config_options(config_options);
        }
        if !self.project_rules.is_empty() {
            let rules: Vec<serde_json::Value> = self
                .project_rules
                .iter()
                .filter_map(|p| p.file_name())
                .map(|n| serde_json::json!({"name": n.to_string_lossy()}))
                .collect();
            let mut meta = serde_json::Map::new();
            meta.insert("projectRules".to_owned(), serde_json::Value::Array(rules));
            resp = resp.meta(meta);
        }
        resp
    }

    #[tracing::instrument(skip_all, name = "acp.handler.new_session")]
    pub(crate) async fn do_new_session(
        &self,
        args: acp::schema::NewSessionRequest,
        cx: &acp::ConnectionTo<acp::Client>,
    ) -> acp::Result<acp::schema::NewSessionResponse> {
        #[cfg(feature = "unstable-session-add-dirs")]
        self.validate_additional_directories(&args.additional_directories)?;
        self.evict_oldest_idle_session_if_full()?;

        let session_id = acp::schema::SessionId::new(uuid::Uuid::new_v4().to_string());
        tracing::debug!(%session_id, "new ACP session");

        let (channel, handle) = LoopbackChannel::pair(LOOPBACK_CHANNEL_CAPACITY);
        let cancel_signal = Arc::clone(&handle.cancel_signal);
        let provider_override: Arc<RwLock<Option<AnyProvider>>> = Arc::new(RwLock::new(None));
        let provider_override_for_ctx = Arc::clone(&provider_override);

        let session_cwd = args.cwd.clone();
        let acp_ctx = self.build_acp_context(
            &session_id,
            cx,
            cancel_signal,
            provider_override_for_ctx,
            session_cwd.clone(),
        );
        let shell_executor = acp_ctx.shell_executor.clone();
        let initial_model = self.initial_model();
        let entry = Self::make_session_entry(
            handle,
            initial_model.clone(),
            session_cwd.clone(),
            shell_executor,
            provider_override,
        );

        Self::spawn_notify_drainer(&entry, cx)?;
        self.sessions.lock().insert(session_id.clone(), entry);

        let conversation_id = self.create_session_conversation(&session_id).await;
        let session_ctx = SessionContext {
            session_id: session_id.clone(),
            conversation_id,
            working_dir: session_cwd,
        };

        let spawner = Arc::clone(&self.spawner);
        let span = tracing::info_span!("acp.session.agent_loop", session_id = %session_id);
        tokio::task::spawn_local(
            async move {
                (spawner)(channel, Some(acp_ctx), session_ctx).await;
            }
            .instrument(span),
        );

        let resp = self.build_new_session_response(session_id.clone(), &initial_model);
        self.send_commands_update_nowait(&session_id);
        Ok(resp)
    }

    /// Take the `input_tx` / `output_rx` pair for a session and mark it as active.
    ///
    /// Returns an error when the session does not exist or a prompt is already in flight.
    /// Also writes `turn_message_id` into the per-session slot when the feature is enabled.
    fn acquire_prompt_channels(
        &self,
        session_id: &acp::schema::SessionId,
        #[cfg(feature = "unstable-message-id")] turn_message_id: Option<&str>,
    ) -> acp::Result<(mpsc::Sender<ChannelMessage>, mpsc::Receiver<LoopbackEvent>)> {
        let sessions = self.sessions.lock();
        let entry = sessions
            .get(session_id)
            .ok_or_else(|| acp::Error::internal_error().data("session not found"))?;
        let rx = entry
            .output_rx
            .lock()
            .take()
            .ok_or_else(|| acp::Error::internal_error().data("prompt already in progress"))?;
        entry.touch();
        // Write message_id here — output_rx take succeeded, prompt will proceed.
        #[cfg(feature = "unstable-message-id")]
        if let Some(mid) = turn_message_id {
            *entry
                .current_message_id
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(mid.to_owned());
        }
        Ok((entry.input_tx.clone(), rx))
    }

    /// Fire-and-forget: persist `text` as a `user_message` ACP event for `session_id`.
    fn persist_user_message_async(&self, session_id: &acp::schema::SessionId, text: String) {
        if let Some(ref store) = self.store {
            let sid = session_id.to_string();
            let store = store.clone();
            tokio::spawn(async move {
                if let Err(e) = store.save_acp_event(&sid, "user_message", &text).await {
                    tracing::warn!(error = %e, "failed to persist user message");
                }
            });
        }
    }

    #[tracing::instrument(skip_all, name = "acp.handler.prompt", fields(session_id = %args.session_id))]
    pub(crate) async fn do_prompt(
        &self,
        args: acp::schema::PromptRequest,
    ) -> acp::Result<acp::schema::PromptResponse> {
        tracing::debug!(session_id = %args.session_id, "ACP prompt");

        // Capture message_id; written to per-session slot only AFTER output_rx take succeeds
        // to prevent stale id from leaking when a prompt is rejected as "already in progress".
        #[cfg(feature = "unstable-message-id")]
        let turn_message_id: Option<String> = if self.message_ids_enabled {
            args.message_id.clone()
        } else {
            None
        };

        // Capture session cwd for file:// boundary enforcement.
        let session_cwd = self
            .sessions
            .lock()
            .get(&args.session_id)
            .and_then(|e| e.working_dir.lock().clone())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

        let (text, attachments) = self
            .collect_prompt_content(&args.prompt, &session_cwd)
            .await?;

        let trimmed_text = text.trim_start();
        if trimmed_text.starts_with('/') && is_acp_native_slash_command(trimmed_text) {
            return self
                .handle_slash_command(&args.session_id, trimmed_text)
                .await;
        }

        let (input_tx, output_rx) = self.acquire_prompt_channels(
            &args.session_id,
            #[cfg(feature = "unstable-message-id")]
            turn_message_id.as_deref(),
        )?;

        self.persist_user_message_async(&args.session_id, text.clone());

        input_tx
            .send(ChannelMessage {
                text: text.clone(),
                attachments,
                is_guest_context: false,
                is_from_bot: false,
            })
            .await
            .map_err(|_| acp::Error::internal_error().data("agent channel closed"))?;

        // Grab the cancel_signal so we can detect cancellation during the drain loop.
        let cancel_signal = self
            .sessions
            .lock()
            .get(&args.session_id)
            .map(|e| Arc::clone(&e.cancel_signal));

        // Block until the agent finishes this turn (signals via Flush or channel close).
        let (cancelled, stop_hint, rx) = self
            .drain_agent_events(&args.session_id, output_rx, cancel_signal)
            .await;

        // Return the receiver so future prompt() calls on this session can proceed.
        if let Some(entry) = self.sessions.lock().get(&args.session_id) {
            *entry.output_rx.lock() = Some(rx);
        }

        let stop_reason = compute_stop_reason(cancelled, stop_hint);

        // Generate session title after first successful agent response (fire-and-forget).
        if !cancelled {
            self.maybe_generate_session_title(&args.session_id, &text);
        }

        // Clear per-turn message-id slot now that the turn is complete.
        #[cfg(feature = "unstable-message-id")]
        if let Some(entry) = self.sessions.lock().get(&args.session_id) {
            *entry
                .current_message_id
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }

        Ok(build_prompt_response(
            #[cfg(feature = "unstable-message-id")]
            turn_message_id.as_deref(),
            stop_reason,
        ))
    }

    #[allow(clippy::unused_async)]
    #[tracing::instrument(skip_all, name = "acp.handler.cancel", fields(session_id = %args.session_id))]
    pub(crate) async fn do_cancel(&self, args: acp::schema::CancelNotification) -> acp::Result<()> {
        tracing::debug!(session_id = %args.session_id, "ACP cancel");
        if let Some(entry) = self.sessions.lock().get(&args.session_id) {
            entry.cancel_signal.notify_one();
        }
        Ok(())
    }

    #[cfg(feature = "unstable-session-close")]
    #[allow(clippy::unused_async, dead_code)]
    #[tracing::instrument(skip_all, name = "acp.handler.close_session", fields(session_id = %args.session_id))]
    pub(crate) async fn do_close_session(
        &self,
        args: acp::schema::CloseSessionRequest,
    ) -> acp::Result<acp::schema::CloseSessionResponse> {
        tracing::debug!(session_id = %args.session_id, "ACP session closed");
        if let Some(entry) = self.sessions.lock().remove(&args.session_id) {
            entry.cancel_signal.notify_one();
        }
        Ok(acp::schema::CloseSessionResponse::default())
    }

    #[tracing::instrument(skip_all, name = "acp.handler.load_session", fields(session_id = %args.session_id))]
    pub(crate) async fn do_load_session(
        &self,
        args: acp::schema::LoadSessionRequest,
        cx: &acp::ConnectionTo<acp::Client>,
    ) -> acp::Result<acp::schema::LoadSessionResponse> {
        #[cfg(feature = "unstable-session-add-dirs")]
        self.validate_additional_directories(&args.additional_directories)?;
        if self.sessions.lock().contains_key(&args.session_id) {
            return Ok(acp::schema::LoadSessionResponse::new());
        }

        let Some(ref store) = self.store else {
            return Err(acp::Error::internal_error().data("session not found"));
        };

        let exists = store
            .acp_session_exists(&args.session_id.to_string())
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, session_id = %args.session_id, "failed to check ACP session existence");
                acp::Error::internal_error().data("internal error")
            })?;

        if !exists {
            return Err(acp::Error::internal_error().data("session not found"));
        }

        let events = store
            .load_acp_events(&args.session_id.to_string())
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, session_id = %args.session_id, "failed to load ACP session events");
                acp::Error::internal_error().data("internal error")
            })?;

        let session_cwd = args.cwd.clone();
        let conversation_id = resolve_conversation_id(store, &args.session_id).await;

        let (channel, handle) = LoopbackChannel::pair(LOOPBACK_CHANNEL_CAPACITY);
        let cancel_signal = Arc::clone(&handle.cancel_signal);
        let provider_override: Arc<RwLock<Option<AnyProvider>>> = Arc::new(RwLock::new(None));
        let provider_override_for_ctx = Arc::clone(&provider_override);
        let acp_ctx = self.build_acp_context(
            &args.session_id,
            cx,
            cancel_signal,
            provider_override_for_ctx,
            session_cwd.clone(),
        );
        let shell_executor = acp_ctx.shell_executor.clone();
        let initial_model = self.initial_model();
        let entry = Self::make_session_entry(
            handle,
            initial_model,
            session_cwd.clone(),
            shell_executor,
            provider_override,
        );

        Self::spawn_notify_drainer(&entry, cx)?;

        self.sessions.lock().insert(args.session_id.clone(), entry);

        let session_ctx = SessionContext {
            session_id: args.session_id.clone(),
            conversation_id,
            working_dir: session_cwd,
        };

        let spawner = Arc::clone(&self.spawner);
        let span = tracing::info_span!("acp.session.agent_loop", session_id = %args.session_id);
        tokio::task::spawn_local(
            async move {
                (spawner)(channel, Some(acp_ctx), session_ctx).await;
            }
            .instrument(span),
        );

        self.replay_session_events(&args.session_id, events).await;

        let default_mode_id = acp::schema::SessionModeId::new(DEFAULT_MODE_ID);
        let load_resp =
            acp::schema::LoadSessionResponse::new().modes(build_mode_state(&default_mode_id));

        self.send_commands_update_nowait(&args.session_id);

        Ok(load_resp)
    }

    #[tracing::instrument(skip_all, name = "acp.handler.list_sessions")]
    pub(crate) async fn do_list_sessions(
        &self,
        args: acp::schema::ListSessionsRequest,
    ) -> acp::Result<acp::schema::ListSessionsResponse> {
        let mut result: std::collections::HashMap<String, acp::schema::SessionInfo> = {
            let sessions = self.sessions.lock();
            sessions
                .iter()
                .filter_map(|(session_id, entry)| {
                    let working_dir = entry.working_dir.lock().clone().unwrap_or_default();
                    if let Some(ref filter) = args.cwd
                        && &working_dir != filter
                    {
                        return None;
                    }
                    let meta = model_meta(&entry.current_model.lock());
                    let mut info = acp::schema::SessionInfo::new(session_id.clone(), working_dir)
                        .updated_at(entry.created_at.to_rfc3339())
                        .meta(meta);
                    if let Some(ref t) = *entry.title.lock() {
                        info = info.title(t.clone());
                    }
                    Some((session_id.to_string(), info))
                })
                .collect()
        };

        if let Some(ref store) = self.store {
            match store.list_acp_sessions(self.max_history).await {
                Ok(persisted) => {
                    for persisted_info in persisted {
                        let sid = acp::schema::SessionId::new(&*persisted_info.id);
                        if result.contains_key(&persisted_info.id) {
                            continue;
                        }
                        let info = acp::schema::SessionInfo::new(sid, std::path::PathBuf::new())
                            .title(persisted_info.title)
                            .updated_at(persisted_info.updated_at);
                        result.insert(persisted_info.id, info);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to list persisted ACP sessions");
                }
            }
        }

        let mut sessions_vec: Vec<acp::schema::SessionInfo> = result.into_values().collect();
        sessions_vec.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

        Ok(acp::schema::ListSessionsResponse::new(sessions_vec))
    }

    #[cfg(feature = "unstable-session-fork")]
    #[allow(dead_code, clippy::too_many_lines)]
    #[tracing::instrument(skip_all, name = "acp.handler.fork_session")]
    pub(crate) async fn do_fork_session(
        &self,
        args: acp::schema::ForkSessionRequest,
        cx: &acp::ConnectionTo<acp::Client>,
    ) -> acp::Result<acp::schema::ForkSessionResponse> {
        #[cfg(feature = "unstable-session-add-dirs")]
        self.validate_additional_directories(&args.additional_directories)?;
        let in_memory = self.sessions.lock().contains_key(&args.session_id);

        if !in_memory {
            match self.store.as_ref() {
                None => return Err(acp::Error::internal_error().data("session not found")),
                Some(s) => {
                    let exists = s
                        .acp_session_exists(&args.session_id.to_string())
                        .await
                        .map_err(|e| {
                            tracing::warn!(error = %e, "failed to check ACP session existence");
                            acp::Error::internal_error().data("internal error")
                        })?;
                    if !exists {
                        return Err(acp::Error::internal_error().data("session not found"));
                    }
                }
            }
        }

        if self.sessions.lock().len() >= self.max_sessions {
            let evict_id = {
                let sessions = self.sessions.lock();
                sessions
                    .iter()
                    .filter(|(_, e)| e.output_rx.lock().is_some())
                    .min_by_key(|(_, e)| e.last_active_ms.load(Ordering::Relaxed))
                    .map(|(id, _)| id.clone())
            };
            match evict_id {
                Some(id) => {
                    if let Some(entry) = self.sessions.lock().remove(&id) {
                        entry.cancel_signal.notify_one();
                        tracing::debug!(session_id = %id, "evicted idle ACP session (LRU)");
                    }
                }
                None => {
                    return Err(acp::Error::internal_error().data("session limit reached"));
                }
            }
        }

        let new_id = acp::schema::SessionId::new(uuid::Uuid::new_v4().to_string());
        tracing::debug!(source = %args.session_id, new = %new_id, "forking ACP session");

        let new_conversation_id = self.fork_conversation(&args.session_id, &new_id).await?;

        let (channel, handle) = LoopbackChannel::pair(LOOPBACK_CHANNEL_CAPACITY);
        let cancel_signal = Arc::clone(&handle.cancel_signal);
        let provider_override: Arc<RwLock<Option<AnyProvider>>> = Arc::new(RwLock::new(None));
        let provider_override_for_ctx = Arc::clone(&provider_override);
        let acp_ctx = self.build_acp_context(
            &new_id,
            cx,
            cancel_signal,
            provider_override_for_ctx,
            args.cwd.clone(),
        );
        let shell_executor = acp_ctx.shell_executor.clone();
        let initial_model = self.initial_model();
        let entry = Self::make_session_entry(
            handle,
            initial_model.clone(),
            args.cwd.clone(),
            shell_executor,
            provider_override,
        );

        Self::spawn_notify_drainer(&entry, cx)?;

        self.sessions.lock().insert(new_id.clone(), entry);

        let session_ctx = SessionContext {
            session_id: new_id.clone(),
            conversation_id: new_conversation_id,
            working_dir: args.cwd.clone(),
        };

        let spawner = Arc::clone(&self.spawner);
        let span = tracing::info_span!("acp.session.agent_loop", session_id = %new_id);
        tokio::task::spawn_local(
            async move {
                (spawner)(channel, Some(acp_ctx), session_ctx).await;
            }
            .instrument(span),
        );

        let available_models = self.available_models_snapshot();
        let config_options =
            build_config_options(&available_models, &initial_model, false, "suggest");
        let default_mode_id = acp::schema::SessionModeId::new(DEFAULT_MODE_ID);
        let mut resp =
            acp::schema::ForkSessionResponse::new(new_id).modes(build_mode_state(&default_mode_id));
        if !config_options.is_empty() {
            resp = resp.config_options(config_options);
        }
        Ok(resp)
    }

    #[cfg(feature = "unstable-session-resume")]
    #[allow(dead_code)]
    #[tracing::instrument(skip_all, name = "acp.handler.resume_session")]
    pub(crate) async fn do_resume_session(
        &self,
        args: acp::schema::ResumeSessionRequest,
        cx: &acp::ConnectionTo<acp::Client>,
    ) -> acp::Result<acp::schema::ResumeSessionResponse> {
        #[cfg(feature = "unstable-session-add-dirs")]
        self.validate_additional_directories(&args.additional_directories)?;
        if self.sessions.lock().contains_key(&args.session_id) {
            return Ok(acp::schema::ResumeSessionResponse::new());
        }

        let Some(ref store) = self.store else {
            return Err(acp::Error::internal_error().data("session not found"));
        };

        let exists = store
            .acp_session_exists(&args.session_id.to_string())
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, session_id = %args.session_id, "failed to check ACP session existence");
                acp::Error::internal_error().data("internal error")
            })?;

        if !exists {
            return Err(acp::Error::internal_error().data("session not found"));
        }

        if self.sessions.lock().len() >= self.max_sessions {
            let evict_id = {
                let sessions = self.sessions.lock();
                sessions
                    .iter()
                    .filter(|(id, e)| *id != &args.session_id && e.output_rx.lock().is_some())
                    .min_by_key(|(_, e)| e.last_active_ms.load(Ordering::Relaxed))
                    .map(|(id, _)| id.clone())
            };
            match evict_id {
                Some(id) => {
                    if let Some(entry) = self.sessions.lock().remove(&id) {
                        entry.cancel_signal.notify_one();
                        tracing::debug!(session_id = %id, "evicted idle ACP session (LRU)");
                    }
                }
                None => {
                    return Err(acp::Error::internal_error().data("session limit reached"));
                }
            }
        }

        let conversation_id = resolve_conversation_id(store, &args.session_id).await;

        let (channel, handle) = LoopbackChannel::pair(LOOPBACK_CHANNEL_CAPACITY);
        let cancel_signal = Arc::clone(&handle.cancel_signal);
        let provider_override: Arc<RwLock<Option<AnyProvider>>> = Arc::new(RwLock::new(None));
        let provider_override_for_ctx = Arc::clone(&provider_override);
        let acp_ctx = self.build_acp_context(
            &args.session_id,
            cx,
            cancel_signal,
            provider_override_for_ctx,
            args.cwd.clone(),
        );
        let shell_executor = acp_ctx.shell_executor.clone();
        let initial_model = self.initial_model();
        let entry = Self::make_session_entry(
            handle,
            initial_model,
            args.cwd.clone(),
            shell_executor,
            provider_override,
        );

        Self::spawn_notify_drainer(&entry, cx)?;

        self.sessions.lock().insert(args.session_id.clone(), entry);

        let session_ctx = SessionContext {
            session_id: args.session_id.clone(),
            conversation_id,
            working_dir: args.cwd,
        };

        let spawner = Arc::clone(&self.spawner);
        let span = tracing::info_span!("acp.session.agent_loop", session_id = %args.session_id);
        tokio::task::spawn_local(
            async move {
                (spawner)(channel, Some(acp_ctx), session_ctx).await;
            }
            .instrument(span),
        );

        Ok(acp::schema::ResumeSessionResponse::new())
    }

    #[allow(clippy::unused_async)]
    #[tracing::instrument(skip_all, name = "acp.handler.set_session_config_option")]
    pub(crate) async fn do_set_session_config_option(
        &self,
        args: acp::schema::SetSessionConfigOptionRequest,
    ) -> acp::Result<acp::schema::SetSessionConfigOptionResponse> {
        let config_id = args.config_id.0.clone();
        #[cfg(not(feature = "unstable-boolean-config"))]
        let value_str: std::sync::Arc<str> = args.value.0.clone();
        #[cfg(feature = "unstable-boolean-config")]
        let value_str: std::sync::Arc<str> = match &args.value {
            acp::schema::SessionConfigOptionValue::ValueId { value } => value.0.clone(),
            acp::schema::SessionConfigOptionValue::Boolean { value } => {
                if *value { "true" } else { "false" }.into()
            }
            _ => "".into(),
        };
        let value: &str = &value_str;

        let (current_model, thinking, auto_approve) = {
            let sessions = self.sessions.lock();
            let entry = sessions
                .get(&args.session_id)
                .ok_or_else(|| acp::Error::invalid_request().data("session not found"))?;

            self.apply_session_config(entry, config_id.as_ref(), value, &args.session_id)?;

            (
                entry.current_model.lock().clone(),
                entry.thinking_enabled.load(Ordering::Relaxed),
                entry.auto_approve_level.lock().clone(),
            )
        };

        let config_options = build_config_options(
            &self.available_models_snapshot(),
            &current_model,
            thinking,
            &auto_approve,
        );

        let changed_option = config_options.iter().find(|o| o.id.0 == config_id).cloned();

        if let Some(option) = changed_option {
            let update = acp::schema::SessionUpdate::ConfigOptionUpdate(
                acp::schema::ConfigOptionUpdate::new(vec![option]),
            );
            self.send_notification_nowait(
                &args.session_id,
                acp::schema::SessionNotification::new(args.session_id.clone(), update),
            );

            if config_id.as_ref() == "model" {
                let info_update = acp::schema::SessionUpdate::SessionInfoUpdate(
                    acp::schema::SessionInfoUpdate::new().meta(model_meta(&current_model)),
                );
                self.send_notification_nowait(
                    &args.session_id,
                    acp::schema::SessionNotification::new(args.session_id.clone(), info_update),
                );
            }
        }

        Ok(acp::schema::SetSessionConfigOptionResponse::new(
            config_options,
        ))
    }

    #[tracing::instrument(skip_all, name = "acp.handler.set_session_mode")]
    pub(crate) async fn do_set_session_mode(
        &self,
        args: acp::schema::SetSessionModeRequest,
    ) -> acp::Result<acp::schema::SetSessionModeResponse> {
        let valid_ids: &[&str] = &["code", "architect", "ask"];
        let mode_str = args.mode_id.0.as_ref();
        if !valid_ids.contains(&mode_str) {
            return Err(acp::Error::invalid_request().data(format!("unknown mode: {mode_str}")));
        }

        {
            let sessions = self.sessions.lock();
            let entry = sessions
                .get(&args.session_id)
                .ok_or_else(|| acp::Error::invalid_request().data("session not found"))?;
            *entry.current_mode.lock() = args.mode_id.clone();
        }

        tracing::debug!(session_id = %args.session_id, mode = %mode_str, "ACP session mode switched");

        let update = acp::schema::SessionUpdate::CurrentModeUpdate(
            acp::schema::CurrentModeUpdate::new(args.mode_id.clone()),
        );
        let notification = acp::schema::SessionNotification::new(args.session_id.clone(), update);
        if let Err(e) = self.send_notification(&args.session_id, notification).await {
            tracing::warn!(error = %e, "failed to send current_mode_update");
        }

        Ok(acp::schema::SetSessionModeResponse::new())
    }

    /// Validate `requested` paths against the configured allowlist.
    ///
    /// Each requested path is canonicalized and checked with `Path::starts_with` (component-aware)
    /// against every entry in `self.additional_directories_allow`. Returns an `invalid_params`
    /// error if any path is not covered by the allowlist.
    #[cfg(feature = "unstable-session-add-dirs")]
    fn validate_additional_directories(
        &self,
        requested: &[std::path::PathBuf],
    ) -> acp::Result<Vec<std::path::PathBuf>> {
        if requested.is_empty() {
            return Ok(Vec::new());
        }
        if self.additional_directories_allow.is_empty() {
            return Err(acp::Error::invalid_params()
                .data("additional_directories not permitted: allowlist is empty"));
        }
        let mut out = Vec::with_capacity(requested.len());
        for p in requested {
            let canon = std::fs::canonicalize(p).map_err(|e| {
                acp::Error::invalid_params()
                    .data(format!("cannot canonicalize {}: {e}", p.display()))
            })?;
            let allowed = self
                .additional_directories_allow
                .iter()
                .any(|allow| canon.starts_with(allow));
            if !allowed {
                return Err(acp::Error::invalid_params().data(format!(
                    "{} is not in the additional_directories allowlist",
                    canon.display()
                )));
            }
            out.push(canon);
        }
        Ok(out)
    }

    #[cfg(feature = "unstable-session-model")]
    #[allow(clippy::unused_async, dead_code)]
    #[tracing::instrument(skip_all, name = "acp.handler.set_session_model")]
    pub(crate) async fn do_set_session_model(
        &self,
        args: acp::schema::SetSessionModelRequest,
    ) -> acp::Result<acp::schema::SetSessionModelResponse> {
        let model_id: &str = &args.model_id.0;

        let Some(ref factory) = self.provider_factory else {
            return Err(acp::Error::internal_error().data("model switching not configured"));
        };

        if !self
            .available_models_snapshot()
            .iter()
            .any(|m| m == model_id)
        {
            return Err(acp::Error::invalid_request().data("model not in allowed list"));
        }

        let Some(new_provider) = factory(model_id) else {
            return Err(acp::Error::invalid_request().data("unknown model"));
        };

        {
            let sessions = self.sessions.lock();
            let entry = sessions
                .get(&args.session_id)
                .ok_or_else(|| acp::Error::internal_error().data("session not found"))?;
            *entry.provider_override.write() = Some(new_provider);
            model_id.clone_into(&mut *entry.current_model.lock());
        }

        tracing::debug!(session_id = %args.session_id, model = %model_id, "ACP session model switched via set_session_model");

        let info_update = acp::schema::SessionUpdate::SessionInfoUpdate(
            acp::schema::SessionInfoUpdate::new().meta(model_meta(model_id)),
        );
        self.send_notification_nowait(
            &args.session_id,
            acp::schema::SessionNotification::new(args.session_id.clone(), info_update),
        );

        Ok(acp::schema::SetSessionModelResponse::new())
    }
}

impl ZephAcpAgentState {
    fn apply_session_config(
        &self,
        entry: &SessionEntry,
        config_id: &str,
        value: &str,
        session_id: &acp::schema::SessionId,
    ) -> acp::Result<()> {
        match config_id {
            "model" => {
                let Some(ref factory) = self.provider_factory else {
                    return Err(acp::Error::internal_error().data("model switching not configured"));
                };
                let available_models = self.available_models_snapshot();
                if !available_models.iter().any(|m| m == value) {
                    return Err(acp::Error::invalid_request().data("model not in allowed list"));
                }
                let Some(new_provider) = factory(value) else {
                    return Err(acp::Error::invalid_request().data("unknown model"));
                };
                *entry.provider_override.write() = Some(new_provider);
                value.clone_into(&mut *entry.current_model.lock());
                tracing::debug!(session_id = %session_id, model = %value, "ACP model switched");
            }
            "thinking" => {
                let enabled = match value {
                    "on" => true,
                    "off" => false,
                    _ => {
                        return Err(
                            acp::Error::invalid_request().data("thinking value must be on or off")
                        );
                    }
                };
                entry.thinking_enabled.store(enabled, Ordering::Relaxed);
                tracing::debug!(session_id = %session_id, thinking = %enabled, "ACP thinking toggled");
            }
            "auto_approve" => {
                if !["suggest", "auto-edit", "full-auto"].contains(&value) {
                    return Err(acp::Error::invalid_request()
                        .data("auto_approve must be suggest, auto-edit, or full-auto"));
                }
                value.clone_into(&mut *entry.auto_approve_level.lock());
                tracing::debug!(session_id = %session_id, auto_approve = %value, "ACP auto-approve level changed");
            }
            _ => {
                return Err(acp::Error::invalid_request().data("unknown config_id"));
            }
        }
        Ok(())
    }

    /// Dispatch a slash command, returning a short-circuit `PromptResponse`.
    async fn handle_slash_command(
        &self,
        session_id: &acp::schema::SessionId,
        text: &str,
    ) -> acp::Result<acp::schema::PromptResponse> {
        let mut parts = text.splitn(2, ' ');
        let cmd = parts.next().unwrap_or("").trim();
        let arg = parts.next().unwrap_or("").trim();

        let reply = match cmd {
            "/help" => "Available commands:\n\
                 /help — show this message\n\
                 /model <id> — switch the active model\n\
                 /mode <code|architect|ask> — switch session mode\n\
                 /clear — clear session history\n\
                 /review [path] — review recent changes (read-only)"
                .to_owned(),
            "/model" => self.handle_model_command(session_id, arg)?,
            "/review" => {
                return self.handle_review_command(session_id, arg);
            }
            "/mode" => {
                let valid_ids: &[&str] = &["code", "architect", "ask"];
                if !valid_ids.contains(&arg) {
                    return Err(acp::Error::invalid_request().data(format!("unknown mode: {arg}")));
                }
                {
                    let sessions = self.sessions.lock();
                    let entry = sessions
                        .get(session_id)
                        .ok_or_else(|| acp::Error::invalid_request().data("session not found"))?;
                    *entry.current_mode.lock() = acp::schema::SessionModeId::new(arg);
                }
                let update = acp::schema::SessionUpdate::CurrentModeUpdate(
                    acp::schema::CurrentModeUpdate::new(acp::schema::SessionModeId::new(arg)),
                );
                let notification =
                    acp::schema::SessionNotification::new(session_id.clone(), update);
                if let Err(e) = self.send_notification(session_id, notification).await {
                    tracing::warn!(error = %e, "failed to send current_mode_update from /mode");
                }
                format!("Switched to mode: {arg}")
            }
            "/clear" => {
                if let Some(ref store) = self.store {
                    let sid = session_id.to_string();
                    let store = store.clone();
                    tokio::spawn(async move {
                        if let Err(e) = store.delete_acp_session(&sid).await {
                            tracing::warn!(error = %e, "failed to clear session history");
                        }
                        if let Err(e) = store.create_acp_session(&sid).await {
                            tracing::warn!(error = %e, "failed to recreate session after clear");
                        }
                    });
                }
                // Send sentinel to clear in-memory agent context.
                let tx = self
                    .sessions
                    .lock()
                    .get(session_id)
                    .map(|e| e.input_tx.clone());
                if let Some(tx) = tx {
                    let _ = tx.try_send(ChannelMessage {
                        text: "/clear".to_owned(),
                        attachments: vec![],
                        is_guest_context: false,
                        is_from_bot: false,
                    });
                }
                "Session history cleared.".to_owned()
            }
            _ => {
                return Err(acp::Error::invalid_request().data(format!("unknown command: {cmd}")));
            }
        };

        let update = acp::schema::SessionUpdate::AgentMessageChunk(acp::schema::ContentChunk::new(
            reply.clone().into(),
        ));
        let notification = acp::schema::SessionNotification::new(session_id.clone(), update);
        if let Err(e) = self.send_notification(session_id, notification).await {
            tracing::warn!(error = %e, "failed to send command reply");
        }

        Ok(acp::schema::PromptResponse::new(
            acp::schema::StopReason::EndTurn,
        ))
    }

    fn handle_review_command(
        &self,
        session_id: &acp::schema::SessionId,
        arg: &str,
    ) -> acp::Result<acp::schema::PromptResponse> {
        // Validate arg to prevent prompt injection: allow only safe path characters.
        if !arg.is_empty() {
            let valid = arg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | ' ' | '-'));
            if !valid || arg.len() > 512 {
                return Err(acp::Error::invalid_request()
                    .data("invalid path argument: only alphanumeric, _, ., /, space, - allowed (max 512 chars)"));
            }
        }
        let review_prompt = if arg.is_empty() {
            "Review the recent changes in this workspace. Show a plain-text diff summary. \
             Use only read_file and list_directory tools. Do not execute any commands or \
             write any files."
                .to_owned()
        } else {
            format!(
                "Review the following file or path: {arg}. Show a plain-text diff summary. \
                 Use only read_file and list_directory tools. Do not execute any commands or \
                 write any files."
            )
        };

        let tx = self
            .sessions
            .lock()
            .get(session_id)
            .map(|e| e.input_tx.clone());
        let Some(tx) = tx else {
            return Err(acp::Error::invalid_request().data("session not found"));
        };
        if tx
            .try_send(ChannelMessage {
                text: review_prompt,
                attachments: vec![],
                is_guest_context: false,
                is_from_bot: false,
            })
            .is_err()
        {
            tracing::warn!(%session_id, "failed to forward /review to agent input");
        }

        Ok(acp::schema::PromptResponse::new(
            acp::schema::StopReason::EndTurn,
        ))
    }

    fn resolve_model_fuzzy(&self, query: &str) -> acp::Result<String> {
        let available_models = self.available_models_snapshot();
        if available_models.iter().any(|m| m == query) {
            return Ok(query.to_owned());
        }
        let tokens: Vec<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(String::from)
            .collect();
        let candidates: Vec<&String> = available_models
            .iter()
            .filter(|m| {
                let lower = m.to_lowercase();
                tokens.iter().all(|t| lower.contains(t.as_str()))
            })
            .collect();
        match candidates.len() {
            0 => {
                let models = available_models.join(", ");
                Err(acp::Error::invalid_request()
                    .data(format!("no matching model found. Available: {models}")))
            }
            1 => Ok(candidates[0].clone()),
            _ => {
                let names: Vec<&str> = candidates.iter().map(|s| s.as_str()).collect();
                Err(acp::Error::invalid_request()
                    .data(format!("ambiguous model, candidates: {}", names.join(", "))))
            }
        }
    }

    fn handle_model_command(
        &self,
        session_id: &acp::schema::SessionId,
        arg: &str,
    ) -> acp::Result<String> {
        let available_models = self.available_models_snapshot();
        if arg.is_empty() {
            let models = available_models.join(", ");
            return Ok(format!("Available models: {models}"));
        }
        let Some(ref factory) = self.provider_factory else {
            return Err(acp::Error::internal_error().data("model switching not configured"));
        };
        let resolved = self.resolve_model_fuzzy(arg)?;
        let Some(new_provider) = factory(&resolved) else {
            return Err(acp::Error::invalid_request().data("unknown model"));
        };
        let sessions = self.sessions.lock();
        let entry = sessions
            .get(session_id)
            .ok_or_else(|| acp::Error::internal_error().data("session not found"))?;
        *entry.provider_override.write() = Some(new_provider);
        resolved.clone_into(&mut *entry.current_model.lock());
        Ok(format!("Switched to model: {resolved}"))
    }

    /// Collect text and attachments from ACP content blocks.
    ///
    /// Resolves `ResourceLink` URIs, decodes images, and formats embedded resources.
    /// Returns an error if the resulting text exceeds `MAX_PROMPT_BYTES`.
    async fn collect_prompt_content(
        &self,
        blocks: &[acp::schema::ContentBlock],
        session_cwd: &std::path::Path,
    ) -> acp::Result<(String, Vec<zeph_core::channel::Attachment>)> {
        let mut text = String::new();
        let mut attachments = Vec::new();
        for block in blocks {
            match block {
                acp::schema::ContentBlock::Text(t) => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&t.text);
                }
                acp::schema::ContentBlock::Image(img) => {
                    if !SUPPORTED_IMAGE_MIMES.contains(&img.mime_type.as_str()) {
                        tracing::debug!(mime_type = %img.mime_type, "unsupported image MIME type in ACP prompt, skipping");
                    } else if img.data.len() > MAX_IMAGE_BASE64_BYTES {
                        tracing::warn!(
                            size = img.data.len(),
                            max = MAX_IMAGE_BASE64_BYTES,
                            "image base64 data exceeds size limit, skipping"
                        );
                    } else {
                        use base64::Engine as _;
                        match base64::engine::general_purpose::STANDARD.decode(&img.data) {
                            Ok(bytes) => {
                                attachments.push(zeph_core::channel::Attachment {
                                    kind: zeph_core::channel::AttachmentKind::Image,
                                    data: bytes,
                                    filename: Some(format!(
                                        "image.{}",
                                        mime_to_ext(&img.mime_type)
                                    )),
                                });
                            }
                            Err(e) => {
                                tracing::debug!(error = %e, "failed to decode image base64, skipping");
                            }
                        }
                    }
                }
                acp::schema::ContentBlock::Resource(embedded) => {
                    if let acp::schema::EmbeddedResourceResource::TextResourceContents(res) =
                        &embedded.resource
                    {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        if res
                            .mime_type
                            .as_deref()
                            .is_some_and(|m| m == DIAGNOSTICS_MIME_TYPE)
                        {
                            format_diagnostics_block(&res.text, &mut text);
                        } else if res.mime_type.is_some()
                            && res.mime_type.as_deref() != Some("text/plain")
                        {
                            tracing::debug!(mime_type = ?res.mime_type, uri = %res.uri, "unknown resource mime type — skipping");
                        } else {
                            text.push_str("<resource name=\"");
                            text.push_str(&res.uri.replace('"', "&quot;"));
                            text.push_str("\">");
                            text.push_str(&res.text);
                            text.push_str("</resource>");
                        }
                    }
                }
                acp::schema::ContentBlock::Audio(_) => {
                    tracing::warn!("unsupported content block: Audio — skipping");
                }
                acp::schema::ContentBlock::ResourceLink(link) => {
                    match resolve_resource_link(link, session_cwd).await {
                        Ok(content) => {
                            // S-2: XML-escape URI (attribute) and content (body) using full escaping.
                            let escaped_uri = xml_escape(&link.uri);
                            let escaped_content = xml_escape(&content);
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str("<resource uri=\"");
                            text.push_str(&escaped_uri);
                            text.push_str("\">");
                            text.push_str(&escaped_content);
                            text.push_str("</resource>");
                        }
                        Err(e) => {
                            tracing::warn!(uri = %link.uri, error = %e, "ResourceLink resolution failed — skipping");
                        }
                    }
                }
                &_ => {
                    tracing::warn!("unsupported content block: unknown — skipping");
                }
            }
        }
        if text.len() > MAX_PROMPT_BYTES {
            return Err(acp::Error::invalid_request().data("prompt too large"));
        }
        Ok((text, attachments))
    }

    /// Drain events from `rx` until `Flush` or channel close, forwarding each as an ACP
    /// notification. Returns `(cancelled, stop_hint, rx)`.
    async fn drain_agent_events(
        &self,
        session_id: &acp::schema::SessionId,
        output_rx: tokio::sync::mpsc::Receiver<LoopbackEvent>,
        cancel_signal: Option<std::sync::Arc<tokio::sync::Notify>>,
    ) -> (
        bool,
        Option<StopHint>,
        tokio::sync::mpsc::Receiver<LoopbackEvent>,
    ) {
        let mut rx = output_rx;
        let mut cancelled = false;
        let mut stop_hint: Option<StopHint> = None;
        // Capture turn message_id once per drain to avoid re-locking sessions per event.
        #[cfg(feature = "unstable-message-id")]
        let turn_mid: Option<String> = self
            .sessions
            .lock()
            .get(session_id)
            .and_then(|e| e.current_message_id.lock().ok().and_then(|g| g.clone()));
        loop {
            let event = if let Some(ref signal) = cancel_signal {
                tokio::select! {
                    biased;
                    () = signal.notified() => { cancelled = true; break; }
                    ev = rx.recv() => ev,
                }
            } else {
                rx.recv().await
            };
            let Some(event) = event else { break };
            if let LoopbackEvent::Stop(hint) = event {
                stop_hint = Some(hint);
                continue;
            }
            let is_flush = matches!(event, LoopbackEvent::Flush);
            // Extract terminal_id before consuming the event so we can release after notify.
            let pending_terminal_release = if let LoopbackEvent::ToolOutput(ref data) = event {
                data.terminal_id.clone()
            } else {
                None
            };
            for update in loopback_event_to_updates(event) {
                if let Some(ref store) = self.store {
                    let sid = session_id.to_string();
                    let (event_type, payload) = session_update_to_event(&update);
                    let store = store.clone();
                    tokio::spawn(async move {
                        if let Err(e) = store.save_acp_event(&sid, event_type, &payload).await {
                            tracing::warn!(error = %e, "failed to persist session event");
                        }
                    });
                }
                #[cfg(feature = "unstable-message-id")]
                let update = apply_message_id_to_chunk(update, turn_mid.as_deref());
                #[cfg(not(feature = "unstable-message-id"))]
                let update = update;
                let notification =
                    acp::schema::SessionNotification::new(session_id.clone(), update);
                if let Err(e) = self.send_notification(session_id, notification).await {
                    tracing::warn!(error = %e, "failed to send notification");
                    break;
                }
            }
            // Release the terminal after tool_call_update has been sent.
            if let Some(terminal_id) = pending_terminal_release {
                let executor = self
                    .sessions
                    .lock()
                    .get(session_id)
                    .and_then(|e| e.shell_executor.clone());
                if let Some(executor) = executor {
                    executor.release_terminal(terminal_id);
                }
            }
            if is_flush {
                break;
            }
        }
        (cancelled, stop_hint, rx)
    }

    /// Create a forked conversation for `new_id` from `source_id`.
    ///
    /// Copies ACP events and conversation history from the source session synchronously before
    /// the agent loop is spawned to eliminate race conditions where the agent starts
    /// `load_history()` before the copy completes.
    #[allow(dead_code)]
    async fn fork_conversation(
        &self,
        source_id: &acp::schema::SessionId,
        new_id: &acp::schema::SessionId,
    ) -> acp::Result<Option<ConversationId>> {
        let Some(s) = &self.store else {
            return Ok(None);
        };
        let source_events = s
            .load_acp_events(&source_id.to_string())
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "failed to load ACP session events for fork");
                acp::Error::internal_error().data("internal error")
            })?;

        let new_id_str = new_id.to_string();
        let pairs: Vec<(&str, &str)> = source_events
            .iter()
            .map(|ev| (ev.event_type.as_str(), ev.payload.as_str()))
            .collect();

        match s.create_conversation().await {
            Ok(forked_cid) => {
                let forked_from_cid = s
                    .get_acp_session_conversation_id(&source_id.to_string())
                    .await
                    .unwrap_or(None);
                if let Err(e) = s
                    .create_acp_session_with_conversation(&new_id_str, forked_cid)
                    .await
                {
                    tracing::warn!(error = %e, "failed to persist forked ACP session mapping");
                }
                if let Err(e) = s.import_acp_events(&new_id_str, &pairs).await {
                    tracing::warn!(error = %e, "failed to import events for forked session");
                }
                if let Some(src_cid) = forked_from_cid
                    && let Err(e) = s.copy_conversation(src_cid, forked_cid).await
                {
                    tracing::warn!(error = %e, "failed to copy conversation for forked session");
                }
                Ok(Some(forked_cid))
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to create conversation for forked session; history will not be copied");
                if let Err(e2) = s.create_acp_session(&new_id_str).await {
                    tracing::warn!(error = %e2, "failed to persist forked ACP session");
                }
                if let Err(e2) = s.import_acp_events(&new_id_str, &pairs).await {
                    tracing::warn!(error = %e2, "failed to import events for forked session");
                }
                Ok(None)
            }
        }
    }

    /// Spawn a background title-generation task for the session's first prompt.
    fn maybe_generate_session_title(&self, session_id: &acp::schema::SessionId, user_text: &str) {
        let (should_generate, current_model, notify_tx) = {
            let sessions = self.sessions.lock();
            let Some(entry) = sessions.get(session_id) else {
                return;
            };
            let already_done = entry.first_prompt_done.load(Ordering::Relaxed);
            if already_done {
                return;
            }
            entry.first_prompt_done.store(true, Ordering::Relaxed);
            let model = entry.current_model.lock().clone();
            let tx = entry.notify_tx.clone();
            (true, model, tx)
        };
        if !should_generate {
            return;
        }
        if let Some(ref factory) = self.provider_factory
            && !current_model.is_empty()
            && let Some(provider) = factory(&current_model)
        {
            let user_text = user_text.to_owned();
            let sid = session_id.clone();
            let store = self.store.clone();
            let title_max_chars = self.title_max_chars;
            let sessions = Arc::clone(&self.sessions);
            tokio::spawn(async move {
                let prompt = format!(
                    "Generate a concise 5-7 word title for a conversation that starts \
                     with: {user_text}\nRespond with only the title, no quotes."
                );
                let messages = vec![zeph_llm::provider::Message::from_legacy(
                    zeph_llm::provider::Role::User,
                    &prompt,
                )];
                let sid_str = sid.to_string();
                let sid_prefix = &sid_str[..8.min(sid_str.len())];
                let fallback_title = format!("Session {sid_prefix}");
                let title = match tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    provider.chat(&messages),
                )
                .await
                {
                    Ok(Ok(t)) => truncate_to_chars(t.trim(), title_max_chars),
                    Ok(Err(e)) => {
                        tracing::debug!(error = %e, "title generation LLM call failed");
                        fallback_title
                    }
                    Err(_) => {
                        tracing::debug!("title generation timed out");
                        fallback_title
                    }
                };
                if let Some(ref store) = store {
                    let _ = store.update_session_title(&sid.to_string(), &title).await;
                }
                if let Some(entry) = sessions.lock().get(&sid) {
                    *entry.title.lock() = Some(title.clone());
                }
                let update = acp::schema::SessionUpdate::SessionInfoUpdate(
                    acp::schema::SessionInfoUpdate::new().title(title),
                );
                let notification = acp::schema::SessionNotification::new(sid, update);
                let (tx, _rx) = oneshot::channel();
                if let Err(e) = notify_tx.send((notification, tx)).await {
                    tracing::debug!(error = %e, "session title notification dropped");
                }
            });
        }
    }

    /// Build a fresh `SessionEntry` from a `LoopbackHandle`.
    fn make_session_entry(
        handle: LoopbackHandle,
        initial_model: String,
        cwd: PathBuf,
        shell_executor: Option<AcpShellExecutor>,
        provider_override: Arc<RwLock<Option<AnyProvider>>>,
    ) -> SessionEntry {
        // Bounded: prevents a misbehaving IDE from buffering notifications without limit.
        // 256 slots cover any realistic burst between drainer loop iterations.
        let (notify_tx, notify_rx) = mpsc::channel(256);
        let now_ms = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        )
        .unwrap_or(u64::MAX);
        SessionEntry {
            input_tx: handle.input_tx,
            output_rx: Mutex::new(Some(handle.output_rx)),
            cancel_signal: handle.cancel_signal,
            last_active_ms: AtomicU64::new(now_ms),
            created_at: chrono::Utc::now(),
            working_dir: Mutex::new(Some(cwd)),
            notify_tx,
            notify_rx: Mutex::new(Some(notify_rx)),
            provider_override,
            current_model: Mutex::new(initial_model),
            current_mode: Mutex::new(acp::schema::SessionModeId::new(DEFAULT_MODE_ID)),
            first_prompt_done: AtomicBool::new(false),
            title: Mutex::new(None),
            thinking_enabled: AtomicBool::new(false),
            auto_approve_level: Mutex::new("suggest".to_owned()),
            shell_executor,
            #[cfg(feature = "unstable-message-id")]
            current_message_id: std::sync::Mutex::new(None),
        }
    }

    /// Replay stored `AcpSessionEvent` records as ACP notifications for the session.
    async fn replay_session_events(
        &self,
        session_id: &acp::schema::SessionId,
        events: Vec<zeph_memory::store::AcpSessionEvent>,
    ) {
        for ev in events {
            let update = match ev.event_type.as_str() {
                "user_message" => acp::schema::SessionUpdate::UserMessageChunk(
                    acp::schema::ContentChunk::new(ev.payload.into()),
                ),
                "agent_message" => acp::schema::SessionUpdate::AgentMessageChunk(
                    acp::schema::ContentChunk::new(ev.payload.into()),
                ),
                "agent_thought" => acp::schema::SessionUpdate::AgentThoughtChunk(
                    acp::schema::ContentChunk::new(ev.payload.into()),
                ),
                "tool_call" => match serde_json::from_str::<acp::schema::ToolCall>(&ev.payload) {
                    Ok(tc) => acp::schema::SessionUpdate::ToolCall(tc),
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to deserialize tool call event during replay");
                        continue;
                    }
                },
                other => {
                    tracing::debug!(
                        event_type = other,
                        "skipping unknown event type during replay"
                    );
                    continue;
                }
            };
            let notification = acp::schema::SessionNotification::new(session_id.clone(), update);
            if let Err(e) = self.send_notification(session_id, notification).await {
                tracing::warn!(error = %e, "failed to replay notification");
                break;
            }
        }
    }

    /// Create a new conversation for `session_id` and persist the mapping.
    async fn create_session_conversation(
        &self,
        session_id: &acp::schema::SessionId,
    ) -> Option<ConversationId> {
        let store = self.store.as_ref()?;
        let sid = session_id.to_string();
        match store.create_conversation().await {
            Ok(cid) => {
                if let Err(e) = store.create_acp_session_with_conversation(&sid, cid).await {
                    tracing::warn!(error = %e, "failed to persist ACP session mapping; history may not survive restart");
                }
                Some(cid)
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to create conversation for ACP session; session will have no persistent history");
                if let Err(e2) = store.create_acp_session(&sid).await {
                    tracing::warn!(error = %e2, "failed to persist ACP session");
                }
                None
            }
        }
    }

    /// Fire-and-forget the `AvailableCommandsUpdate` notification for a session.
    fn send_commands_update_nowait(&self, session_id: &acp::schema::SessionId) {
        let cmds_update = acp::schema::SessionUpdate::AvailableCommandsUpdate(
            acp::schema::AvailableCommandsUpdate::new(build_available_commands()),
        );
        self.send_notification_nowait(
            session_id,
            acp::schema::SessionNotification::new(session_id.clone(), cmds_update),
        );
    }

    async fn ext_method_mcp(
        &self,
        args: &acp::schema::ExtRequest,
    ) -> acp::Result<acp::schema::ExtResponse> {
        let method = args.method.as_ref();
        match method {
            "_agent/mcp/list" => {
                let Some(ref manager) = self.mcp_manager else {
                    return Err(acp::Error::internal_error().data("MCP manager not configured"));
                };
                let servers = manager.list_servers().await;
                let json = serde_json::to_string(&servers).map_err(|e| {
                    tracing::error!(error = %e, "failed to serialize MCP server list");
                    acp::Error::internal_error().data("internal error")
                })?;
                let raw: Box<serde_json::value::RawValue> =
                    serde_json::value::RawValue::from_string(json).map_err(|e| {
                        tracing::error!(error = %e, "failed to build MCP list response");
                        acp::Error::internal_error().data("internal error")
                    })?;
                Ok(acp::schema::ExtResponse::new(raw.into()))
            }
            "_agent/mcp/add" => {
                let Some(ref manager) = self.mcp_manager else {
                    return Err(acp::Error::internal_error().data("MCP manager not configured"));
                };
                let entry: ServerEntry = serde_json::from_str(args.params.get())
                    .map_err(|e| acp::Error::invalid_request().data(e.to_string()))?;
                let tools = manager.add_server(&entry).await.map_err(|e| {
                    tracing::error!(error = %e, "failed to add MCP server");
                    acp::Error::internal_error().data("internal error")
                })?;
                let json = serde_json::json!({ "added": entry.id, "tools": tools.len() });
                let raw =
                    serde_json::value::RawValue::from_string(json.to_string()).map_err(|e| {
                        tracing::error!(error = %e, "failed to build MCP add response");
                        acp::Error::internal_error().data("internal error")
                    })?;
                Ok(acp::schema::ExtResponse::new(raw.into()))
            }
            "_agent/mcp/remove" => {
                let Some(ref manager) = self.mcp_manager else {
                    return Err(acp::Error::internal_error().data("MCP manager not configured"));
                };
                let params: McpRemoveParams = serde_json::from_str(args.params.get())
                    .map_err(|e| acp::Error::invalid_request().data(e.to_string()))?;
                manager.remove_server(&params.id).await.map_err(|e| {
                    tracing::error!(error = %e, "failed to remove MCP server");
                    acp::Error::internal_error().data("internal error")
                })?;
                let raw = serde_json::value::RawValue::from_string(
                    serde_json::json!({ "removed": params.id }).to_string(),
                )
                .map_err(|e| {
                    tracing::error!(error = %e, "failed to build MCP remove response");
                    acp::Error::internal_error().data("internal error")
                })?;
                Ok(acp::schema::ExtResponse::new(raw.into()))
            }
            _ => Ok(acp::schema::ExtResponse::new(
                serde_json::value::RawValue::NULL.to_owned().into(),
            )),
        }
    }
}

/// Returns `true` when `trimmed_text` is an ACP-native slash command that should
/// be handled by [`ZephAcpAgentState::handle_slash_command`] rather than forwarded
/// to the agent loop.
fn is_acp_native_slash_command(trimmed_text: &str) -> bool {
    trimmed_text == "/help"
        || trimmed_text.starts_with("/help ")
        || trimmed_text == "/mode"
        || trimmed_text.starts_with("/mode ")
        || trimmed_text == "/clear"
        || trimmed_text.starts_with("/review")
        || trimmed_text == "/model"
        || trimmed_text.starts_with("/model ")
}

/// Map `(cancelled, stop_hint)` to the ACP `StopReason` wire value.
fn compute_stop_reason(cancelled: bool, stop_hint: Option<StopHint>) -> acp::schema::StopReason {
    if cancelled {
        acp::schema::StopReason::Cancelled
    } else {
        match stop_hint {
            Some(StopHint::MaxTokens) => acp::schema::StopReason::MaxTokens,
            Some(StopHint::MaxTurnRequests) => acp::schema::StopReason::MaxTurnRequests,
            None => acp::schema::StopReason::EndTurn,
        }
    }
}

/// Construct the `PromptResponse`, optionally echoing `turn_message_id`.
fn build_prompt_response(
    #[cfg(feature = "unstable-message-id")] turn_message_id: Option<&str>,
    stop_reason: acp::schema::StopReason,
) -> acp::schema::PromptResponse {
    let r = acp::schema::PromptResponse::new(stop_reason);
    #[cfg(feature = "unstable-message-id")]
    if let Some(mid) = turn_message_id {
        return r.user_message_id(mid.to_owned());
    }
    r
}

pub(super) mod helpers;
use helpers::{
    DEFAULT_MODE_ID, DIAGNOSTICS_MIME_TYPE, build_available_commands, build_config_options,
    build_mode_state, format_diagnostics_block, loopback_event_to_updates, mime_to_ext, model_meta,
    session_update_to_event, xml_escape,
};

pub(crate) mod handlers;

/// Build a request handler closure that clones `state` for each incoming request.
///
/// The closure signature matches what `Builder::on_receive_request` expects:
/// `(req, responder, cx) -> impl Future<Output = acp::Result<()>>`.
macro_rules! req_handler {
    ($state:expr, $handler:path) => {{
        let s = Arc::clone(&$state);
        move |req, responder, cx| {
            let s = Arc::clone(&s);
            async move { $handler(req, responder, cx, s).await }
        }
    }};
}

/// Build a notification handler closure that clones `state` for each incoming notification.
macro_rules! notif_handler {
    ($state:expr, $handler:path) => {{
        let s = Arc::clone(&$state);
        move |notif, cx| {
            let s = Arc::clone(&s);
            async move { $handler(notif, cx, s).await }
        }
    }};
}

/// Run the ACP agent loop over the provided transport until the connection closes.
///
/// Builds the ACP 0.11 handler chain from `state` and connects it to `transport`.
/// All request handlers delegate to the corresponding `do_*` methods on
/// [`ZephAcpAgentState`] which carry all session management logic.
///
/// # Errors
///
/// Returns an `acp::Error` if the underlying JSON-RPC transport fails.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
/// use agent_client_protocol as acp;
/// use agent_client_protocol::ByteStreams;
/// use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
/// use zeph_acp::agent::{ZephAcpAgentState, run_agent};
/// use zeph_acp::AgentSpawner;
///
/// # async fn example(spawner: AgentSpawner) -> acp::Result<()> {
/// let state = Arc::new(ZephAcpAgentState::new(spawner, 4, 1800, None));
/// run_agent(
///     state,
///     ByteStreams::new(
///         tokio::io::stdout().compat_write(),
///         tokio::io::stdin().compat(),
///     ),
/// ).await
/// # }
/// ```
pub async fn run_agent(
    state: Arc<ZephAcpAgentState>,
    transport: impl acp::ConnectTo<acp::Agent>,
) -> acp::Result<()> {
    #[cfg(feature = "unstable-session-close")]
    use handlers::close_session;
    #[cfg(feature = "unstable-session-fork")]
    use handlers::fork_session;
    #[cfg(feature = "unstable-logout")]
    use handlers::logout;
    #[cfg(feature = "unstable-session-resume")]
    use handlers::resume_session;
    #[cfg(feature = "unstable-session-model")]
    use handlers::set_session_model;
    use handlers::{
        authenticate, cancel, dispatch, initialize, list_sessions, load_session, new_session,
        prompt, set_session_config_option, set_session_mode,
    };

    let builder = acp::Agent
        .builder()
        .on_receive_request(
            req_handler!(state, initialize::handle_initialize),
            acp::on_receive_request!(),
        )
        .on_receive_request(
            req_handler!(state, authenticate::handle_authenticate),
            acp::on_receive_request!(),
        )
        .on_receive_request(
            req_handler!(state, new_session::handle_new_session),
            acp::on_receive_request!(),
        )
        .on_receive_request(
            req_handler!(state, prompt::handle_prompt),
            acp::on_receive_request!(),
        )
        .on_receive_request(
            req_handler!(state, list_sessions::handle_list_sessions),
            acp::on_receive_request!(),
        )
        .on_receive_request(
            req_handler!(state, load_session::handle_load_session),
            acp::on_receive_request!(),
        )
        .on_receive_request(
            req_handler!(
                state,
                set_session_config_option::handle_set_session_config_option
            ),
            acp::on_receive_request!(),
        )
        .on_receive_request(
            req_handler!(state, set_session_mode::handle_set_session_mode),
            acp::on_receive_request!(),
        )
        .on_receive_notification(
            notif_handler!(state, cancel::handle_cancel),
            acp::on_receive_notification!(),
        );

    #[cfg(feature = "unstable-session-close")]
    let builder = builder.on_receive_request(
        req_handler!(state, close_session::handle_close_session),
        acp::on_receive_request!(),
    );
    #[cfg(feature = "unstable-session-fork")]
    let builder = builder.on_receive_request(
        req_handler!(state, fork_session::handle_fork_session),
        acp::on_receive_request!(),
    );
    #[cfg(feature = "unstable-session-resume")]
    let builder = builder.on_receive_request(
        req_handler!(state, resume_session::handle_resume_session),
        acp::on_receive_request!(),
    );
    #[cfg(feature = "unstable-session-model")]
    let builder = builder.on_receive_request(
        req_handler!(state, set_session_model::handle_set_session_model),
        acp::on_receive_request!(),
    );
    #[cfg(feature = "unstable-logout")]
    let builder = builder.on_receive_request(
        req_handler!(state, logout::handle_logout),
        acp::on_receive_request!(),
    );

    builder
        .on_receive_dispatch(
            {
                let s = Arc::clone(&state);
                move |msg, cx| {
                    let s = Arc::clone(&s);
                    async move { dispatch::handle_dispatch(msg, cx, s).await }
                }
            },
            acp::on_receive_dispatch!(),
        )
        .connect_to(transport)
        .await
}

/// Attach `message_id` to `AgentMessageChunk`, `UserMessageChunk`, and `AgentThoughtChunk`
/// updates when a message id is present for this turn.
#[cfg(feature = "unstable-message-id")]
fn apply_message_id_to_chunk(
    update: acp::schema::SessionUpdate,
    message_id: Option<&str>,
) -> acp::schema::SessionUpdate {
    let Some(mid) = message_id else {
        return update;
    };
    match update {
        acp::schema::SessionUpdate::AgentMessageChunk(chunk) => {
            acp::schema::SessionUpdate::AgentMessageChunk(chunk.message_id(mid.to_owned()))
        }
        acp::schema::SessionUpdate::UserMessageChunk(chunk) => {
            acp::schema::SessionUpdate::UserMessageChunk(chunk.message_id(mid.to_owned()))
        }
        acp::schema::SessionUpdate::AgentThoughtChunk(chunk) => {
            acp::schema::SessionUpdate::AgentThoughtChunk(chunk.message_id(mid.to_owned()))
        }
        other => other,
    }
}

/// Compile-time assertions that ACP state and executors are `Send + Sync`.
const _: () = {
    #[allow(clippy::used_underscore_items)]
    fn assert_send_sync<T: Send + Sync>() {}
    fn check_send_sync() {
        assert_send_sync::<ZephAcpAgentState>();
        assert_send_sync::<crate::fs::AcpFileExecutor>();
        assert_send_sync::<crate::terminal::AcpShellExecutor>();
        assert_send_sync::<crate::permission::AcpPermissionGate>();
    }
    let _ = check_send_sync;
};

#[cfg(any())] // ACP 0.10 tests disabled — rewrite for 0.11 tracked in #3267
mod tests;

#[cfg(all(test, feature = "unstable-message-id"))]
mod message_id_tests {
    use super::*;

    fn agent_chunk(text: &str) -> acp::schema::SessionUpdate {
        acp::schema::SessionUpdate::AgentMessageChunk(acp::schema::ContentChunk::new(
            text.to_owned().into(),
        ))
    }

    fn user_chunk(text: &str) -> acp::schema::SessionUpdate {
        acp::schema::SessionUpdate::UserMessageChunk(acp::schema::ContentChunk::new(
            text.to_owned().into(),
        ))
    }

    #[test]
    fn apply_sets_message_id_on_agent_chunk() {
        let update = agent_chunk("hello");
        let result = apply_message_id_to_chunk(update, Some("msg-001"));
        if let acp::schema::SessionUpdate::AgentMessageChunk(chunk) = result {
            assert_eq!(chunk.message_id, Some("msg-001".to_owned()));
        } else {
            panic!("expected AgentMessageChunk");
        }
    }

    #[test]
    fn apply_sets_message_id_on_user_chunk() {
        let update = user_chunk("hi");
        let result = apply_message_id_to_chunk(update, Some("msg-002"));
        if let acp::schema::SessionUpdate::UserMessageChunk(chunk) = result {
            assert_eq!(chunk.message_id, Some("msg-002".to_owned()));
        } else {
            panic!("expected UserMessageChunk");
        }
    }

    #[test]
    fn apply_none_message_id_is_noop() {
        let update = agent_chunk("hello");
        let result = apply_message_id_to_chunk(update, None);
        if let acp::schema::SessionUpdate::AgentMessageChunk(chunk) = result {
            assert_eq!(chunk.message_id, None);
        } else {
            panic!("expected AgentMessageChunk");
        }
    }
}
