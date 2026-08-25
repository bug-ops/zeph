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

#[cfg(feature = "unstable-llm-providers")]
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::{Mutex, RwLock};

use agent_client_protocol as acp;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zeph_common::task_supervisor::TaskSupervisor;
use zeph_core::channel::{ChannelMessage, LoopbackChannel};
use zeph_core::{ContentSanitizer, LoopbackEvent, StopHint};
use zeph_llm::any::AnyProvider;
use zeph_mcp::McpManager;
use zeph_memory::ConversationId;
use zeph_memory::store::SqliteStore;

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
    pub session_id: acp::schema::v1::SessionId,
    /// `SQLite` conversation ID for persisting message history, if available.
    pub conversation_id: Option<ConversationId>,
    /// Working directory reported by the IDE for this session.
    pub working_dir: PathBuf,
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
    /// Handle for proactively notifying the client outside of the prompt-drain path.
    ///
    /// See [`SessionStatusNotifier`] for why this exists alongside `LoopbackChannel::send_status`.
    pub status_notifier: SessionStatusNotifier,
    /// Elicitation bridge for sending form requests to the IDE.
    ///
    /// `None` when the IDE did not advertise elicitation capability during `initialize()`,
    /// or when the `unstable-elicitation` feature is disabled.
    #[cfg(feature = "unstable-elicitation")]
    #[allow(dead_code)]
    pub(crate) elicitation_bridge: Option<elicitation::ElicitationBridge>,
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
#[cfg_attr(docsrs, doc(cfg(feature = "acp-http")))]
pub type SendAgentSpawner = AgentSpawner;

/// Sender half for delivering session notifications to the per-session drainer.
///
/// `pub` (not `pub(crate)`) solely so [`SessionStatusNotifier::new`] can appear in the public
/// API: it lets integration tests outside this crate construct a real notifier bound to a
/// plain `mpsc::channel`, without a full `AcpContext`/ACP connection.
pub type NotifySender = mpsc::Sender<(acp::schema::v1::SessionNotification, oneshot::Sender<()>)>;

/// Receiver half paired with [`NotifySender`].
pub(crate) type NotifyReceiver =
    mpsc::Receiver<(acp::schema::v1::SessionNotification, oneshot::Sender<()>)>;

/// Fire-and-forget handle for pushing a client-visible status update outside of the normal
/// prompt-drain path.
///
/// Most agent output reaches the client through [`LoopbackChannel`] and is only flushed to
/// the IDE as part of a `session/prompt` response (see `helpers::loopback_event_to_updates`
/// and `drain_agent_events`). Some failures are discovered before any prompt is ever sent —
/// e.g. session hydration in `spawn_acp_agent` (`zeph` binary crate) hitting
/// `SessionError::AlreadyLocked` — so a client that never prompts, or whose first prompt is
/// cancelled before the drain, would otherwise never learn persistence degraded (#5519).
/// `SessionStatusNotifier` reuses the same per-session notification channel that
/// `ZephAcpAgentState::send_notification_nowait` already drives for other proactive updates
/// (e.g. `available_commands_update`), so it delivers immediately via the session's notify
/// drainer instead of waiting on the next prompt.
#[derive(Clone)]
pub struct SessionStatusNotifier {
    notify_tx: NotifySender,
    session_id: acp::schema::v1::SessionId,
}

impl SessionStatusNotifier {
    /// Builds a notifier bound to a session's notification channel.
    ///
    /// `notify_tx` is normally a `SessionEntry`'s own notify sender (see `build_acp_context`),
    /// so pushes from this notifier are drained by the same task that delivers this session's
    /// `session/update` notifications to the client. `pub` (not `pub(crate)`) so integration
    /// tests outside this crate can bind a notifier to a plain `mpsc::channel` and assert on
    /// the receiving end directly, without constructing a full `AcpContext`/ACP connection.
    ///
    /// # Examples
    ///
    /// ```
    /// use agent_client_protocol::schema::v1::SessionId;
    /// use tokio::sync::mpsc;
    /// use zeph_acp::SessionStatusNotifier;
    ///
    /// let (tx, mut rx) = mpsc::channel(4);
    /// let notifier = SessionStatusNotifier::new(tx, SessionId::new("session-1".to_owned()));
    /// notifier.notify_status_nowait("degraded");
    /// assert!(rx.try_recv().is_ok());
    /// ```
    #[must_use]
    pub fn new(notify_tx: NotifySender, session_id: acp::schema::v1::SessionId) -> Self {
        Self {
            notify_tx,
            session_id,
        }
    }

    /// Push a status message to the client immediately, without waiting for an ack.
    ///
    /// Mirrors the `AgentThoughtChunk` shape `loopback_event_to_updates` already produces for
    /// `LoopbackEvent::Status`, so proactive and prompt-drained status messages render
    /// identically on the client. Errors (channel full or closed) are logged and swallowed —
    /// same tolerance as `ZephAcpAgentState::send_notification_nowait`.
    pub fn notify_status_nowait(&self, text: impl Into<String>) {
        let text = text.into();
        if text.is_empty() {
            return;
        }
        let update = acp::schema::v1::SessionUpdate::AgentThoughtChunk(
            acp::schema::v1::ContentChunk::new(text.into()),
        );
        let notification =
            acp::schema::v1::SessionNotification::new(self.session_id.clone(), update);
        let (ack_tx, _) = oneshot::channel();
        if let Err(e) = self.notify_tx.try_send((notification, ack_tx)) {
            tracing::warn!(
                error = %e,
                "proactive session status notification dropped: channel full or closed"
            );
        }
    }
}

/// Per-session config fields seeded into a fresh `SessionEntry` (#5373).
///
/// Callers pass either configured defaults (new/loaded session) or values inherited from a
/// source session (fork/resume of an existing session) — see `inherited_session_config`.
pub(crate) struct SessionConfigSeed {
    thinking_enabled: bool,
    auto_approve_level: String,
    temperature_preset: zeph_config::AcpTemperaturePreset,
}

/// Monotonic counter assigned to every `SessionEntry` at construction (`make_session_entry`).
///
/// Lets `turn::PromptChannelGuard`, captured at the start of a turn, detect at restore time
/// whether the session map still holds the *same* entry it started with. `do_load_session` /
/// `do_resume_session` both early-return without inserting anything if the `SessionId` is
/// already present in the map — so a fresh `SessionEntry` only ever lands under an id that a
/// prior `do_close_session`/`do_delete_session` has already `remove()`-d. Neither of those
/// removals waits for or aborts any turn still in flight on that session, so a
/// `PromptChannelGuard` acquired before the close can outlive it and still be holding the
/// (now orphaned) receiver when the id is reloaded/resumed (#6666). The fresh entry gets a new
/// generation, so the stale guard's later `Drop` can tell its receiver is no longer the live
/// one and skip clobbering the reloaded session's `output_rx`.
static SESSION_ENTRY_GENERATION: AtomicU64 = AtomicU64::new(0);

pub(crate) struct SessionEntry {
    pub(crate) input_tx: mpsc::Sender<ChannelMessage>,
    /// Receiver is owned solely by the `prompt()` handler.
    /// `Mutex` instead of `RefCell` so `SessionEntry` is `Send`.
    pub(crate) output_rx: Mutex<Option<mpsc::Receiver<LoopbackEvent>>>,
    /// Identity stamp from [`SESSION_ENTRY_GENERATION`], assigned once at construction.
    /// See that constant's doc for why this exists.
    pub(crate) generation: u64,
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
    current_mode: Mutex<acp::schema::v1::SessionModeId>,
    /// Set after the first successful prompt so title generation fires only once.
    first_prompt_done: AtomicBool,
    /// Auto-generated session title; populated after first prompt via `SessionTitle` event.
    title: Mutex<Option<String>>,
    /// Whether extended thinking is enabled for this session.
    thinking_enabled: AtomicBool,
    /// Auto-approve level for this session ("suggest" | "auto-edit" | "full-auto").
    auto_approve_level: Mutex<String>,
    /// Sampling-temperature preset for this session, advertised under the `model_config`
    /// `session/set_config_option` category (`config_id = "temperature"`).
    temperature_preset: Mutex<zeph_config::AcpTemperaturePreset>,
    /// Shell executor for this session, retained so the event loop can release terminals
    /// after `tool_call_update` notifications are sent (ACP requires the terminal to
    /// remain alive until after the notification that embeds it).
    pub(crate) shell_executor: Option<AcpShellExecutor>,
    /// Join handle for this session's agent-loop task (`spawn_local`, spawned in
    /// `do_new_session`/`do_load_session`/`do_fork_session`/`do_resume_session`).
    ///
    /// `Mutex`-wrapped (unlike `elicitation_bridge_handle` below) because it is attached
    /// *after* the entry is already behind `self.sessions`' lock — the loop task is spawned
    /// once `session_ctx` is ready, which depends on async work (conversation resolution)
    /// that happens after the entry is constructed — so it needs interior mutability to be
    /// set through a shared reference (see `set_agent_loop_handle`). `do_close_session` and
    /// `do_delete_session` `take()` and abort+await it (bounded) before removing the entry,
    /// so a subsequent reload/resume can never race a still-running loop left over from a
    /// closed/deleted session generation (#6674). `Drop` also aborts it unconditionally as a
    /// safety net for the other entry-removal path (LRU eviction in `do_fork_session`/
    /// `do_resume_session`), mirroring `elicitation_bridge_handle`'s existing pattern.
    pub(crate) agent_loop_handle: Mutex<Option<JoinHandle<()>>>,
    /// Join handle for the elicitation bridge task spawned in `do_new_session`.
    ///
    /// Aborted on session close / reap for clean shutdown. `None` when the IDE
    /// did not advertise elicitation capability or the feature is not enabled.
    #[cfg(feature = "unstable-elicitation")]
    pub(crate) elicitation_bridge_handle: Option<JoinHandle<()>>,
    /// Lifetime token and cost totals for the session-close usage summary.
    #[cfg(feature = "unstable-session-usage")]
    pub(crate) usage_accumulator: Mutex<SessionUsageAccumulator>,
}

impl Drop for SessionEntry {
    fn drop(&mut self) {
        if let Some(handle) = self.agent_loop_handle.lock().take() {
            handle.abort();
        }
        #[cfg(feature = "unstable-elicitation")]
        if let Some(handle) = self.elicitation_bridge_handle.take() {
            handle.abort();
        }
    }
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

type SessionMap = Arc<Mutex<std::collections::HashMap<acp::schema::v1::SessionId, SessionEntry>>>;

/// Per-connection ACP agent state.
///
/// A fresh instance is built per ACP connection by `build_agent_state` — it is **not** shared
/// across connections. Wraps session management, configuration, and per-session tool
/// executors. Pass an `Arc<ZephAcpAgentState>` to [`run_agent`] to drive the dispatch loop.
pub struct ZephAcpAgentState {
    pub(crate) spawner: AgentSpawner,
    pub(crate) sessions: SessionMap,
    pub(crate) agent_name: String,
    agent_version: String,
    max_sessions: usize,
    idle_timeout: std::time::Duration,
    pub(crate) store: Option<SqliteStore>,
    /// Directory for durable per-session JSONL event logs (spec-068, #5343). `Some` when
    /// `[session] enabled = true`; enables `ForkEngine`-based forking in `fork_conversation`.
    pub(crate) session_data_dir: Option<std::path::PathBuf>,
    permission_file: Option<std::path::PathBuf>,
    /// IDE capabilities received during `initialize()`; used by `build_acp_context`.
    pub(crate) client_caps: RwLock<acp::schema::v1::ClientCapabilities>,
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
    /// Supervisor for long-lived agent-level background tasks (idle reaper, etc.).
    task_supervisor: TaskSupervisor,
    /// Canonicalized allowlist of directories ACP clients may reference in session requests.
    additional_directories_allow: Vec<std::path::PathBuf>,
    /// Auth methods to advertise in the `initialize` response. MVP: always `[Agent]`.
    auth_methods_config: Vec<zeph_core::config::AcpAuthMethod>,
    /// Timeout configuration for ACP operations (terminal, elicitation, MCP bridge).
    pub(crate) timeouts: zeph_config::AcpTimeoutsConfig,
    /// Model-related configuration parameters (from `[acp.model_config]`).
    pub(crate) model_config: zeph_config::AcpModelConfigConfig,
    /// Injection-detection-only sanitizer for advisory scanning of inbound ACP prompts.
    ///
    /// Spotlight wrapping is explicitly disabled: operator-typed prompts must not be
    /// repackaged as untrusted data. The sanitizer is used solely for logging injection
    /// pattern matches so anomalies are visible in traces and metrics.
    prompt_injection_detector: ContentSanitizer,
    /// Whether the IDE advertised elicitation capability during `initialize()`.
    #[cfg(feature = "unstable-elicitation")]
    pub(crate) elicitation_supported: std::sync::atomic::AtomicBool,
    /// Available provider names from `[[llm.providers]]` configuration.
    ///
    /// Used by `providers/list` to build the response without exposing vault keys.
    /// Each entry pairs the provider name with its protocol type.
    #[cfg(feature = "unstable-llm-providers")]
    pub(crate) provider_names: Vec<(String, agent_client_protocol_schema::v1::LlmProtocol)>,
    /// Connection-scoped disabled providers (no `session_id` in ACP schema).
    #[cfg(feature = "unstable-llm-providers")]
    pub(crate) global_disabled_providers: Mutex<HashSet<String>>,
    /// Connection-scoped provider overrides (no `session_id` in ACP schema).
    #[cfg(feature = "unstable-llm-providers")]
    pub(crate) global_provider_overrides: Mutex<HashMap<String, ProviderSetOverride>>,
    /// Authenticated identity of this connection (#5868), scoping persisted ACP session
    /// list/load/resume. `"acp-local"` for stdio and unauthenticated HTTP; the matched
    /// bearer-token client id for authenticated HTTP/WS. Set once in `build_agent_state`.
    pub(crate) owner_key: String,
}

/// Backward-compatible alias.
pub type ZephAcpAgent = ZephAcpAgentState;

impl ZephAcpAgentState {
    /// Returns the `cancel_signal` for `session_id`, if the session is currently in memory.
    ///
    /// Used to bridge the real ACP `$/cancel_request` protocol notification onto the same
    /// internal signal `session/cancel` already notifies (see `handlers/prompt.rs`).
    #[cfg(feature = "unstable-cancel-request")]
    pub(crate) fn session_cancel_signal(
        &self,
        session_id: &acp::schema::v1::SessionId,
    ) -> Option<Arc<tokio::sync::Notify>> {
        self.sessions
            .lock()
            .get(session_id)
            .map(|entry| Arc::clone(&entry.cancel_signal))
    }

    #[allow(clippy::too_many_arguments)] // function with many required inputs; a *Params struct would be more verbose without simplifying the call site
    pub(crate) async fn build_acp_context(
        &self,
        session_id: &acp::schema::v1::SessionId,
        cx: &acp::ConnectionTo<acp::Client>,
        cancel_signal: Arc<tokio::sync::Notify>,
        provider_override: Arc<RwLock<Option<AnyProvider>>>,
        cwd: PathBuf,
        notify_tx: NotifySender,
        #[cfg(feature = "unstable-elicitation")] elicitation_tx: Option<
            elicitation::ElicitationSender,
        >,
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
        // EXEMPT(#5144): per-session handler tied to connection lifetime; many concurrent
        // sessions → static name collision under TaskSupervisor::spawn. Self-terminating
        // when the connection or cancel_signal closes.
        tokio::spawn(perm_handler);

        let (fs_exec, fs_handler) = AcpFileExecutor::new(
            Arc::clone(&conn),
            session_id.clone(),
            can_read,
            can_write,
            cwd,
            Some(perm_gate.clone()),
        )
        .await;
        // EXEMPT(#5144): per-session handler, same reasoning as perm_handler above.
        tokio::spawn(fs_handler);

        let (shell_exec, shell_handler) = AcpShellExecutor::new(
            Arc::clone(&conn),
            session_id.clone(),
            Some(perm_gate.clone()),
            self.timeouts.terminal_secs,
        );
        // EXEMPT(#5144): per-session handler, same reasoning as perm_handler above.
        tokio::spawn(shell_handler);

        let lsp_provider = if ide_supports_lsp {
            let (provider, lsp_handler) = crate::lsp::AcpLspProvider::new(
                Arc::clone(&conn),
                true,
                self.lsp_config.request_timeout_secs,
                self.lsp_config.max_references,
                self.lsp_config.max_workspace_symbols,
            );
            // EXEMPT(#5144): per-session handler, same reasoning as perm_handler above.
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
            status_notifier: SessionStatusNotifier::new(notify_tx, session_id.clone()),
            #[cfg(feature = "unstable-elicitation")]
            elicitation_bridge: elicitation_tx.map(|tx| elicitation::ElicitationBridge {
                tx,
                timeout_secs: self.timeouts.elicitation_secs,
            }),
        }
    }

    pub(crate) async fn send_notification(
        &self,
        session_id: &acp::schema::v1::SessionId,
        notification: acp::schema::v1::SessionNotification,
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
        let timeout = std::time::Duration::from_millis(self.timeouts.notify_ack_timeout_ms);
        tokio::time::timeout(timeout, ack_rx)
            .await
            .map_err(|_| {
                tracing::warn!(
                    timeout_ms = self.timeouts.notify_ack_timeout_ms,
                    "notification ack timed out — IDE client may be hung"
                );
                acp::Error::internal_error().data("notification ack timed out")
            })?
            .map_err(|_| acp::Error::internal_error().data("notification ack lost"))
    }

    /// Fire-and-forget notification via the session's notify channel (no ack).
    pub(crate) fn send_notification_nowait(
        &self,
        session_id: &acp::schema::v1::SessionId,
        notification: acp::schema::v1::SessionNotification,
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
}

/// Handler implementations — called from `run_agent` handler closures.
impl ZephAcpAgentState {
    #[allow(clippy::unused_async)]
    #[tracing::instrument(skip_all, name = "acp.handler.initialize")]
    pub(crate) async fn do_initialize(
        &self,
        args: acp::schema::v1::InitializeRequest,
    ) -> acp::Result<acp::schema::v1::InitializeResponse> {
        tracing::debug!("ACP initialize");
        #[cfg(feature = "unstable-elicitation")]
        {
            let supports = args.client_capabilities.elicitation.is_some();
            self.elicitation_supported
                .store(supports, std::sync::atomic::Ordering::Relaxed);
            tracing::debug!(
                elicitation_supported = supports,
                "ACP initialize: elicitation capability"
            );
        }
        *self.client_caps.write() = args.client_capabilities;
        let title = format!("{} AI Agent", self.agent_name);

        // stdio transport implies a trusted local client; do not expose internal
        // configuration details. Provide only a generic authentication hint.
        let mut meta = serde_json::Map::new();
        meta.insert(
            "auth_hint".to_owned(),
            serde_json::json!("authentication required"),
        );

        let mut caps = acp::schema::v1::AgentCapabilities::new()
            .load_session(true)
            .prompt_capabilities(
                acp::schema::v1::PromptCapabilities::new()
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
            caps = caps.mcp_capabilities(
                acp::schema::v1::McpCapabilities::new()
                    .http(true)
                    .sse(false),
            );
        }
        let caps = {
            let mut session_caps = acp::schema::v1::SessionCapabilities::new();
            session_caps = session_caps.list(acp::schema::v1::SessionListCapabilities::default());
            {
                session_caps =
                    session_caps.close(acp::schema::v1::SessionCloseCapabilities::default());
            }
            #[cfg(feature = "unstable-session-fork")]
            {
                session_caps =
                    session_caps.fork(acp::schema::v1::SessionForkCapabilities::default());
            }
            {
                session_caps =
                    session_caps.resume(acp::schema::v1::SessionResumeCapabilities::default());
            }
            caps.session_capabilities(session_caps)
        };

        let caps = caps.auth(
            acp::schema::v1::AgentAuthCapabilities::default()
                .logout(acp::schema::v1::LogoutCapabilities::default()),
        );

        let auth_methods: Vec<acp::schema::v1::AuthMethod> = self
            .auth_methods_config
            .iter()
            .map(|_m| {
                acp::schema::v1::AuthMethod::Agent(acp::schema::v1::AuthMethodAgent::new(
                    "zeph", "Zeph",
                ))
            })
            .collect();

        Ok(
            acp::schema::v1::InitializeResponse::new(acp::schema::ProtocolVersion::LATEST)
                .auth_methods(auth_methods)
                .agent_info(
                    acp::schema::v1::Implementation::new(&self.agent_name, &self.agent_version)
                        .title(title),
                )
                .agent_capabilities(caps)
                .meta(meta),
        )
    }

    #[tracing::instrument(skip_all, name = "acp.handler.dispatch")]
    pub(crate) async fn do_ext_method(
        &self,
        args: acp::schema::v1::ExtRequest,
    ) -> acp::Result<acp::schema::v1::ExtResponse> {
        if let Some(fut) = crate::custom::dispatch(self, &args) {
            return fut.await;
        }
        #[cfg(feature = "unstable-llm-providers")]
        {
            if let Some(resp) = self.ext_method_providers(&args)? {
                return Ok(resp);
            }
        }
        self.ext_method_mcp(&args).await
    }

    pub(crate) async fn do_ext_notification(
        &self,
        args: acp::schema::v1::ExtNotification,
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
        _args: acp::schema::v1::AuthenticateRequest,
    ) -> acp::Result<acp::schema::v1::AuthenticateResponse> {
        Ok(acp::schema::v1::AuthenticateResponse::default())
    }

    #[allow(clippy::unused_async)]
    #[tracing::instrument(skip_all, name = "acp.handler.logout")]
    pub(crate) async fn do_logout(
        &self,
        _args: acp::schema::v1::LogoutRequest,
    ) -> acp::Result<acp::schema::v1::LogoutResponse> {
        tracing::debug!("ACP logout (no-op: vault-based auth)");
        Ok(acp::schema::v1::LogoutResponse::default())
    }

    #[allow(clippy::unused_async)]
    #[tracing::instrument(skip_all, name = "acp.handler.cancel", fields(session_id = %args.session_id))]
    pub(crate) async fn do_cancel(
        &self,
        args: acp::schema::v1::CancelNotification,
    ) -> acp::Result<()> {
        tracing::debug!(session_id = %args.session_id, "ACP cancel");
        if let Some(entry) = self.sessions.lock().get(&args.session_id) {
            entry.cancel_signal.notify_one();
        }
        Ok(())
    }
}

/// Map one durable [`zeph_session::SessionEvent`] to the ACP `SessionUpdate`(s) it replays as
/// (spec-068 §12.3, D-2's ACP read-handler cutover).
///
/// `SessionStarted`/`ForkPoint`/`Condensation`/`Compaction`/`ModelChanged`/`SessionEnded` are
/// session-log bookkeeping, not turn content — they produce no client-visible notification.
/// `ToolCall`/`ToolResult` are handled for schema completeness even though no production write
/// path currently emits them (today, tool use/results are embedded as `MessagePart::ToolUse`/text
/// inside `AssistantMessage`/`UserMessage` via `persist_message`).
///
/// Pure and side-effect-free so the event-to-notification mapping is unit-testable without the
/// full `serve_connection` ACP harness — see `tests::session_event_to_updates_*` below.
fn session_event_to_updates(
    event: zeph_session::SessionEvent,
) -> Vec<acp::schema::v1::SessionUpdate> {
    match event {
        zeph_session::SessionEvent::UserMessage { text, .. } => {
            vec![acp::schema::v1::SessionUpdate::UserMessageChunk(
                acp::schema::v1::ContentChunk::new(text.into()),
            )]
        }
        zeph_session::SessionEvent::AssistantMessage { parts } => parts
            .into_iter()
            .filter_map(|part| match part {
                zeph_llm::provider::MessagePart::ToolUse { id, name, input } => {
                    Some(acp::schema::v1::SessionUpdate::ToolCall(
                        acp::schema::v1::ToolCall::new(id, name).raw_input(input),
                    ))
                }
                other => other.as_plain_text().map(|text| {
                    acp::schema::v1::SessionUpdate::AgentMessageChunk(
                        acp::schema::v1::ContentChunk::new(text.to_owned().into()),
                    )
                }),
            })
            .collect(),
        zeph_session::SessionEvent::ToolCall { id, name, input } => {
            vec![acp::schema::v1::SessionUpdate::ToolCall(
                acp::schema::v1::ToolCall::new(id, name).raw_input(input),
            )]
        }
        zeph_session::SessionEvent::ToolResult {
            id,
            output,
            is_error,
            ..
        } => {
            let status = if is_error {
                acp::schema::v1::ToolCallStatus::Failed
            } else {
                acp::schema::v1::ToolCallStatus::Completed
            };
            vec![acp::schema::v1::SessionUpdate::ToolCallUpdate(
                acp::schema::v1::ToolCallUpdate::new(
                    id,
                    acp::schema::v1::ToolCallUpdateFields::new()
                        .status(status)
                        .content(vec![output.into()]),
                ),
            )]
        }
        zeph_session::SessionEvent::SessionStarted { .. }
        | zeph_session::SessionEvent::ForkPoint { .. }
        | zeph_session::SessionEvent::Condensation { .. }
        | zeph_session::SessionEvent::Compaction { .. }
        | zeph_session::SessionEvent::ModelChanged { .. }
        | zeph_session::SessionEvent::SessionEnded { .. } => Vec::new(),
    }
}

/// Returns `true` when `trimmed_text` is an ACP-native slash command that should
/// be handled by [`ZephAcpAgentState::handle_slash_command`] rather than forwarded
/// to the agent loop.
///
/// `/review` is deliberately absent: `do_prompt` (`turn.rs`) intercepts it before this check
/// even runs, expanding it into a real prompt that flows through the normal turn machinery
/// instead of `handle_slash_command`'s synchronous short-circuit reply (#6673).
fn is_acp_native_slash_command(trimmed_text: &str) -> bool {
    trimmed_text == "/help"
        || trimmed_text.starts_with("/help ")
        || trimmed_text == "/mode"
        || trimmed_text.starts_with("/mode ")
        || trimmed_text == "/clear"
        || trimmed_text == "/model"
        || trimmed_text.starts_with("/model ")
}

/// Populate model caches for a single provider, then expand every other unique provider slug
/// present in `available_models` from its on-disk cache only (no extra network calls).
///
/// Used both at ACP startup (`src/acp.rs`, to warm every configured provider's cache before the
/// server starts accepting connections — one call per provider there) and by `/model refresh`
/// (`ZephAcpAgentState::model_refresh_as_string`, #5986) for the session's single currently
/// active provider — mirroring `Agent::model_refresh_as_string`
/// (`crates/zeph-core/src/agent/model_commands.rs`), which likewise refreshes only the active
/// provider rather than looping over every configured one.
///
/// Uses a 5-second timeout so that a slow or unavailable provider does not block the caller.
/// Returns the number of models fetched from the live network call (`0` on error or timeout);
/// the on-disk cache expansion for other slugs always runs regardless of that outcome.
pub async fn warm_model_caches(
    provider: zeph_llm::any::AnyProvider,
    available_models: SharedAvailableModels,
) -> usize {
    use zeph_llm::model_cache::ModelCache;

    let provider_count = {
        let models = available_models.read();
        models
            .iter()
            .filter_map(|k| k.split_once(':').map(|(slug, _)| slug))
            .collect::<std::collections::HashSet<_>>()
            .len()
    };
    tracing::info!(
        providers = provider_count,
        "warming model caches in background"
    );

    let fetch = async move {
        match provider.list_models_remote().await {
            Ok(models) => {
                let count = models.len();
                tracing::info!(models = count, "model cache fetch completed");
                count
            }
            Err(e) => {
                tracing::info!(error = %e, "model cache warm-up failed; keeping fallback list");
                0
            }
        }
    };

    let Ok(fetched) = tokio::time::timeout(std::time::Duration::from_secs(5), fetch).await else {
        tracing::info!("model cache warm-up timed out; keeping fallback list");
        return 0;
    };

    // Collect unique provider slugs from the current available_models list.
    let slugs: Vec<String> = {
        let models = available_models.read();
        models
            .iter()
            .filter_map(|k| k.split_once(':').map(|(s, _)| s.to_owned()))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect()
    };

    for slug in slugs {
        let cache = ModelCache::for_slug(&slug);
        if cache.is_stale_async().await {
            tracing::info!(provider = %slug, "model cache still stale after warm-up");
            continue;
        }
        if let Ok(Some(entries)) = cache.load_async().await
            && !entries.is_empty()
        {
            let new_keys: Vec<String> = entries
                .into_iter()
                .map(|m| format!("{slug}:{}", m.id))
                .collect();
            let count = new_keys.len();
            let mut models = available_models.write();
            models.retain(|k| !k.starts_with(&format!("{slug}:")));
            models.extend(new_keys);
            models.dedup();
            tracing::info!(provider = %slug, models = count, "model cache ready");
        }
    }
    let total_models = available_models.read().len();
    tracing::info!(models = total_models, "model cache warming finished");
    fetched
}

/// Map `(cancelled, stop_hint)` to the ACP `StopReason` wire value.
fn compute_stop_reason(
    cancelled: bool,
    stop_hint: Option<StopHint>,
) -> acp::schema::v1::StopReason {
    if cancelled {
        acp::schema::v1::StopReason::Cancelled
    } else {
        match stop_hint {
            Some(StopHint::MaxTokens) => acp::schema::v1::StopReason::MaxTokens,
            Some(StopHint::MaxTurnRequests) => acp::schema::v1::StopReason::MaxTurnRequests,
            None | Some(_) => acp::schema::v1::StopReason::EndTurn,
        }
    }
}

/// Construct the `PromptResponse`, attaching per-turn token usage when the
/// `unstable-session-usage` feature is enabled.
fn build_prompt_response(
    stop_reason: acp::schema::v1::StopReason,
    #[cfg(feature = "unstable-session-usage")] turn_usage: TurnUsage,
) -> acp::schema::v1::PromptResponse {
    let r = acp::schema::v1::PromptResponse::new(stop_reason);
    #[cfg(feature = "unstable-session-usage")]
    let r = {
        let total = turn_usage
            .input_tokens
            .saturating_add(turn_usage.output_tokens);
        let usage =
            acp::schema::v1::Usage::new(total, turn_usage.input_tokens, turn_usage.output_tokens)
                // thought_tokens: not tracked for MVP — provider may fold them into output_tokens
                .cached_read_tokens(
                    (turn_usage.cache_read_tokens > 0).then_some(turn_usage.cache_read_tokens),
                )
                .cached_write_tokens(
                    (turn_usage.cache_write_tokens > 0).then_some(turn_usage.cache_write_tokens),
                );
        r.usage(usage)
    };
    r
}

#[cfg(feature = "unstable-elicitation")]
pub(crate) mod elicitation;
#[cfg(feature = "unstable-llm-providers")]
mod providers;
#[cfg(feature = "unstable-llm-providers")]
pub(crate) use providers::ProviderSetOverride;
#[cfg(feature = "unstable-session-usage")]
mod usage;
#[cfg(feature = "unstable-session-usage")]
pub(crate) use usage::{SessionUsageAccumulator, TurnUsage};
pub(super) mod helpers;
use helpers::{
    DEFAULT_MODE_ID, DIAGNOSTICS_MIME_TYPE, build_available_commands, build_config_options,
    build_mode_state, format_diagnostics_block, loopback_event_to_updates, mime_to_ext, model_meta,
};
use zeph_common::text::xml_escape;

pub(crate) mod handlers;

mod builder;
mod lsp_events;
mod mcp_ext;
mod model;
mod reaper;
mod session;
mod slash;
mod turn;

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
#[allow(clippy::too_many_lines)]
pub async fn run_agent(
    state: Arc<ZephAcpAgentState>,
    transport: impl acp::ConnectTo<acp::Agent>,
) -> acp::Result<()> {
    #[cfg(feature = "unstable-session-fork")]
    use handlers::fork_session;
    use handlers::{
        authenticate, cancel, close_session, delete_session, dispatch, initialize, list_sessions,
        load_session, logout, new_session, prompt, resume_session, set_session_config_option,
        set_session_mode,
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

    let builder = builder.on_receive_request(
        req_handler!(state, close_session::handle_close_session),
        acp::on_receive_request!(),
    );
    let builder = builder.on_receive_request(
        req_handler!(state, delete_session::handle_delete_session),
        acp::on_receive_request!(),
    );
    #[cfg(feature = "unstable-session-fork")]
    let builder = builder.on_receive_request(
        req_handler!(state, fork_session::handle_fork_session),
        acp::on_receive_request!(),
    );
    let builder = builder.on_receive_request(
        req_handler!(state, resume_session::handle_resume_session),
        acp::on_receive_request!(),
    );
    let builder = builder.on_receive_request(
        req_handler!(state, logout::handle_logout),
        acp::on_receive_request!(),
    );
    #[cfg(feature = "unstable-cancel-request")]
    let builder = builder.on_receive_notification(
        notif_handler!(state, handlers::cancel_request::handle_cancel_request),
        acp::on_receive_notification!(),
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

/// Regression tests for #4528: `send_notification` must not block indefinitely.
#[cfg(test)]
mod notify_timeout_tests {
    use std::sync::Arc;

    use parking_lot::RwLock;
    use zeph_core::channel::LoopbackChannel;
    use zeph_llm::any::AnyProvider;

    use super::*;

    fn make_agent_for_timeout() -> ZephAcpAgent {
        let spawner: AgentSpawner = Arc::new(|_ch, _ctx, _sc| Box::pin(async {}));
        let mut agent = ZephAcpAgent::new(spawner, 4, 1800, None);
        // Override to a very small value so the test finishes in ~50 ms.
        agent.timeouts.notify_ack_timeout_ms = 50;
        agent
    }

    /// `send_notification` must return an error within `notify_ack_timeout_ms` when
    /// no drainer is running (simulates a hung IDE client).
    #[tokio::test]
    async fn send_notification_returns_error_when_ack_times_out() {
        let agent = make_agent_for_timeout();
        let session_id = acp::schema::v1::SessionId::new("timeout-test".to_owned());

        let (_, handle) = LoopbackChannel::pair(4);
        let provider_override = Arc::new(RwLock::new(None::<AnyProvider>));
        let (notify_tx, notify_rx) = mpsc::channel(256);
        let entry = ZephAcpAgent::make_session_entry(
            handle,
            "test-model".to_owned(),
            std::path::PathBuf::from("."),
            None,
            provider_override,
            SessionConfigSeed {
                thinking_enabled: false,
                auto_approve_level: "suggest".to_owned(),
                temperature_preset: zeph_config::AcpTemperaturePreset::default(),
            },
            notify_tx,
            notify_rx,
        );
        // Insert without starting the drainer — no ack will ever be sent.
        agent.sessions.lock().insert(session_id.clone(), entry);

        let update = acp::schema::v1::SessionUpdate::AgentMessageChunk(
            acp::schema::v1::ContentChunk::new("hello".into()),
        );
        let notif = acp::schema::v1::SessionNotification::new(session_id.clone(), update);
        let result = agent.send_notification(&session_id, notif).await;
        assert!(
            result.is_err(),
            "send_notification must fail when ack does not arrive within the timeout"
        );
    }
}

/// Regression tests for #5519: `SessionStatusNotifier` pushes status updates immediately,
/// without waiting for a prompt-drain.
#[cfg(test)]
mod session_status_notifier_tests {
    use super::*;

    #[tokio::test]
    async fn notify_status_nowait_delivers_agent_thought_chunk_immediately() {
        let (notify_tx, mut notify_rx) = mpsc::channel(4);
        let session_id = acp::schema::v1::SessionId::new("notifier-test".to_owned());
        let notifier = SessionStatusNotifier::new(notify_tx, session_id.clone());

        notifier.notify_status_nowait("degraded");

        let (notification, _ack) = notify_rx.try_recv().expect(
            "notify_status_nowait must push onto the channel synchronously, without a drainer",
        );
        assert_eq!(notification.session_id, session_id);
        match notification.update {
            acp::schema::v1::SessionUpdate::AgentThoughtChunk(chunk) => match chunk.content {
                acp::schema::v1::ContentBlock::Text(t) => assert_eq!(t.text, "degraded"),
                other => panic!("expected ContentBlock::Text, got {other:?}"),
            },
            other => panic!("expected AgentThoughtChunk, got {other:?}"),
        }
    }

    /// Matches `loopback_event_to_updates`'s handling of `LoopbackEvent::Status("")`: empty
    /// text is a no-op, not an empty chunk.
    #[tokio::test]
    async fn notify_status_nowait_skips_empty_text() {
        let (notify_tx, mut notify_rx) = mpsc::channel(4);
        let session_id = acp::schema::v1::SessionId::new("notifier-empty-test".to_owned());
        let notifier = SessionStatusNotifier::new(notify_tx, session_id);

        notifier.notify_status_nowait("");

        assert!(notify_rx.try_recv().is_err(), "empty text must not be sent");
    }
}

/// Regression coverage for S1 (spec-068 §12.3 / D-2): `session_event_to_updates` is the mapping
/// `do_load_session` now uses to replay the durable JSONL event log instead of the emptied
/// `acp_session_events` table. Exercised directly (no ACP client/server harness needed) since the
/// function is pure.
#[cfg(test)]
mod session_event_replay_tests {
    use super::*;

    #[test]
    fn user_message_becomes_user_message_chunk() {
        let updates = session_event_to_updates(zeph_session::SessionEvent::UserMessage {
            text: "hello".to_owned(),
            image_refs: Vec::new(),
        });
        assert_eq!(updates.len(), 1);
        assert!(matches!(
            updates[0],
            acp::schema::v1::SessionUpdate::UserMessageChunk(_)
        ));
    }

    #[test]
    fn assistant_text_part_becomes_agent_message_chunk() {
        let updates = session_event_to_updates(zeph_session::SessionEvent::AssistantMessage {
            parts: vec![zeph_llm::provider::MessagePart::Text {
                text: "hi there".to_owned(),
            }],
        });
        assert_eq!(updates.len(), 1);
        assert!(matches!(
            updates[0],
            acp::schema::v1::SessionUpdate::AgentMessageChunk(_)
        ));
    }

    #[test]
    fn assistant_tool_use_part_becomes_tool_call() {
        let updates = session_event_to_updates(zeph_session::SessionEvent::AssistantMessage {
            parts: vec![zeph_llm::provider::MessagePart::ToolUse {
                id: "call_0".to_owned(),
                name: "shell".to_owned(),
                input: serde_json::json!({"cmd": "ls"}),
            }],
        });
        assert_eq!(updates.len(), 1);
        assert!(matches!(
            updates[0],
            acp::schema::v1::SessionUpdate::ToolCall(_)
        ));
    }

    #[test]
    fn assistant_message_maps_each_part_independently() {
        let updates = session_event_to_updates(zeph_session::SessionEvent::AssistantMessage {
            parts: vec![
                zeph_llm::provider::MessagePart::ToolUse {
                    id: "call_0".to_owned(),
                    name: "shell".to_owned(),
                    input: serde_json::json!({}),
                },
                zeph_llm::provider::MessagePart::Text {
                    text: "done".to_owned(),
                },
            ],
        });
        assert_eq!(updates.len(), 2);
        assert!(matches!(
            updates[0],
            acp::schema::v1::SessionUpdate::ToolCall(_)
        ));
        assert!(matches!(
            updates[1],
            acp::schema::v1::SessionUpdate::AgentMessageChunk(_)
        ));
    }

    #[test]
    fn tool_result_becomes_tool_call_update_with_status() {
        let updates = session_event_to_updates(zeph_session::SessionEvent::ToolResult {
            id: "call_0".to_owned(),
            name: "shell".to_owned(),
            output: "ok".to_owned(),
            is_error: false,
            duration_ms: 10,
        });
        assert_eq!(updates.len(), 1);
        let acp::schema::v1::SessionUpdate::ToolCallUpdate(update) = &updates[0] else {
            panic!("expected ToolCallUpdate");
        };
        assert_eq!(
            update.fields.status,
            Some(acp::schema::v1::ToolCallStatus::Completed)
        );
    }

    #[test]
    fn failed_tool_result_maps_to_failed_status() {
        let updates = session_event_to_updates(zeph_session::SessionEvent::ToolResult {
            id: "call_0".to_owned(),
            name: "shell".to_owned(),
            output: "boom".to_owned(),
            is_error: true,
            duration_ms: 10,
        });
        let acp::schema::v1::SessionUpdate::ToolCallUpdate(update) = &updates[0] else {
            panic!("expected ToolCallUpdate");
        };
        assert_eq!(
            update.fields.status,
            Some(acp::schema::v1::ToolCallStatus::Failed)
        );
    }

    #[test]
    fn bookkeeping_events_produce_no_client_visible_update() {
        assert!(
            session_event_to_updates(zeph_session::SessionEvent::SessionStarted {
                session_id: "s1".to_owned(),
                cwd: "/tmp".to_owned(),
                provider_name: "claude".to_owned(),
                model: "opus".to_owned(),
                forked_from: None,
            })
            .is_empty()
        );
        assert!(
            session_event_to_updates(zeph_session::SessionEvent::ForkPoint {
                new_session_id: "s2".to_owned(),
            })
            .is_empty()
        );
        assert!(
            session_event_to_updates(zeph_session::SessionEvent::SessionEnded {
                reason: "user_quit".to_owned(),
            })
            .is_empty()
        );
    }
}
