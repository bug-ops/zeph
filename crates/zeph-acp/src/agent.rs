// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::cell::RefCell;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use agent_client_protocol as acp;
use tokio::sync::{mpsc, oneshot};
use zeph_core::LoopbackEvent;
use zeph_core::channel::{ChannelMessage, LoopbackChannel};
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider as _;
use zeph_mcp::McpManager;
use zeph_mcp::manager::ServerEntry;
use zeph_memory::sqlite::SqliteStore;

use crate::fs::AcpFileExecutor;
use crate::permission::AcpPermissionGate;
use crate::terminal::AcpShellExecutor;
use crate::transport::ConnSlot;

/// Factory that creates a provider by `{provider}:{model}` key.
pub type ProviderFactory = Arc<dyn Fn(&str) -> Option<AnyProvider> + Send + Sync>;

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

/// IDE-proxied capabilities passed to the agent loop per session.
///
/// Each field is `None` when the IDE did not advertise the corresponding capability.
pub struct AcpContext {
    pub file_executor: Option<AcpFileExecutor>,
    pub shell_executor: Option<AcpShellExecutor>,
    pub permission_gate: Option<AcpPermissionGate>,
    /// Shared cancellation signal: notify to interrupt the running agent operation.
    pub cancel_signal: std::sync::Arc<tokio::sync::Notify>,
    /// Shared slot for runtime model switching via `set_session_config_option`.
    /// When `Some`, the agent should swap its provider before the next turn.
    pub provider_override: Arc<std::sync::RwLock<Option<AnyProvider>>>,
}

/// Factory: receives a [`LoopbackChannel`] and optional [`AcpContext`], runs the agent loop.
pub type AgentSpawner = Arc<
    dyn Fn(
            LoopbackChannel,
            Option<AcpContext>,
        ) -> Pin<Box<dyn std::future::Future<Output = ()> + 'static>>
        + 'static,
>;

/// Thread-safe variant of `AgentSpawner` required by the HTTP transport.
///
/// Used with `AcpHttpState` to satisfy `axum::State` requirements (`Send + Sync`).
#[cfg(feature = "acp-http")]
pub type SendAgentSpawner = Arc<
    dyn Fn(
            LoopbackChannel,
            Option<AcpContext>,
        ) -> Pin<Box<dyn std::future::Future<Output = ()> + 'static>>
        + Send
        + Sync
        + 'static,
>;

/// Sender half for delivering session notifications to the background writer.
pub(crate) type NotifySender =
    mpsc::UnboundedSender<(acp::SessionNotification, oneshot::Sender<()>)>;

pub(crate) struct SessionEntry {
    pub(crate) input_tx: mpsc::Sender<ChannelMessage>,
    // Receiver is owned solely by the prompt() handler; RefCell avoids Arc<Mutex> overhead.
    // prompt() is not called concurrently for the same session.
    pub(crate) output_rx: RefCell<Option<mpsc::Receiver<LoopbackEvent>>>,
    pub(crate) cancel_signal: std::sync::Arc<tokio::sync::Notify>,
    pub(crate) last_active: std::cell::Cell<std::time::Instant>,
    pub(crate) created_at: chrono::DateTime<chrono::Utc>,
    pub(crate) working_dir: RefCell<Option<std::path::PathBuf>>,
    /// Shared provider override slot; written by `set_session_config_option`, read by agent loop.
    provider_override: Arc<std::sync::RwLock<Option<AnyProvider>>>,
    /// Currently selected model identifier (display / tracking only).
    current_model: RefCell<String>,
    /// Current session mode (ask / architect / code).
    current_mode: RefCell<acp::SessionModeId>,
    /// Set after the first successful prompt so title generation fires only once.
    first_prompt_done: std::cell::Cell<bool>,
}

type SessionMap = Rc<RefCell<std::collections::HashMap<acp::SessionId, SessionEntry>>>;

pub struct ZephAcpAgent {
    notify_tx: NotifySender,
    spawner: AgentSpawner,
    pub(crate) sessions: SessionMap,
    conn_slot: ConnSlot,
    agent_name: String,
    agent_version: String,
    max_sessions: usize,
    idle_timeout: std::time::Duration,
    pub(crate) store: Option<SqliteStore>,
    permission_file: Option<std::path::PathBuf>,
    // IDE capabilities received during initialize(); used by build_acp_context.
    client_caps: RefCell<acp::ClientCapabilities>,
    /// Factory for creating a new provider by `{provider}:{model}` key.
    provider_factory: Option<ProviderFactory>,
    /// Available model identifiers advertised in `new_session` `config_options`.
    available_models: Vec<String>,
    /// Shared MCP manager for `ext_method` add/remove/list.
    mcp_manager: Option<Arc<McpManager>>,
}

impl ZephAcpAgent {
    pub fn new(
        spawner: AgentSpawner,
        notify_tx: NotifySender,
        conn_slot: ConnSlot,
        max_sessions: usize,
        session_idle_timeout_secs: u64,
        permission_file: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            notify_tx,
            spawner,
            sessions: Rc::new(RefCell::new(std::collections::HashMap::new())),
            conn_slot,
            agent_name: "zeph".to_owned(),
            agent_version: env!("CARGO_PKG_VERSION").to_owned(),
            max_sessions,
            idle_timeout: std::time::Duration::from_secs(session_idle_timeout_secs),
            store: None,
            permission_file,
            client_caps: RefCell::new(acp::ClientCapabilities::default()),
            provider_factory: None,
            available_models: Vec::new(),
            mcp_manager: None,
        }
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
        available_models: Vec<String>,
    ) -> Self {
        self.provider_factory = Some(factory);
        self.available_models = available_models;
        self
    }

    #[must_use]
    pub fn with_mcp_manager(mut self, manager: Arc<McpManager>) -> Self {
        self.mcp_manager = Some(manager);
        self
    }

    /// Spawn a background task that periodically evicts idle sessions.
    ///
    /// Must be called from within a `LocalSet` context.
    pub fn start_idle_reaper(&self) {
        let sessions = Rc::clone(&self.sessions);
        let idle_timeout = self.idle_timeout;
        tokio::task::spawn_local(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            interval.tick().await; // skip first tick
            loop {
                interval.tick().await;
                let now = std::time::Instant::now();
                let expired: Vec<acp::SessionId> = sessions
                    .borrow()
                    .iter()
                    .filter(|(_, e)| {
                        // Only evict idle sessions (output_rx is Some = not busy).
                        e.output_rx.borrow().is_some()
                            && now.duration_since(e.last_active.get()) > idle_timeout
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in expired {
                    if let Some(entry) = sessions.borrow_mut().remove(&id) {
                        entry.cancel_signal.notify_one();
                        tracing::debug!(session_id = %id, "evicted idle ACP session (timeout)");
                    }
                }
            }
        });
    }

    fn build_acp_context(
        &self,
        session_id: &acp::SessionId,
        cancel_signal: std::sync::Arc<tokio::sync::Notify>,
        provider_override: Arc<std::sync::RwLock<Option<AnyProvider>>>,
    ) -> Option<AcpContext> {
        let conn_guard = self.conn_slot.borrow();
        let conn = conn_guard.as_ref()?;

        let (perm_gate, perm_handler) =
            AcpPermissionGate::new(Rc::clone(conn), self.permission_file.clone());
        tokio::task::spawn_local(perm_handler);

        // Use actual IDE capabilities from initialize(); default to false (deny by default).
        let caps = self.client_caps.borrow();
        let can_read = caps.fs.read_text_file;
        let can_write = caps.fs.write_text_file;
        drop(caps);

        let (fs_exec, fs_handler) =
            AcpFileExecutor::new(Rc::clone(conn), session_id.clone(), can_read, can_write);
        tokio::task::spawn_local(fs_handler);

        let (shell_exec, shell_handler) = AcpShellExecutor::new(
            Rc::clone(conn),
            session_id.clone(),
            Some(perm_gate.clone()),
            120,
        );
        tokio::task::spawn_local(shell_handler);

        Some(AcpContext {
            file_executor: Some(fs_exec),
            shell_executor: Some(shell_exec),
            permission_gate: Some(perm_gate),
            cancel_signal,
            provider_override,
        })
    }

    async fn send_notification(&self, notification: acp::SessionNotification) -> acp::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.notify_tx
            .send((notification, tx))
            .map_err(|_| acp::Error::internal_error().data("notification channel closed"))?;
        rx.await
            .map_err(|_| acp::Error::internal_error().data("notification ack lost"))
    }
}

#[derive(serde::Deserialize)]
struct McpRemoveParams {
    id: String,
}

#[async_trait::async_trait(?Send)]
#[allow(clippy::too_many_lines)]
impl acp::Agent for ZephAcpAgent {
    async fn initialize(
        &self,
        args: acp::InitializeRequest,
    ) -> acp::Result<acp::InitializeResponse> {
        tracing::debug!("ACP initialize");
        *self.client_caps.borrow_mut() = args.client_capabilities;
        let title = format!("{} AI Agent", self.agent_name);

        // stdio transport implies a trusted local client; do not expose internal
        // configuration details. Provide only a generic authentication hint.
        let mut meta = serde_json::Map::new();
        meta.insert(
            "auth_hint".to_owned(),
            serde_json::json!("authentication required"),
        );

        let caps = acp::AgentCapabilities::new()
            .load_session(true)
            .prompt_capabilities(
                acp::PromptCapabilities::new()
                    .image(true)
                    .embedded_context(true),
            )
            .meta({
                let mut cap_meta = serde_json::Map::new();
                cap_meta.insert("config_options".to_owned(), serde_json::json!(true));
                cap_meta.insert("ext_methods".to_owned(), serde_json::json!(true));
                cap_meta
            });
        #[cfg(any(
            feature = "unstable-session-list",
            feature = "unstable-session-fork",
            feature = "unstable-session-resume",
        ))]
        let caps = {
            let mut session_caps = acp::SessionCapabilities::new();
            #[cfg(feature = "unstable-session-list")]
            {
                session_caps = session_caps.list(acp::SessionListCapabilities::default());
            }
            #[cfg(feature = "unstable-session-fork")]
            {
                session_caps = session_caps.fork(acp::SessionForkCapabilities::default());
            }
            #[cfg(feature = "unstable-session-resume")]
            {
                session_caps = session_caps.resume(acp::SessionResumeCapabilities::default());
            }
            caps.session_capabilities(session_caps)
        };

        Ok(acp::InitializeResponse::new(acp::ProtocolVersion::LATEST)
            .agent_info(
                acp::Implementation::new(&self.agent_name, &self.agent_version).title(title),
            )
            .agent_capabilities(caps)
            .meta(meta))
    }

    async fn ext_method(&self, args: acp::ExtRequest) -> acp::Result<acp::ExtResponse> {
        if let Some(fut) = crate::custom::dispatch(self, &args) {
            return fut.await;
        }
        // Fall through to inline MCP management methods from main.
        // Defined below in the second ext_method block merged from origin/main.
        self.ext_method_mcp(&args).await
    }

    async fn ext_notification(&self, args: acp::ExtNotification) -> acp::Result<()> {
        tracing::debug!(method = %args.method, "received ext_notification");
        Ok(())
    }

    async fn authenticate(
        &self,
        _args: acp::AuthenticateRequest,
    ) -> acp::Result<acp::AuthenticateResponse> {
        // stdio transport: authentication is a no-op, IDE client is trusted.
        Ok(acp::AuthenticateResponse::default())
    }

    async fn new_session(
        &self,
        _args: acp::NewSessionRequest,
    ) -> acp::Result<acp::NewSessionResponse> {
        // LRU eviction: find and remove the oldest idle (non-busy) session when at limit.
        if self.sessions.borrow().len() >= self.max_sessions {
            let evict_id = {
                let sessions = self.sessions.borrow();
                sessions
                    .iter()
                    .filter(|(_, e)| e.output_rx.borrow().is_some())
                    .min_by_key(|(_, e)| e.last_active.get())
                    .map(|(id, _)| id.clone())
            };
            match evict_id {
                Some(id) => {
                    if let Some(entry) = self.sessions.borrow_mut().remove(&id) {
                        entry.cancel_signal.notify_one();
                        tracing::debug!(session_id = %id, "evicted idle ACP session (LRU)");
                    }
                }
                None => {
                    return Err(acp::Error::internal_error().data("session limit reached"));
                }
            }
        }

        let session_id = acp::SessionId::new(uuid::Uuid::new_v4().to_string());
        tracing::debug!(%session_id, "new ACP session");

        let (channel, handle) = LoopbackChannel::pair(LOOPBACK_CHANNEL_CAPACITY);
        // Clone once for build_acp_context; ownership of the original moves into SessionEntry.
        let cancel_signal = std::sync::Arc::clone(&handle.cancel_signal);
        let provider_override: Arc<std::sync::RwLock<Option<AnyProvider>>> =
            Arc::new(std::sync::RwLock::new(None));
        let provider_override_for_ctx = Arc::clone(&provider_override);

        let entry = SessionEntry {
            input_tx: handle.input_tx,
            output_rx: RefCell::new(Some(handle.output_rx)),
            cancel_signal: handle.cancel_signal,
            last_active: std::cell::Cell::new(std::time::Instant::now()),
            created_at: chrono::Utc::now(),
            working_dir: RefCell::new(None),
            provider_override,
            current_model: RefCell::new(String::new()),
            current_mode: RefCell::new(acp::SessionModeId::new(DEFAULT_MODE_ID)),
            first_prompt_done: std::cell::Cell::new(false),
        };
        self.sessions.borrow_mut().insert(session_id.clone(), entry);

        if let Some(ref store) = self.store {
            let sid = session_id.to_string();
            let store = store.clone();
            tokio::task::spawn_local(async move {
                if let Err(e) = store.create_acp_session(&sid).await {
                    tracing::warn!(error = %e, "failed to persist ACP session");
                }
            });
        }

        let acp_ctx = self.build_acp_context(&session_id, cancel_signal, provider_override_for_ctx);
        let spawner = Arc::clone(&self.spawner);
        tokio::task::spawn_local(async move {
            (spawner)(channel, acp_ctx).await;
        });

        let config_options = build_model_config_options(&self.available_models, "");
        let default_mode_id = acp::SessionModeId::new(DEFAULT_MODE_ID);
        let mut resp = acp::NewSessionResponse::new(session_id.clone())
            .modes(build_mode_state(&default_mode_id));
        if !config_options.is_empty() {
            resp = resp.config_options(config_options);
        }

        let cmds_update = acp::SessionUpdate::AvailableCommandsUpdate(
            acp::AvailableCommandsUpdate::new(build_available_commands()),
        );
        let (tx, _rx) = oneshot::channel();
        self.notify_tx
            .send((acp::SessionNotification::new(session_id, cmds_update), tx))
            .ok();

        Ok(resp)
    }

    #[allow(clippy::too_many_lines)]
    async fn prompt(&self, args: acp::PromptRequest) -> acp::Result<acp::PromptResponse> {
        tracing::debug!(session_id = %args.session_id, "ACP prompt");

        let mut text = String::new();
        let mut attachments = Vec::new();
        for block in &args.prompt {
            match block {
                acp::ContentBlock::Text(t) => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&t.text);
                }
                acp::ContentBlock::Image(img) => {
                    if !SUPPORTED_IMAGE_MIMES.contains(&img.mime_type.as_str()) {
                        tracing::debug!(
                            mime_type = %img.mime_type,
                            "unsupported image MIME type in ACP prompt, skipping"
                        );
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
                                tracing::debug!(
                                    error = %e,
                                    "failed to decode image base64, skipping"
                                );
                            }
                        }
                    }
                }
                acp::ContentBlock::Resource(embedded) => {
                    if let acp::EmbeddedResourceResource::TextResourceContents(res) =
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
                            tracing::debug!(
                                mime_type = ?res.mime_type,
                                uri = %res.uri,
                                "unknown resource mime type — skipping"
                            );
                        } else {
                            text.push_str("<resource name=\"");
                            text.push_str(&res.uri.replace('"', "&quot;"));
                            text.push_str("\">");
                            text.push_str(&res.text);
                            text.push_str("</resource>");
                        }
                    }
                }
                acp::ContentBlock::Audio(_) => {
                    tracing::warn!("unsupported content block: Audio — skipping");
                }
                acp::ContentBlock::ResourceLink(link) => {
                    tracing::warn!(uri = %link.uri, "unsupported content block: ResourceLink — skipping");
                }
                &_ => {
                    tracing::warn!("unsupported content block: unknown — skipping");
                }
            }
        }

        if text.len() > MAX_PROMPT_BYTES {
            return Err(acp::Error::invalid_request().data("prompt too large"));
        }

        let trimmed_text = text.trim_start();
        if trimmed_text.starts_with('/') && trimmed_text != "/compact" {
            return self
                .handle_slash_command(&args.session_id, trimmed_text)
                .await;
        }

        let (input_tx, output_rx) = {
            let sessions = self.sessions.borrow();
            let entry = sessions
                .get(&args.session_id)
                .ok_or_else(|| acp::Error::internal_error().data("session not found"))?;
            let rx =
                entry.output_rx.borrow_mut().take().ok_or_else(|| {
                    acp::Error::internal_error().data("prompt already in progress")
                })?;
            entry.last_active.set(std::time::Instant::now());
            (entry.input_tx.clone(), rx)
        };

        // Persist user message before sending to agent.
        if let Some(ref store) = self.store {
            let sid = args.session_id.to_string();
            let payload = text.clone();
            let store = store.clone();
            tokio::task::spawn_local(async move {
                if let Err(e) = store.save_acp_event(&sid, "user_message", &payload).await {
                    tracing::warn!(error = %e, "failed to persist user message");
                }
            });
        }

        input_tx
            .send(ChannelMessage {
                text: text.clone(),
                attachments,
            })
            .await
            .map_err(|_| acp::Error::internal_error().data("agent channel closed"))?;

        // Grab the cancel_signal so we can detect cancellation during the drain loop.
        let cancel_signal = self
            .sessions
            .borrow()
            .get(&args.session_id)
            .map(|e| std::sync::Arc::clone(&e.cancel_signal));

        // Block until the agent finishes this turn (signals via Flush or channel close).
        let mut rx = output_rx;
        let mut cancelled = false;
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
            let is_flush = matches!(event, LoopbackEvent::Flush);
            for update in loopback_event_to_updates(event) {
                // Persist event before sending notification (best-effort).
                if let Some(ref store) = self.store {
                    let sid = args.session_id.to_string();
                    let (event_type, payload) = session_update_to_event(&update);
                    let store = store.clone();
                    tokio::task::spawn_local(async move {
                        if let Err(e) = store.save_acp_event(&sid, event_type, &payload).await {
                            tracing::warn!(error = %e, "failed to persist session event");
                        }
                    });
                }
                let notification = acp::SessionNotification::new(args.session_id.clone(), update);
                if let Err(e) = self.send_notification(notification).await {
                    tracing::warn!(error = %e, "failed to send notification");
                    break;
                }
            }
            if is_flush {
                break;
            }
        }

        // Return the receiver so future prompt() calls on this session can proceed.
        if let Some(entry) = self.sessions.borrow().get(&args.session_id) {
            *entry.output_rx.borrow_mut() = Some(rx);
        }

        let stop_reason = if cancelled {
            acp::StopReason::Cancelled
        } else {
            acp::StopReason::EndTurn
        };

        // Generate session title after first successful agent response (fire-and-forget).
        #[cfg(feature = "unstable-session-info-update")]
        if !cancelled {
            let should_generate = self
                .sessions
                .borrow()
                .get(&args.session_id)
                .is_some_and(|e| !e.first_prompt_done.get());
            if should_generate {
                if let Some(entry) = self.sessions.borrow().get(&args.session_id) {
                    entry.first_prompt_done.set(true);
                }
                if let Some(ref factory) = self.provider_factory
                    && let Some(model_key) = self.available_models.first()
                    && let Some(provider) = factory(model_key)
                {
                    let user_text = text.clone();
                    let sid = args.session_id.clone();
                    let store = self.store.clone();
                    let notify_tx = self.notify_tx.clone();
                    tokio::task::spawn_local(async move {
                        let prompt = format!(
                            "Generate a concise 5-7 word title for a conversation that starts \
                             with: {user_text}\nRespond with only the title, no quotes."
                        );
                        let messages = vec![zeph_llm::provider::Message::from_legacy(
                            zeph_llm::provider::Role::User,
                            &prompt,
                        )];
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(15),
                            provider.chat(&messages),
                        )
                        .await
                        {
                            Ok(Ok(title)) => {
                                let title = title.trim().to_owned();
                                if let Some(ref store) = store {
                                    let _ =
                                        store.update_session_title(&sid.to_string(), &title).await;
                                }
                                let update = acp::SessionUpdate::SessionInfoUpdate(
                                    acp::SessionInfoUpdate::new().title(title),
                                );
                                let notification = acp::SessionNotification::new(sid, update);
                                let (tx, _rx) = oneshot::channel();
                                notify_tx.send((notification, tx)).ok();
                            }
                            Ok(Err(e)) => {
                                tracing::debug!(error = %e, "title generation LLM call failed");
                            }
                            Err(_) => {
                                tracing::debug!("title generation timed out");
                            }
                        }
                    });
                }
            }
        }

        Ok(acp::PromptResponse::new(stop_reason))
    }

    async fn cancel(&self, args: acp::CancelNotification) -> acp::Result<()> {
        tracing::debug!(session_id = %args.session_id, "ACP cancel");
        // Signal the agent loop to stop, but keep the session alive — the IDE may
        // send another prompt on the same session_id after cancellation.
        if let Some(entry) = self.sessions.borrow().get(&args.session_id) {
            entry.cancel_signal.notify_one();
        }
        Ok(())
    }

    async fn load_session(
        &self,
        args: acp::LoadSessionRequest,
    ) -> acp::Result<acp::LoadSessionResponse> {
        // Session already in memory — nothing to restore.
        if self.sessions.borrow().contains_key(&args.session_id) {
            return Ok(acp::LoadSessionResponse::new());
        }

        // Try to restore from SQLite persistence.
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

        // Load events BEFORE spawning the agent loop to avoid orphaned sessions on error.
        let events = store
            .load_acp_events(&args.session_id.to_string())
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, session_id = %args.session_id, "failed to load ACP session events");
                acp::Error::internal_error().data("internal error")
            })?;

        // Rebuild agent loop for the restored session.
        let (channel, handle) = LoopbackChannel::pair(LOOPBACK_CHANNEL_CAPACITY);
        let cancel_signal = std::sync::Arc::clone(&handle.cancel_signal);
        let provider_override: Arc<std::sync::RwLock<Option<AnyProvider>>> =
            Arc::new(std::sync::RwLock::new(None));
        let provider_override_for_ctx = Arc::clone(&provider_override);
        let entry = SessionEntry {
            input_tx: handle.input_tx,
            output_rx: RefCell::new(Some(handle.output_rx)),
            cancel_signal: handle.cancel_signal,
            last_active: std::cell::Cell::new(std::time::Instant::now()),
            created_at: chrono::Utc::now(),
            working_dir: RefCell::new(None),
            provider_override,
            current_model: RefCell::new(String::new()),
            current_mode: RefCell::new(acp::SessionModeId::new(DEFAULT_MODE_ID)),
            first_prompt_done: std::cell::Cell::new(false),
        };
        self.sessions
            .borrow_mut()
            .insert(args.session_id.clone(), entry);

        let acp_ctx =
            self.build_acp_context(&args.session_id, cancel_signal, provider_override_for_ctx);
        let spawner = Arc::clone(&self.spawner);
        tokio::task::spawn_local(async move {
            (spawner)(channel, acp_ctx).await;
        });

        // Replay stored events as session/update notifications per ACP spec.

        for ev in events {
            let update = match ev.event_type.as_str() {
                "user_message" => {
                    acp::SessionUpdate::UserMessageChunk(acp::ContentChunk::new(ev.payload.into()))
                }
                "agent_message" => {
                    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(ev.payload.into()))
                }
                "agent_thought" => {
                    acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(ev.payload.into()))
                }
                "tool_call" => match serde_json::from_str::<acp::ToolCall>(&ev.payload) {
                    Ok(tc) => acp::SessionUpdate::ToolCall(tc),
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
            let notification = acp::SessionNotification::new(args.session_id.clone(), update);
            if let Err(e) = self.send_notification(notification).await {
                tracing::warn!(error = %e, "failed to replay notification");
                break;
            }
        }

        let default_mode_id = acp::SessionModeId::new(DEFAULT_MODE_ID);
        let load_resp = acp::LoadSessionResponse::new().modes(build_mode_state(&default_mode_id));

        let cmds_update = acp::SessionUpdate::AvailableCommandsUpdate(
            acp::AvailableCommandsUpdate::new(build_available_commands()),
        );
        let (tx, _rx) = oneshot::channel();
        self.notify_tx
            .send((
                acp::SessionNotification::new(args.session_id, cmds_update),
                tx,
            ))
            .ok();

        Ok(load_resp)
    }

    #[cfg(feature = "unstable-session-list")]
    async fn list_sessions(
        &self,
        args: acp::ListSessionsRequest,
    ) -> acp::Result<acp::ListSessionsResponse> {
        let sessions = self.sessions.borrow();
        let mut result = Vec::with_capacity(sessions.len());
        for (session_id, entry) in sessions.iter() {
            let working_dir = entry.working_dir.borrow().clone().unwrap_or_default();
            if let Some(ref filter) = args.cwd
                && &working_dir != filter
            {
                continue;
            }
            let info = acp::SessionInfo::new(session_id.clone(), working_dir)
                .updated_at(entry.created_at.to_rfc3339());
            result.push(info);
        }
        Ok(acp::ListSessionsResponse::new(result))
    }

    #[cfg(feature = "unstable-session-fork")]
    async fn fork_session(
        &self,
        args: acp::ForkSessionRequest,
    ) -> acp::Result<acp::ForkSessionResponse> {
        let in_memory = self.sessions.borrow().contains_key(&args.session_id);
        let store = self.store.as_ref();

        if !in_memory {
            match store {
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

        // LRU eviction: find and remove the oldest idle session when at limit.
        if self.sessions.borrow().len() >= self.max_sessions {
            let evict_id = {
                let sessions = self.sessions.borrow();
                sessions
                    .iter()
                    .filter(|(_, e)| e.output_rx.borrow().is_some())
                    .min_by_key(|(_, e)| e.last_active.get())
                    .map(|(id, _)| id.clone())
            };
            match evict_id {
                Some(id) => {
                    if let Some(entry) = self.sessions.borrow_mut().remove(&id) {
                        entry.cancel_signal.notify_one();
                        tracing::debug!(session_id = %id, "evicted idle ACP session (LRU)");
                    }
                }
                None => {
                    return Err(acp::Error::internal_error().data("session limit reached"));
                }
            }
        }

        let new_id = acp::SessionId::new(uuid::Uuid::new_v4().to_string());
        tracing::debug!(
            source = %args.session_id,
            new = %new_id,
            "forking ACP session"
        );

        if let Some(s) = store {
            let source_events = s
                .load_acp_events(&args.session_id.to_string())
                .await
                .map_err(|e| {
                    tracing::warn!(error = %e, "failed to load ACP session events for fork");
                    acp::Error::internal_error().data("internal error")
                })?;

            let new_id_str = new_id.to_string();
            let store_clone = s.clone();
            let pairs: Vec<(String, String)> = source_events
                .into_iter()
                .map(|ev| (ev.event_type, ev.payload))
                .collect();
            tokio::task::spawn_local(async move {
                if let Err(e) = store_clone.create_acp_session(&new_id_str).await {
                    tracing::warn!(error = %e, "failed to create forked ACP session");
                    return;
                }
                let refs: Vec<(&str, &str)> = pairs
                    .iter()
                    .map(|(t, p)| (t.as_str(), p.as_str()))
                    .collect();
                if let Err(e) = store_clone.import_acp_events(&new_id_str, &refs).await {
                    tracing::warn!(error = %e, "failed to import events for forked session");
                }
            });
        }

        let (channel, handle) = LoopbackChannel::pair(LOOPBACK_CHANNEL_CAPACITY);
        let cancel_signal = std::sync::Arc::clone(&handle.cancel_signal);
        let provider_override: Arc<std::sync::RwLock<Option<AnyProvider>>> =
            Arc::new(std::sync::RwLock::new(None));
        let provider_override_for_ctx = Arc::clone(&provider_override);
        let entry = SessionEntry {
            input_tx: handle.input_tx,
            output_rx: RefCell::new(Some(handle.output_rx)),
            cancel_signal: handle.cancel_signal,
            last_active: std::cell::Cell::new(std::time::Instant::now()),
            created_at: chrono::Utc::now(),
            working_dir: RefCell::new(Some(args.cwd.clone())),
            provider_override,
            current_model: RefCell::new(String::new()),
            current_mode: RefCell::new(acp::SessionModeId::new(DEFAULT_MODE_ID)),
            first_prompt_done: std::cell::Cell::new(false),
        };
        self.sessions.borrow_mut().insert(new_id.clone(), entry);

        let acp_ctx = self.build_acp_context(&new_id, cancel_signal, provider_override_for_ctx);
        let spawner = Arc::clone(&self.spawner);
        tokio::task::spawn_local(async move {
            (spawner)(channel, acp_ctx).await;
        });

        let config_options = build_model_config_options(&self.available_models, "");
        let default_mode_id = acp::SessionModeId::new(DEFAULT_MODE_ID);
        let mut resp =
            acp::ForkSessionResponse::new(new_id).modes(build_mode_state(&default_mode_id));
        if !config_options.is_empty() {
            resp = resp.config_options(config_options);
        }
        Ok(resp)
    }

    #[cfg(feature = "unstable-session-resume")]
    async fn resume_session(
        &self,
        args: acp::ResumeSessionRequest,
    ) -> acp::Result<acp::ResumeSessionResponse> {
        // Session already in memory — nothing to restore.
        if self.sessions.borrow().contains_key(&args.session_id) {
            return Ok(acp::ResumeSessionResponse::new());
        }

        // Try to restore from SQLite persistence (same as load_session but no event replay).
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

        // LRU eviction: find and remove the oldest idle session when at limit.
        if self.sessions.borrow().len() >= self.max_sessions {
            let evict_id = {
                let sessions = self.sessions.borrow();
                sessions
                    .iter()
                    .filter(|(_, e)| e.output_rx.borrow().is_some())
                    .min_by_key(|(_, e)| e.last_active.get())
                    .map(|(id, _)| id.clone())
            };
            match evict_id {
                Some(id) => {
                    if let Some(entry) = self.sessions.borrow_mut().remove(&id) {
                        entry.cancel_signal.notify_one();
                        tracing::debug!(session_id = %id, "evicted idle ACP session (LRU)");
                    }
                }
                None => {
                    return Err(acp::Error::internal_error().data("session limit reached"));
                }
            }
        }

        let (channel, handle) = LoopbackChannel::pair(LOOPBACK_CHANNEL_CAPACITY);
        let cancel_signal = std::sync::Arc::clone(&handle.cancel_signal);
        let provider_override: Arc<std::sync::RwLock<Option<AnyProvider>>> =
            Arc::new(std::sync::RwLock::new(None));
        let provider_override_for_ctx = Arc::clone(&provider_override);
        let entry = SessionEntry {
            input_tx: handle.input_tx,
            output_rx: RefCell::new(Some(handle.output_rx)),
            cancel_signal: handle.cancel_signal,
            last_active: std::cell::Cell::new(std::time::Instant::now()),
            created_at: chrono::Utc::now(),
            working_dir: RefCell::new(Some(args.cwd)),
            provider_override,
            current_model: RefCell::new(String::new()),
            current_mode: RefCell::new(acp::SessionModeId::new(DEFAULT_MODE_ID)),
            first_prompt_done: std::cell::Cell::new(false),
        };
        self.sessions
            .borrow_mut()
            .insert(args.session_id.clone(), entry);

        let acp_ctx =
            self.build_acp_context(&args.session_id, cancel_signal, provider_override_for_ctx);
        let spawner = Arc::clone(&self.spawner);
        tokio::task::spawn_local(async move {
            (spawner)(channel, acp_ctx).await;
        });

        Ok(acp::ResumeSessionResponse::new())
    }

    async fn set_session_config_option(
        &self,
        args: acp::SetSessionConfigOptionRequest,
    ) -> acp::Result<acp::SetSessionConfigOptionResponse> {
        let config_id: &str = &args.config_id.0;
        if config_id != "model" {
            return Err(acp::Error::invalid_request().data("unknown config_id"));
        }

        let value: &str = &args.value.0;
        let Some(ref factory) = self.provider_factory else {
            return Err(acp::Error::internal_error().data("model switching not configured"));
        };

        if !self.available_models.iter().any(|m| m == value) {
            return Err(acp::Error::invalid_request().data("model not in allowed list"));
        }

        let Some(new_provider) = factory(value) else {
            return Err(acp::Error::invalid_request().data("unknown model"));
        };

        let sessions = self.sessions.borrow();
        let entry = sessions
            .get(&args.session_id)
            .ok_or_else(|| acp::Error::internal_error().data("session not found"))?;

        *entry
            .provider_override
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(new_provider);
        value.clone_into(&mut entry.current_model.borrow_mut());
        let current = entry.current_model.borrow().clone();
        drop(sessions);

        tracing::debug!(
            session_id = %args.session_id,
            model = %value,
            "ACP model switched"
        );

        let config_options = build_model_config_options(&self.available_models, &current);
        Ok(acp::SetSessionConfigOptionResponse::new(config_options))
    }

    async fn set_session_mode(
        &self,
        args: acp::SetSessionModeRequest,
    ) -> acp::Result<acp::SetSessionModeResponse> {
        let valid_ids: &[&str] = &["code", "architect", "ask"];
        let mode_str = args.mode_id.0.as_ref();
        if !valid_ids.contains(&mode_str) {
            return Err(acp::Error::invalid_request().data(format!("unknown mode: {mode_str}")));
        }

        {
            let sessions = self.sessions.borrow();
            let entry = sessions
                .get(&args.session_id)
                .ok_or_else(|| acp::Error::invalid_request().data("session not found"))?;
            *entry.current_mode.borrow_mut() = args.mode_id.clone();
        }

        tracing::debug!(
            session_id = %args.session_id,
            mode = %mode_str,
            "ACP session mode switched"
        );

        let update = acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new(
            args.mode_id.clone(),
        ));
        let notification = acp::SessionNotification::new(args.session_id, update);
        if let Err(e) = self.send_notification(notification).await {
            tracing::warn!(error = %e, "failed to send current_mode_update");
        }

        Ok(acp::SetSessionModeResponse::new())
    }

    #[cfg(feature = "unstable-session-model")]
    async fn set_session_model(
        &self,
        args: acp::SetSessionModelRequest,
    ) -> acp::Result<acp::SetSessionModelResponse> {
        let model_id: &str = &args.model_id.0;

        let Some(ref factory) = self.provider_factory else {
            return Err(acp::Error::internal_error().data("model switching not configured"));
        };

        if !self.available_models.iter().any(|m| m == model_id) {
            return Err(acp::Error::invalid_request().data("model not in allowed list"));
        }

        let Some(new_provider) = factory(model_id) else {
            return Err(acp::Error::invalid_request().data("unknown model"));
        };

        let sessions = self.sessions.borrow();
        let entry = sessions
            .get(&args.session_id)
            .ok_or_else(|| acp::Error::internal_error().data("session not found"))?;
        *entry
            .provider_override
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(new_provider);
        model_id.clone_into(&mut entry.current_model.borrow_mut());

        tracing::debug!(
            session_id = %args.session_id,
            model = %model_id,
            "ACP session model switched via set_session_model"
        );

        Ok(acp::SetSessionModelResponse::new())
    }
}

impl ZephAcpAgent {
    /// Dispatch a slash command, returning a short-circuit `PromptResponse`.
    async fn handle_slash_command(
        &self,
        session_id: &acp::SessionId,
        text: &str,
    ) -> acp::Result<acp::PromptResponse> {
        let mut parts = text.splitn(2, ' ');
        let cmd = parts.next().unwrap_or("").trim();
        let arg = parts.next().unwrap_or("").trim();

        let reply = match cmd {
            "/help" => "Available commands:\n\
                 /help — show this message\n\
                 /model <id> — switch the active model\n\
                 /mode <code|architect|ask> — switch session mode\n\
                 /clear — clear session history\n\
                 /compact — summarize and compact context"
                .to_owned(),
            "/model" => self.handle_model_command(session_id, arg)?,
            "/mode" => {
                let valid_ids: &[&str] = &["code", "architect", "ask"];
                if !valid_ids.contains(&arg) {
                    return Err(acp::Error::invalid_request().data(format!("unknown mode: {arg}")));
                }
                {
                    let sessions = self.sessions.borrow();
                    let entry = sessions
                        .get(session_id)
                        .ok_or_else(|| acp::Error::invalid_request().data("session not found"))?;
                    *entry.current_mode.borrow_mut() = acp::SessionModeId::new(arg);
                }
                let update = acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new(
                    acp::SessionModeId::new(arg),
                ));
                let notification = acp::SessionNotification::new(session_id.clone(), update);
                if let Err(e) = self.send_notification(notification).await {
                    tracing::warn!(error = %e, "failed to send current_mode_update from /mode");
                }
                format!("Switched to mode: {arg}")
            }
            "/clear" => {
                if let Some(ref store) = self.store {
                    let sid = session_id.to_string();
                    let store = store.clone();
                    tokio::task::spawn_local(async move {
                        if let Err(e) = store.delete_acp_session(&sid).await {
                            tracing::warn!(error = %e, "failed to clear session history");
                        }
                        if let Err(e) = store.create_acp_session(&sid).await {
                            tracing::warn!(error = %e, "failed to recreate session after clear");
                        }
                    });
                }
                // Send sentinel to clear in-memory agent context.
                let sessions = self.sessions.borrow();
                if let Some(entry) = sessions.get(session_id) {
                    let _ = entry.input_tx.try_send(ChannelMessage {
                        text: "/clear".to_owned(),
                        attachments: vec![],
                    });
                }
                "Session history cleared.".to_owned()
            }
            _ => {
                return Err(acp::Error::invalid_request().data(format!("unknown command: {cmd}")));
            }
        };

        let update =
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(reply.clone().into()));
        let notification = acp::SessionNotification::new(session_id.clone(), update);
        if let Err(e) = self.send_notification(notification).await {
            tracing::warn!(error = %e, "failed to send command reply");
        }

        Ok(acp::PromptResponse::new(acp::StopReason::EndTurn))
    }

    fn resolve_model_fuzzy<'a>(&'a self, query: &str) -> acp::Result<String> {
        if self.available_models.iter().any(|m| m == query) {
            return Ok(query.to_owned());
        }
        let tokens: Vec<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(String::from)
            .collect();
        let candidates: Vec<&'a String> = self
            .available_models
            .iter()
            .filter(|m| {
                let lower = m.to_lowercase();
                tokens.iter().all(|t| lower.contains(t.as_str()))
            })
            .collect();
        match candidates.len() {
            0 => {
                let models = self.available_models.join(", ");
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

    fn handle_model_command(&self, session_id: &acp::SessionId, arg: &str) -> acp::Result<String> {
        if arg.is_empty() {
            let models = self.available_models.join(", ");
            return Ok(format!("Available models: {models}"));
        }
        let Some(ref factory) = self.provider_factory else {
            return Err(acp::Error::internal_error().data("model switching not configured"));
        };
        let resolved = self.resolve_model_fuzzy(arg)?;
        let Some(new_provider) = factory(&resolved) else {
            return Err(acp::Error::invalid_request().data("unknown model"));
        };
        let sessions = self.sessions.borrow();
        let entry = sessions
            .get(session_id)
            .ok_or_else(|| acp::Error::internal_error().data("session not found"))?;
        *entry
            .provider_override
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(new_provider);
        resolved.clone_into(&mut entry.current_model.borrow_mut());
        Ok(format!("Switched to model: {resolved}"))
    }

    async fn ext_method_mcp(&self, args: &acp::ExtRequest) -> acp::Result<acp::ExtResponse> {
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
                Ok(acp::ExtResponse::new(raw.into()))
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
                Ok(acp::ExtResponse::new(raw.into()))
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
                Ok(acp::ExtResponse::new(raw.into()))
            }
            _ => Ok(acp::ExtResponse::new(
                serde_json::value::RawValue::NULL.to_owned().into(),
            )),
        }
    }
}

/// Map a `SessionUpdate` to a `(event_type, payload)` pair for `SQLite` persistence.
fn content_chunk_text(chunk: &acp::ContentChunk) -> String {
    match &chunk.content {
        acp::ContentBlock::Text(t) => t.text.clone(),
        _ => String::new(),
    }
}

fn session_update_to_event(update: &acp::SessionUpdate) -> (&'static str, String) {
    match update {
        acp::SessionUpdate::UserMessageChunk(c) => ("user_message", content_chunk_text(c)),
        acp::SessionUpdate::AgentMessageChunk(c) => ("agent_message", content_chunk_text(c)),
        acp::SessionUpdate::AgentThoughtChunk(c) => ("agent_thought", content_chunk_text(c)),
        acp::SessionUpdate::ToolCall(tc) => {
            let payload = match serde_json::to_string(tc) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to serialize ToolCall for persistence");
                    String::new()
                }
            };
            ("tool_call", payload)
        }
        acp::SessionUpdate::ToolCallUpdate(tcu) => {
            let payload = match serde_json::to_string(tcu) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to serialize ToolCallUpdate for persistence");
                    String::new()
                }
            };
            ("tool_call_update", payload)
        }
        _ => ("unknown", String::new()),
    }
}

/// Returns `true` if `text` looks like a raw tool-use marker that should not be
/// forwarded to the IDE (e.g. `[tool_use: bash (toolu_abc123)]`).
fn is_tool_use_marker(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.starts_with("[tool_use:") && trimmed.ends_with(']')
}

fn mime_to_ext(mime: &str) -> &str {
    match mime {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    }
}

fn tool_kind_from_name(name: &str) -> acp::ToolKind {
    match name {
        "bash" | "shell" => acp::ToolKind::Execute,
        "read_file" => acp::ToolKind::Read,
        "write_file" => acp::ToolKind::Edit,
        "search" | "grep" | "find" => acp::ToolKind::Search,
        "web_scrape" | "fetch" => acp::ToolKind::Fetch,
        _ => acp::ToolKind::Other,
    }
}

const DEFAULT_MODE_ID: &str = "code";

/// MIME type used by Zed IDE to deliver LSP diagnostics as embedded resource blocks.
const DIAGNOSTICS_MIME_TYPE: &str = "application/vnd.zed.diagnostics+json";

/// Deserialize Zed LSP diagnostics JSON and append a formatted `<diagnostics>` block to `out`.
///
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Each entry is rendered as `file:line: [SEVERITY] message`.
/// On parse error the block is emitted empty to avoid injecting untrusted raw JSON into the prompt.
fn format_diagnostics_block(json: &str, out: &mut String) {
    #[derive(serde::Deserialize)]
    struct DiagEntry {
        path: Option<String>,
        row: Option<u32>,
        severity: Option<String>,
        message: Option<String>,
    }

    out.push_str("<diagnostics>\n");
    match serde_json::from_str::<Vec<DiagEntry>>(json) {
        Ok(entries) => {
            for entry in entries {
                let path = entry
                    .path
                    .as_deref()
                    .map_or_else(|| "<unknown>".to_owned(), xml_escape);
                let row = entry.row.map_or_else(|| "?".to_owned(), |r| r.to_string());
                let sev = entry
                    .severity
                    .as_deref()
                    .map_or_else(|| "?".to_owned(), xml_escape);
                let msg = entry
                    .message
                    .as_deref()
                    .map_or_else(String::new, xml_escape);
                out.push_str(&path);
                out.push(':');
                out.push_str(&row);
                out.push_str(": [");
                out.push_str(&sev);
                out.push_str("] ");
                out.push_str(&msg);
                out.push('\n');
            }
        }
        Err(e) => {
            tracing::debug!(error = %e, "failed to parse diagnostics JSON — skipping");
        }
    }
    out.push_str("</diagnostics>");
}

fn build_available_commands() -> Vec<acp::AvailableCommand> {
    vec![
        acp::AvailableCommand::new("help", "Show available commands"),
        acp::AvailableCommand::new("model", "Switch the active model").input(
            acp::AvailableCommandInput::Unstructured(acp::UnstructuredCommandInput::new(
                "model id",
            )),
        ),
        acp::AvailableCommand::new("mode", "Switch session mode (code/architect/ask)").input(
            acp::AvailableCommandInput::Unstructured(acp::UnstructuredCommandInput::new(
                "code | architect | ask",
            )),
        ),
        acp::AvailableCommand::new("clear", "Clear session history"),
        acp::AvailableCommand::new("compact", "Summarize and compact context"),
    ]
}

fn available_session_modes() -> Vec<acp::SessionMode> {
    vec![
        acp::SessionMode::new("code", "Code").description("Write and edit code, execute tools"),
        acp::SessionMode::new("architect", "Architect")
            .description("Design and plan without writing code"),
        acp::SessionMode::new("ask", "Ask")
            .description("Answer questions without code changes or tools"),
    ]
}

fn build_mode_state(current_mode_id: &acp::SessionModeId) -> acp::SessionModeState {
    acp::SessionModeState::new(current_mode_id.clone(), available_session_modes())
}

/// Build the `model` config option selector from the list of available model keys.
///
/// `current` is the currently selected model key; empty string means no selection yet.
fn build_model_config_options(
    available_models: &[String],
    current: &str,
) -> Vec<acp::SessionConfigOption> {
    if available_models.is_empty() {
        return Vec::new();
    }
    let current_value = if current.is_empty() {
        available_models[0].clone()
    } else {
        current.to_owned()
    };
    let options: Vec<acp::SessionConfigSelectOption> = available_models
        .iter()
        .map(|m| acp::SessionConfigSelectOption::new(m.clone(), m.clone()))
        .collect();
    vec![
        acp::SessionConfigOption::select("model", "Model", current_value, options)
            .category(acp::SessionConfigOptionCategory::Model),
    ]
}

#[allow(clippy::too_many_lines)]
fn loopback_event_to_updates(event: LoopbackEvent) -> Vec<acp::SessionUpdate> {
    match event {
        LoopbackEvent::Chunk(text) | LoopbackEvent::FullMessage(text)
            if text.is_empty() || is_tool_use_marker(&text) =>
        {
            vec![]
        }
        LoopbackEvent::Chunk(text) | LoopbackEvent::FullMessage(text) => {
            if text.is_empty() {
                vec![]
            } else {
                vec![acp::SessionUpdate::AgentMessageChunk(
                    acp::ContentChunk::new(text.into()),
                )]
            }
        }
        LoopbackEvent::Status(text) if text.is_empty() => vec![],
        LoopbackEvent::Status(text) => vec![
            acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new("\n".into())),
            acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(text.into())),
        ],
        LoopbackEvent::ToolStart {
            tool_name,
            tool_call_id,
        } => {
            let tool_call = acp::ToolCall::new(tool_call_id, &tool_name)
                .kind(tool_kind_from_name(&tool_name))
                .status(acp::ToolCallStatus::InProgress);
            vec![acp::SessionUpdate::ToolCall(tool_call)]
        }
        LoopbackEvent::ToolOutput {
            tool_name: _,
            display,
            locations,
            tool_call_id,
            is_error,
            terminal_id,
            ..
        } => {
            let acp_locations: Vec<acp::ToolCallLocation> = locations
                .unwrap_or_default()
                .into_iter()
                .map(|p| acp::ToolCallLocation::new(std::path::PathBuf::from(p)))
                .collect();

            let status = if is_error {
                acp::ToolCallStatus::Failed
            } else {
                acp::ToolCallStatus::Completed
            };
            let mut content = vec![acp::ToolCallContent::from(acp::ContentBlock::Text(
                acp::TextContent::new(display),
            ))];
            if let Some(tid) = terminal_id {
                content.push(acp::ToolCallContent::Terminal(acp::Terminal::new(tid)));
            }
            let mut fields = acp::ToolCallUpdateFields::new()
                .status(status)
                .content(content);
            if !acp_locations.is_empty() {
                fields = fields.locations(acp_locations);
            }
            vec![acp::SessionUpdate::ToolCallUpdate(
                acp::ToolCallUpdate::new(tool_call_id, fields),
            )]
        }
        LoopbackEvent::Flush => vec![],
        #[cfg(feature = "unstable-session-usage")]
        LoopbackEvent::Usage {
            input_tokens,
            output_tokens,
            context_window,
        } => {
            let used = input_tokens.saturating_add(output_tokens);
            vec![acp::SessionUpdate::UsageUpdate(acp::UsageUpdate::new(
                used,
                context_window,
            ))]
        }
        #[cfg(not(feature = "unstable-session-usage"))]
        LoopbackEvent::Usage { .. } => vec![],
        #[cfg(feature = "unstable-session-info-update")]
        LoopbackEvent::SessionTitle(title) => {
            vec![acp::SessionUpdate::SessionInfoUpdate(
                acp::SessionInfoUpdate::new().title(title),
            )]
        }
        #[cfg(not(feature = "unstable-session-info-update"))]
        LoopbackEvent::SessionTitle(_) => vec![],
        LoopbackEvent::Plan(entries) => {
            let acp_entries = entries
                .into_iter()
                .map(|(content, status)| {
                    let acp_status = match status {
                        zeph_core::channel::PlanItemStatus::Pending => {
                            acp::PlanEntryStatus::Pending
                        }
                        zeph_core::channel::PlanItemStatus::InProgress => {
                            acp::PlanEntryStatus::InProgress
                        }
                        zeph_core::channel::PlanItemStatus::Completed => {
                            acp::PlanEntryStatus::Completed
                        }
                    };
                    acp::PlanEntry::new(content, acp::PlanEntryPriority::Medium, acp_status)
                })
                .collect();
            vec![acp::SessionUpdate::Plan(acp::Plan::new(acp_entries))]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_spawner() -> AgentSpawner {
        Arc::new(|_channel, _ctx| Box::pin(async {}))
    }

    fn make_agent() -> (
        ZephAcpAgent,
        mpsc::UnboundedReceiver<(acp::SessionNotification, oneshot::Sender<()>)>,
    ) {
        make_agent_with_max(4)
    }

    fn make_agent_with_max(
        max_sessions: usize,
    ) -> (
        ZephAcpAgent,
        mpsc::UnboundedReceiver<(acp::SessionNotification, oneshot::Sender<()>)>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let conn_slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        (
            ZephAcpAgent::new(make_spawner(), tx, conn_slot, max_sessions, 1800, None),
            rx,
        )
    }

    #[tokio::test]
    async fn initialize_returns_agent_info() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .initialize(acp::InitializeRequest::new(acp::ProtocolVersion::LATEST))
                    .await
                    .unwrap();
                assert!(resp.agent_info.is_some());
            })
            .await;
    }

    #[tokio::test]
    async fn initialize_returns_load_session_capability_and_auth_hint() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .initialize(acp::InitializeRequest::new(acp::ProtocolVersion::LATEST))
                    .await
                    .unwrap();
                assert!(resp.agent_capabilities.load_session);
                let prompt_caps = &resp.agent_capabilities.prompt_capabilities;
                assert!(prompt_caps.image);
                assert!(prompt_caps.embedded_context);
                assert!(!prompt_caps.audio);
                let cap_meta = resp
                    .agent_capabilities
                    .meta
                    .as_ref()
                    .expect("agent_capabilities.meta should be present");
                assert!(
                    cap_meta.contains_key("config_options"),
                    "config_options missing from agent_capabilities meta"
                );
                assert!(
                    cap_meta.contains_key("ext_methods"),
                    "ext_methods missing from agent_capabilities meta"
                );
                let meta = resp.meta.expect("meta should be present");
                assert!(
                    meta.contains_key("auth_hint"),
                    "auth_hint key missing from meta"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn ext_notification_accepts_unknown_method() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let notif = acp::ExtNotification::new(
                    "custom/ping",
                    serde_json::value::RawValue::from_string("{}".to_owned())
                        .unwrap()
                        .into(),
                );
                let result = agent.ext_notification(notif).await;
                assert!(result.is_ok());
            })
            .await;
    }

    #[tokio::test]
    async fn new_session_creates_entry() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                assert!(!resp.session_id.to_string().is_empty());
                assert!(agent.sessions.borrow().contains_key(&resp.session_id));
            })
            .await;
    }

    #[tokio::test]
    async fn cancel_keeps_session() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let sid = resp.session_id.clone();
                agent
                    .cancel(acp::CancelNotification::new(sid.clone()))
                    .await
                    .unwrap();
                // Cancel keeps the session alive for subsequent prompts.
                assert!(agent.sessions.borrow().contains_key(&sid));
            })
            .await;
    }

    #[tokio::test]
    async fn cancel_triggers_notify_one() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let sid = resp.session_id.clone();

                // Capture the cancel_signal before cancel() removes the entry.
                let signal = std::sync::Arc::clone(
                    &agent.sessions.borrow().get(&sid).unwrap().cancel_signal,
                );

                // Set up a notified future before calling cancel().
                let notified = signal.notified();

                agent
                    .cancel(acp::CancelNotification::new(sid))
                    .await
                    .unwrap();

                // Should resolve immediately since cancel() called notify_one().
                tokio::time::timeout(std::time::Duration::from_millis(100), notified)
                    .await
                    .expect("cancel_signal was not notified within timeout");
            })
            .await;
    }

    #[tokio::test]
    async fn prompt_image_block_does_not_error() {
        use base64::Engine as _;
        use zeph_core::Channel as _;
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let received: std::rc::Rc<std::cell::RefCell<Option<ChannelMessage>>> =
                    std::rc::Rc::new(std::cell::RefCell::new(None));
                let received_clone = std::rc::Rc::clone(&received);
                let spawner: AgentSpawner = Arc::new(move |mut channel, _ctx| {
                    let received_clone = std::rc::Rc::clone(&received_clone);
                    Box::pin(async move {
                        if let Ok(Some(msg)) = channel.recv().await {
                            *received_clone.borrow_mut() = Some(msg);
                        }
                    })
                });
                let (tx, _rx) = mpsc::unbounded_channel();
                let conn_slot = std::rc::Rc::new(std::cell::RefCell::new(None));
                let agent = ZephAcpAgent::new(spawner, tx, conn_slot, 4, 1800, None);
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();

                let png_bytes = vec![137u8, 80, 78, 71, 13, 10, 26, 10]; // PNG magic bytes
                let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
                let img_block = acp::ContentBlock::Image(acp::ImageContent::new(b64, "image/png"));
                let req = acp::PromptRequest::new(resp.session_id.to_string(), vec![img_block]);
                let result = agent.prompt(req).await;
                assert!(result.is_ok());

                // Spawner received the message with one image attachment
                let msg = received.borrow().clone().unwrap();
                assert_eq!(msg.attachments.len(), 1);
                assert_eq!(
                    msg.attachments[0].kind,
                    zeph_core::channel::AttachmentKind::Image
                );
                assert_eq!(msg.attachments[0].data, png_bytes);
            })
            .await;
    }

    #[tokio::test]
    async fn prompt_resource_block_appends_text() {
        use zeph_core::Channel as _;
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let received: std::rc::Rc<std::cell::RefCell<Option<ChannelMessage>>> =
                    std::rc::Rc::new(std::cell::RefCell::new(None));
                let received_clone = std::rc::Rc::clone(&received);
                let spawner: AgentSpawner = Arc::new(move |mut channel, _ctx| {
                    let received_clone = std::rc::Rc::clone(&received_clone);
                    Box::pin(async move {
                        if let Ok(Some(msg)) = channel.recv().await {
                            *received_clone.borrow_mut() = Some(msg);
                        }
                    })
                });
                let (tx, _rx) = mpsc::unbounded_channel();
                let conn_slot = std::rc::Rc::new(std::cell::RefCell::new(None));
                let agent = ZephAcpAgent::new(spawner, tx, conn_slot, 4, 1800, None);
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();

                let text_block = acp::ContentBlock::Text(acp::TextContent::new("hello"));
                let res_block = acp::ContentBlock::Resource(acp::EmbeddedResource::new(
                    acp::EmbeddedResourceResource::TextResourceContents(
                        acp::TextResourceContents::new("world", "file:///foo.txt"),
                    ),
                ));
                let req = acp::PromptRequest::new(
                    resp.session_id.to_string(),
                    vec![text_block, res_block],
                );
                agent.prompt(req).await.unwrap();

                let msg = received.borrow().clone().unwrap();
                assert!(msg.text.contains("hello"));
                assert!(
                    msg.text
                        .contains("<resource name=\"file:///foo.txt\">world</resource>")
                );
                assert!(msg.attachments.is_empty());
            })
            .await;
    }

    #[tokio::test]
    async fn prompt_rejects_oversized() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let big = "x".repeat(MAX_PROMPT_BYTES + 1);
                let block = acp::ContentBlock::Text(acp::TextContent::new(big));
                let req = acp::PromptRequest::new(resp.session_id.to_string(), vec![block]);
                assert!(agent.prompt(req).await.is_err());
            })
            .await;
    }

    #[test]
    fn loopback_flush_returns_none() {
        assert!(loopback_event_to_updates(LoopbackEvent::Flush).is_empty());
    }

    #[test]
    fn loopback_chunk_maps_to_agent_message() {
        let updates = loopback_event_to_updates(LoopbackEvent::Chunk("hi".into()));
        assert_eq!(updates.len(), 1);
        assert!(matches!(
            updates[0],
            acp::SessionUpdate::AgentMessageChunk(_)
        ));
    }

    #[test]
    fn loopback_status_maps_to_thought() {
        let updates = loopback_event_to_updates(LoopbackEvent::Status("thinking".into()));
        // Two chunks: a newline separator followed by the status text.
        assert_eq!(updates.len(), 2);
        assert!(matches!(
            updates[0],
            acp::SessionUpdate::AgentThoughtChunk(_)
        ));
        assert!(matches!(
            updates[1],
            acp::SessionUpdate::AgentThoughtChunk(_)
        ));
    }

    #[test]
    fn loopback_status_updates_show_as_separate_lines() {
        let first = loopback_event_to_updates(LoopbackEvent::Status("matching skills".into()));
        let second = loopback_event_to_updates(LoopbackEvent::Status("building context".into()));
        let combined: Vec<_> = first.iter().chain(second.iter()).collect();
        // Both status updates produce separator + text, so accumulated text contains newlines
        // between status messages rather than concatenating them directly.
        let text: String = combined
            .iter()
            .filter_map(|u| {
                if let acp::SessionUpdate::AgentThoughtChunk(c) = u {
                    Some(content_chunk_text(c))
                } else {
                    None
                }
            })
            .collect();
        assert!(
            text.contains('\n'),
            "status updates must be separated by newlines"
        );
        assert!(text.contains("matching skills"));
        assert!(text.contains("building context"));
    }

    #[test]
    fn loopback_empty_chunk_returns_none() {
        assert!(loopback_event_to_updates(LoopbackEvent::Chunk(String::new())).is_empty());
        assert!(loopback_event_to_updates(LoopbackEvent::FullMessage(String::new())).is_empty());
        assert!(loopback_event_to_updates(LoopbackEvent::Status(String::new())).is_empty());
    }

    #[test]
    fn loopback_tool_start_maps_to_tool_call_in_progress() {
        let event = LoopbackEvent::ToolStart {
            tool_name: "bash".to_owned(),
            tool_call_id: "test-id".to_owned(),
        };
        let updates = loopback_event_to_updates(event);
        assert_eq!(updates.len(), 1);
        match &updates[0] {
            acp::SessionUpdate::ToolCall(tc) => {
                assert_eq!(tc.title, "bash");
                assert_eq!(tc.status, acp::ToolCallStatus::InProgress);
                assert_eq!(tc.kind, acp::ToolKind::Execute);
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn loopback_tool_output_maps_to_tool_call_update() {
        let event = LoopbackEvent::ToolOutput {
            tool_name: "bash".to_owned(),
            display: "done".to_owned(),
            diff: None,
            filter_stats: None,
            kept_lines: None,
            locations: None,
            tool_call_id: "test-id".to_owned(),
            is_error: false,
            terminal_id: None,
        };
        let updates = loopback_event_to_updates(event);
        assert_eq!(updates.len(), 1);
        match &updates[0] {
            acp::SessionUpdate::ToolCallUpdate(tcu) => {
                assert_eq!(tcu.fields.status, Some(acp::ToolCallStatus::Completed));
            }
            other => panic!("expected ToolCallUpdate, got {other:?}"),
        }
    }

    #[test]
    fn loopback_tool_output_error_maps_to_failed() {
        let event = LoopbackEvent::ToolOutput {
            tool_name: "bash".to_owned(),
            display: "error".to_owned(),
            diff: None,
            filter_stats: None,
            kept_lines: None,
            locations: None,
            tool_call_id: "test-id".to_owned(),
            is_error: true,
            terminal_id: None,
        };
        let updates = loopback_event_to_updates(event);
        assert_eq!(updates.len(), 1);
        match &updates[0] {
            acp::SessionUpdate::ToolCallUpdate(tcu) => {
                assert_eq!(tcu.fields.status, Some(acp::ToolCallStatus::Failed));
            }
            other => panic!("expected ToolCallUpdate, got {other:?}"),
        }
    }

    #[test]
    fn tool_kind_from_name_maps_correctly() {
        assert_eq!(tool_kind_from_name("bash"), acp::ToolKind::Execute);
        assert_eq!(tool_kind_from_name("read_file"), acp::ToolKind::Read);
        assert_eq!(tool_kind_from_name("write_file"), acp::ToolKind::Edit);
        assert_eq!(tool_kind_from_name("search"), acp::ToolKind::Search);
        assert_eq!(tool_kind_from_name("web_scrape"), acp::ToolKind::Fetch);
        assert_eq!(tool_kind_from_name("unknown"), acp::ToolKind::Other);
    }

    #[tokio::test]
    async fn new_session_rejects_over_limit() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent_with_max(1);
                use acp::Agent as _;
                // fill the limit
                agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                // LRU evicts the only idle session, so second succeeds
                let res = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await;
                assert!(res.is_ok());
                // Now there's 1 session again (evicted + new)
                assert_eq!(agent.sessions.borrow().len(), 1);
            })
            .await;
    }

    #[tokio::test]
    async fn new_session_rejects_when_all_busy() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent_with_max(1);
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                // Mark the session as busy by taking output_rx
                agent
                    .sessions
                    .borrow()
                    .get(&resp.session_id)
                    .unwrap()
                    .output_rx
                    .borrow_mut()
                    .take();
                // No idle sessions to evict — should fail
                let res = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await;
                assert!(res.is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn new_session_respects_configurable_limit() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent_with_max(2);
                use acp::Agent as _;
                let r1 = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let _r2 = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                // Third session triggers LRU eviction (evicts r1 as oldest idle)
                let r3 = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                assert_eq!(agent.sessions.borrow().len(), 2);
                assert!(!agent.sessions.borrow().contains_key(&r1.session_id));
                assert!(agent.sessions.borrow().contains_key(&r3.session_id));
            })
            .await;
    }

    #[tokio::test]
    async fn load_session_returns_ok_for_existing() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let res = agent
                    .load_session(acp::LoadSessionRequest::new(
                        resp.session_id,
                        std::path::PathBuf::from("."),
                    ))
                    .await;
                assert!(res.is_ok());
            })
            .await;
    }

    #[tokio::test]
    async fn load_session_errors_for_unknown() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let res = agent
                    .load_session(acp::LoadSessionRequest::new(
                        acp::SessionId::new("no-such"),
                        std::path::PathBuf::from("."),
                    ))
                    .await;
                assert!(res.is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn prompt_errors_for_unknown_session() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let req = acp::PromptRequest::new("no-such", vec![]);
                assert!(agent.prompt(req).await.is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn prompt_oversized_image_base64_skipped() {
        use zeph_core::Channel as _;
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let received: std::rc::Rc<std::cell::RefCell<Option<ChannelMessage>>> =
                    std::rc::Rc::new(std::cell::RefCell::new(None));
                let received_clone = std::rc::Rc::clone(&received);
                let spawner: AgentSpawner = Arc::new(move |mut channel, _ctx| {
                    let received_clone = std::rc::Rc::clone(&received_clone);
                    Box::pin(async move {
                        if let Ok(Some(msg)) = channel.recv().await {
                            *received_clone.borrow_mut() = Some(msg);
                        }
                    })
                });
                let (tx, _rx) = mpsc::unbounded_channel();
                let conn_slot = std::rc::Rc::new(std::cell::RefCell::new(None));
                let agent = ZephAcpAgent::new(spawner, tx, conn_slot, 4, 1800, None);
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();

                // Simulate oversized base64 data (exceeds MAX_IMAGE_BASE64_BYTES)
                let oversized = "A".repeat(MAX_IMAGE_BASE64_BYTES + 1);
                let img_block =
                    acp::ContentBlock::Image(acp::ImageContent::new(oversized, "image/png"));
                let req = acp::PromptRequest::new(resp.session_id.to_string(), vec![img_block]);
                agent.prompt(req).await.unwrap();

                let msg = received.borrow().clone().unwrap();
                assert!(
                    msg.attachments.is_empty(),
                    "oversized image must be skipped"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn prompt_unsupported_mime_image_skipped() {
        use base64::Engine as _;
        use zeph_core::Channel as _;
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let received: std::rc::Rc<std::cell::RefCell<Option<ChannelMessage>>> =
                    std::rc::Rc::new(std::cell::RefCell::new(None));
                let received_clone = std::rc::Rc::clone(&received);
                let spawner: AgentSpawner = Arc::new(move |mut channel, _ctx| {
                    let received_clone = std::rc::Rc::clone(&received_clone);
                    Box::pin(async move {
                        if let Ok(Some(msg)) = channel.recv().await {
                            *received_clone.borrow_mut() = Some(msg);
                        }
                    })
                });
                let (tx, _rx) = mpsc::unbounded_channel();
                let conn_slot = std::rc::Rc::new(std::cell::RefCell::new(None));
                let agent = ZephAcpAgent::new(spawner, tx, conn_slot, 4, 1800, None);
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();

                let b64 = base64::engine::general_purpose::STANDARD.encode(b"data");
                let img_block =
                    acp::ContentBlock::Image(acp::ImageContent::new(b64, "application/pdf"));
                let req = acp::PromptRequest::new(resp.session_id.to_string(), vec![img_block]);
                agent.prompt(req).await.unwrap();

                let msg = received.borrow().clone().unwrap();
                assert!(
                    msg.attachments.is_empty(),
                    "unsupported MIME type must be skipped"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn prompt_resource_text_wrapped_in_markers() {
        use zeph_core::Channel as _;
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let received: std::rc::Rc<std::cell::RefCell<Option<ChannelMessage>>> =
                    std::rc::Rc::new(std::cell::RefCell::new(None));
                let received_clone = std::rc::Rc::clone(&received);
                let spawner: AgentSpawner = Arc::new(move |mut channel, _ctx| {
                    let received_clone = std::rc::Rc::clone(&received_clone);
                    Box::pin(async move {
                        if let Ok(Some(msg)) = channel.recv().await {
                            *received_clone.borrow_mut() = Some(msg);
                        }
                    })
                });
                let (tx, _rx) = mpsc::unbounded_channel();
                let conn_slot = std::rc::Rc::new(std::cell::RefCell::new(None));
                let agent = ZephAcpAgent::new(spawner, tx, conn_slot, 4, 1800, None);
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();

                let res_block = acp::ContentBlock::Resource(acp::EmbeddedResource::new(
                    acp::EmbeddedResourceResource::TextResourceContents(
                        acp::TextResourceContents::new("injected content", "file:///secret.txt"),
                    ),
                ));
                let req = acp::PromptRequest::new(resp.session_id.to_string(), vec![res_block]);
                agent.prompt(req).await.unwrap();

                let msg = received.borrow().clone().unwrap();
                assert!(
                    msg.text.contains(
                        "<resource name=\"file:///secret.txt\">injected content</resource>"
                    ),
                    "resource text must be wrapped in markers with name attribute"
                );
            })
            .await;
    }

    #[test]
    fn mime_to_ext_known_types() {
        assert_eq!(mime_to_ext("image/jpeg"), "jpg");
        assert_eq!(mime_to_ext("image/jpg"), "jpg");
        assert_eq!(mime_to_ext("image/png"), "png");
        assert_eq!(mime_to_ext("image/gif"), "gif");
        assert_eq!(mime_to_ext("image/webp"), "webp");
        assert_eq!(mime_to_ext("image/unknown"), "bin");
    }

    #[test]
    fn loopback_tool_output_with_locations() {
        let event = LoopbackEvent::ToolOutput {
            tool_name: "read_file".to_owned(),
            display: "content".to_owned(),
            diff: None,
            filter_stats: None,
            kept_lines: None,
            locations: Some(vec!["/src/main.rs".to_owned(), "/src/lib.rs".to_owned()]),
            tool_call_id: "test-id".to_owned(),
            is_error: false,
            terminal_id: None,
        };
        let updates = loopback_event_to_updates(event);
        assert_eq!(updates.len(), 1);
        match &updates[0] {
            acp::SessionUpdate::ToolCallUpdate(tcu) => {
                let locs = tcu.fields.locations.as_deref().unwrap_or(&[]);
                assert_eq!(locs.len(), 2);
                assert_eq!(locs[0].path, std::path::PathBuf::from("/src/main.rs"));
                assert_eq!(locs[1].path, std::path::PathBuf::from("/src/lib.rs"));
            }
            other => panic!("expected ToolCallUpdate, got {other:?}"),
        }
    }

    #[test]
    fn loopback_tool_output_empty_locations() {
        let event = LoopbackEvent::ToolOutput {
            tool_name: "bash".to_owned(),
            display: "ok".to_owned(),
            diff: None,
            filter_stats: None,
            kept_lines: None,
            locations: None,
            tool_call_id: "test-id".to_owned(),
            is_error: false,
            terminal_id: None,
        };
        let updates = loopback_event_to_updates(event);
        assert_eq!(updates.len(), 1);
        match &updates[0] {
            acp::SessionUpdate::ToolCallUpdate(tcu) => {
                assert!(tcu.fields.locations.as_deref().unwrap_or(&[]).is_empty());
            }
            other => panic!("expected ToolCallUpdate, got {other:?}"),
        }
    }

    #[test]
    fn tool_use_marker_filtered_duplicate() {
        let event =
            LoopbackEvent::Chunk("[tool_use: bash (toolu_01VzP6Q9b6JQY6ZP5r6qY9Wm)]".into());
        assert!(loopback_event_to_updates(event).is_empty());

        let event = LoopbackEvent::FullMessage("[tool_use: read (toolu_abc)]".into());
        assert!(loopback_event_to_updates(event).is_empty());

        // Normal text should pass through.
        let event = LoopbackEvent::Chunk("hello [tool_use: not a marker".into());
        assert!(!loopback_event_to_updates(event).is_empty());
    }

    #[test]
    fn loopback_tool_output_with_terminal_id() {
        let event = LoopbackEvent::ToolOutput {
            tool_name: "bash".to_owned(),
            display: "ls output".to_owned(),
            diff: None,
            filter_stats: None,
            kept_lines: None,
            locations: None,
            tool_call_id: "tid-1".to_owned(),
            is_error: false,
            terminal_id: Some("term-42".to_owned()),
        };
        let updates = loopback_event_to_updates(event);
        assert_eq!(updates.len(), 1);
        match &updates[0] {
            acp::SessionUpdate::ToolCallUpdate(tcu) => {
                assert!(
                    tcu.fields
                        .content
                        .as_deref()
                        .unwrap_or(&[])
                        .iter()
                        .any(|c| matches!(c, acp::ToolCallContent::Terminal(_)))
                );
            }
            other => panic!("expected ToolCallUpdate with Terminal content, got {other:?}"),
        }
    }

    #[test]
    fn build_model_config_options_empty() {
        let opts = build_model_config_options(&[], "");
        assert!(opts.is_empty());
    }

    #[test]
    fn build_model_config_options_defaults_to_first() {
        let models = vec![
            "claude:claude-sonnet-4-5".to_owned(),
            "ollama:llama3".to_owned(),
        ];
        let opts = build_model_config_options(&models, "");
        assert_eq!(opts.len(), 1);
        let opt = &opts[0];
        assert_eq!(opt.id.0.as_ref(), "model");
    }

    #[test]
    fn build_model_config_options_uses_current() {
        let models = vec![
            "claude:claude-sonnet-4-5".to_owned(),
            "ollama:llama3".to_owned(),
        ];
        let opts = build_model_config_options(&models, "ollama:llama3");
        assert_eq!(opts.len(), 1);
    }

    #[tokio::test]
    async fn initialize_advertises_session_capabilities() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .initialize(acp::InitializeRequest::new(acp::ProtocolVersion::LATEST))
                    .await
                    .unwrap();
                let caps = resp.agent_capabilities;
                let session_caps = caps.session_capabilities;
                assert!(
                    session_caps.list.is_some(),
                    "list capability must be advertised"
                );
                assert!(
                    session_caps.fork.is_some(),
                    "fork capability must be advertised"
                );
                assert!(
                    session_caps.resume.is_some(),
                    "resume capability must be advertised"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn set_session_mode_valid_updates_current_mode() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, mut notify_rx) = make_agent();
                // Drain notifications and send ack so send_notification doesn't block.
                tokio::task::spawn_local(async move {
                    while let Some((_notif, ack)) = notify_rx.recv().await {
                        ack.send(()).ok();
                    }
                });
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let sid = resp.session_id.clone();
                let req = acp::SetSessionModeRequest::new(sid.clone(), "ask");
                let result = agent.set_session_mode(req).await;
                assert!(result.is_ok());
                let sessions = agent.sessions.borrow();
                let entry = sessions.get(&sid).unwrap();
                assert_eq!(*entry.current_mode.borrow(), acp::SessionModeId::new("ask"));
            })
            .await;
    }

    #[tokio::test]
    async fn set_session_mode_unknown_mode_errors() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let req = acp::SetSessionModeRequest::new(resp.session_id.clone(), "turbo");
                let result = agent.set_session_mode(req).await;
                assert!(result.is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn ext_notification_always_ok() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let notif = acp::ExtNotification::new(
                    "_agent/some/event",
                    serde_json::value::RawValue::NULL.to_owned().into(),
                );
                let result = agent.ext_notification(notif).await;
                assert!(result.is_ok());
            })
            .await;
    }

    #[tokio::test]
    async fn set_session_config_option_unknown_config_id_errors() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let req = acp::SetSessionConfigOptionRequest::new(
                    resp.session_id.clone(),
                    "unknown_id",
                    "value",
                );
                let result = agent.set_session_config_option(req).await;
                assert!(result.is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn set_session_config_option_no_factory_errors() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let req = acp::SetSessionConfigOptionRequest::new(
                    resp.session_id.clone(),
                    "model",
                    "ollama:llama3",
                );
                let result = agent.set_session_config_option(req).await;
                assert!(result.is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn set_session_config_option_with_factory_updates_model() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                use acp::Agent as _;
                let (tx, _rx) = mpsc::unbounded_channel();
                let conn_slot = std::rc::Rc::new(std::cell::RefCell::new(None));
                let factory: ProviderFactory = Arc::new(|key: &str| {
                    if key == "ollama:llama3" {
                        // Return a dummy AnyProvider. In tests we can't easily construct
                        // real providers, so we verify the factory is called correctly by
                        // returning Some only for the known key.
                        Some(zeph_llm::any::AnyProvider::Ollama(
                            zeph_llm::ollama::OllamaProvider::new(
                                "http://localhost:11434",
                                "llama3".into(),
                                "nomic-embed-text".into(),
                            ),
                        ))
                    } else {
                        None
                    }
                });
                let agent = ZephAcpAgent::new(make_spawner(), tx, conn_slot, 4, 1800, None)
                    .with_provider_factory(factory, vec!["ollama:llama3".to_owned()]);
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                // config_options should be returned when models are available.
                assert!(resp.config_options.is_some());
                let req = acp::SetSessionConfigOptionRequest::new(
                    resp.session_id.clone(),
                    "model",
                    "ollama:llama3",
                );
                let result = agent.set_session_config_option(req).await;
                assert!(result.is_ok());
                let response = result.unwrap();
                assert_eq!(response.config_options.len(), 1);
                // current_model should be updated in the session entry.
                let sessions = agent.sessions.borrow();
                let entry = sessions.get(&resp.session_id).unwrap();
                assert_eq!(*entry.current_model.borrow(), "ollama:llama3");
            })
            .await;
    }

    #[tokio::test]
    async fn ext_method_no_manager_errors() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let req = acp::ExtRequest::new(
                    "_agent/mcp/list",
                    serde_json::value::RawValue::NULL.to_owned().into(),
                );
                let result = agent.ext_method(req).await;
                assert!(result.is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn ext_method_unknown_returns_null() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let req = acp::ExtRequest::new(
                    "_agent/unknown/method",
                    serde_json::value::RawValue::NULL.to_owned().into(),
                );
                let result = agent.ext_method(req).await;
                assert!(result.is_ok());
            })
            .await;
    }

    #[tokio::test]
    async fn set_session_config_option_rejects_model_not_in_allowlist() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                use acp::Agent as _;
                let (tx, _rx) = mpsc::unbounded_channel();
                let conn_slot = std::rc::Rc::new(std::cell::RefCell::new(None));
                let factory: ProviderFactory = Arc::new(|_key: &str| {
                    Some(zeph_llm::any::AnyProvider::Ollama(
                        zeph_llm::ollama::OllamaProvider::new(
                            "http://localhost:11434",
                            "llama3".into(),
                            "nomic-embed-text".into(),
                        ),
                    ))
                });
                let agent = ZephAcpAgent::new(make_spawner(), tx, conn_slot, 4, 1800, None)
                    .with_provider_factory(factory, vec!["ollama:llama3".to_owned()]);
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                // "expensive:gpt-5" is not in the allowlist — must be rejected.
                let req = acp::SetSessionConfigOptionRequest::new(
                    resp.session_id.clone(),
                    "model",
                    "expensive:gpt-5",
                );
                let result = agent.set_session_config_option(req).await;
                assert!(result.is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn new_session_includes_modes() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let modes = resp
                    .modes
                    .expect("modes should be present in new_session response");
                assert_eq!(modes.current_mode_id.0.as_ref(), DEFAULT_MODE_ID);
                assert_eq!(modes.available_modes.len(), 3);
            })
            .await;
    }

    #[tokio::test]
    async fn set_session_mode_updates_entry() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, mut rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let sid = resp.session_id.clone();

                // Drain notifications in background
                tokio::task::spawn_local(async move {
                    while let Some((_, ack)) = rx.recv().await {
                        let _ = ack.send(());
                    }
                });

                agent
                    .set_session_mode(acp::SetSessionModeRequest::new(sid.clone(), "architect"))
                    .await
                    .unwrap();

                let mode = agent
                    .sessions
                    .borrow()
                    .get(&sid)
                    .map(|e| e.current_mode.borrow().0.as_ref().to_owned())
                    .unwrap();
                assert_eq!(mode, "architect");
            })
            .await;
    }

    #[tokio::test]
    async fn set_session_mode_emits_notification() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, mut rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let sid = resp.session_id.clone();

                // Drain any notifications enqueued by new_session before the mode change.
                while let Ok((_, ack)) = rx.try_recv() {
                    let _ = ack.send(());
                }

                let result = tokio::join!(
                    agent.set_session_mode(acp::SetSessionModeRequest::new(sid, "ask")),
                    async {
                        // Drain until CurrentModeUpdate is found.
                        loop {
                            if let Some((notif, ack)) = rx.recv().await {
                                let _ = ack.send(());
                                if matches!(notif.update, acp::SessionUpdate::CurrentModeUpdate(_))
                                {
                                    return Some(notif);
                                }
                            } else {
                                return None;
                            }
                        }
                    }
                );

                assert!(result.0.is_ok());
                let notif = result.1.expect("notification should be received");
                assert!(matches!(
                    notif.update,
                    acp::SessionUpdate::CurrentModeUpdate(_)
                ));
            })
            .await;
    }

    #[tokio::test]
    async fn set_session_mode_rejects_unknown_mode() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let result = agent
                    .set_session_mode(acp::SetSessionModeRequest::new(
                        resp.session_id,
                        "invalid-mode",
                    ))
                    .await;
                assert!(result.is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn set_session_mode_rejects_unknown_session() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let result = agent
                    .set_session_mode(acp::SetSessionModeRequest::new(
                        acp::SessionId::new("nonexistent"),
                        "code",
                    ))
                    .await;
                assert!(result.is_err());
            })
            .await;
    }

    #[cfg(feature = "unstable-session-list")]
    #[tokio::test]
    async fn list_sessions_returns_active_sessions() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let resp = agent
                    .list_sessions(acp::ListSessionsRequest::new())
                    .await
                    .unwrap();
                assert_eq!(resp.sessions.len(), 2);
            })
            .await;
    }

    #[cfg(feature = "unstable-session-list")]
    #[tokio::test]
    async fn list_sessions_filters_by_cwd() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let resp1 = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let resp2 = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();

                let dir_a = std::path::PathBuf::from("/tmp/dir-a");
                let dir_b = std::path::PathBuf::from("/tmp/dir-b");

                agent
                    .sessions
                    .borrow()
                    .get(&resp1.session_id)
                    .unwrap()
                    .working_dir
                    .replace(Some(dir_a.clone()));
                agent
                    .sessions
                    .borrow()
                    .get(&resp2.session_id)
                    .unwrap()
                    .working_dir
                    .replace(Some(dir_b));

                let resp = agent
                    .list_sessions(acp::ListSessionsRequest::new().cwd(dir_a))
                    .await
                    .unwrap();
                assert_eq!(resp.sessions.len(), 1);
            })
            .await;
    }

    #[cfg(feature = "unstable-session-fork")]
    #[tokio::test]
    async fn fork_session_errors_for_unknown() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let unknown_id = acp::SessionId::new(uuid::Uuid::new_v4().to_string());
                let result = agent
                    .fork_session(acp::ForkSessionRequest::new(
                        unknown_id,
                        std::path::PathBuf::from("."),
                    ))
                    .await;
                assert!(result.is_err());
            })
            .await;
    }

    #[cfg(feature = "unstable-session-resume")]
    #[tokio::test]
    async fn resume_session_returns_ok_for_active() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let result = agent
                    .resume_session(acp::ResumeSessionRequest::new(
                        resp.session_id,
                        std::path::PathBuf::from("."),
                    ))
                    .await;
                assert!(result.is_ok());
            })
            .await;
    }

    #[cfg(feature = "unstable-session-resume")]
    #[tokio::test]
    async fn resume_session_errors_for_unknown() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let unknown_id = acp::SessionId::new(uuid::Uuid::new_v4().to_string());
                let result = agent
                    .resume_session(acp::ResumeSessionRequest::new(
                        unknown_id,
                        std::path::PathBuf::from("."),
                    ))
                    .await;
                assert!(result.is_err());
            })
            .await;
    }

    // --- #962 diagnostics ---

    #[test]
    fn format_diagnostics_valid_json() {
        let json =
            r#"[{"path":"src/main.rs","row":10,"severity":"error","message":"type mismatch"}]"#;
        let mut out = String::new();
        format_diagnostics_block(json, &mut out);
        assert!(out.starts_with("<diagnostics>\n"));
        assert!(out.contains("src/main.rs:10: [error] type mismatch\n"));
        assert!(out.ends_with("</diagnostics>"));
    }

    #[test]
    fn format_diagnostics_invalid_json_emits_empty_block() {
        let json = "not json";
        let mut out = String::new();
        format_diagnostics_block(json, &mut out);
        assert!(
            !out.contains("not json"),
            "raw JSON must not be injected into prompt"
        );
        assert!(out.starts_with("<diagnostics>\n"));
        assert!(out.ends_with("</diagnostics>"));
    }

    #[test]
    fn format_diagnostics_missing_fields_uses_defaults() {
        let json = r#"[{}]"#;
        let mut out = String::new();
        format_diagnostics_block(json, &mut out);
        assert!(out.contains("<unknown>:?: [?] \n"));
    }

    #[tokio::test]
    async fn prompt_diagnostics_block_formatted() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                // Manually test format_diagnostics_block since prompt requires live agent.
                let json = r#"[{"path":"lib.rs","row":5,"severity":"warning","message":"unused"}]"#;
                let mut out = String::new();
                format_diagnostics_block(json, &mut out);
                assert!(out.contains("lib.rs:5: [warning] unused"));
            })
            .await;
    }

    // --- #961 AvailableCommandsUpdate / slash commands ---

    #[test]
    fn build_available_commands_returns_expected_set() {
        let cmds = build_available_commands();
        let names: Vec<&str> = cmds.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"help"));
        assert!(names.contains(&"model"));
        assert!(names.contains(&"mode"));
        assert!(names.contains(&"clear"));
        assert!(names.contains(&"compact"));
    }

    #[test]
    fn build_available_commands_model_has_input() {
        let cmds = build_available_commands();
        let model_cmd = cmds.iter().find(|c| c.name == "model").unwrap();
        assert!(model_cmd.input.is_some());
    }

    #[tokio::test]
    async fn slash_help_returns_end_turn() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, mut rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let sid = resp.session_id.clone();

                // Drain AvailableCommandsUpdate from new_session.
                while let Ok((_, ack)) = rx.try_recv() {
                    let _ = ack.send(());
                }

                let result = tokio::join!(
                    agent.prompt(acp::PromptRequest::new(
                        sid,
                        vec![acp::ContentBlock::Text(acp::TextContent::new("/help"))]
                    )),
                    async {
                        if let Some((_, ack)) = rx.recv().await {
                            let _ = ack.send(());
                        }
                    }
                );
                let resp = result.0.unwrap();
                assert!(matches!(resp.stop_reason, acp::StopReason::EndTurn));
            })
            .await;
    }

    #[tokio::test]
    async fn slash_unknown_command_returns_error() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, mut rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let sid = resp.session_id.clone();
                while let Ok((_, ack)) = rx.try_recv() {
                    let _ = ack.send(());
                }
                let result = agent
                    .prompt(acp::PromptRequest::new(
                        sid,
                        vec![acp::ContentBlock::Text(acp::TextContent::new(
                            "/nonexistent",
                        ))],
                    ))
                    .await;
                assert!(result.is_err());
            })
            .await;
    }

    // --- #957 UsageUpdate ---

    #[test]
    fn loopback_usage_maps_to_usage_update() {
        let event = LoopbackEvent::Usage {
            input_tokens: 100,
            output_tokens: 50,
            context_window: 200_000,
        };
        let updates = loopback_event_to_updates(event);
        assert_eq!(updates.len(), 1);
        #[cfg(feature = "unstable-session-usage")]
        assert!(matches!(updates[0], acp::SessionUpdate::UsageUpdate(_)));
        #[cfg(not(feature = "unstable-session-usage"))]
        assert!(updates.is_empty());
    }

    // --- #959 SessionTitle ---

    #[test]
    fn loopback_session_title_maps_to_session_info_update() {
        let event = LoopbackEvent::SessionTitle("My Session".to_owned());
        let updates = loopback_event_to_updates(event);
        #[cfg(feature = "unstable-session-info-update")]
        {
            assert_eq!(updates.len(), 1);
            assert!(matches!(
                updates[0],
                acp::SessionUpdate::SessionInfoUpdate(_)
            ));
        }
        #[cfg(not(feature = "unstable-session-info-update"))]
        assert!(updates.is_empty());
    }

    // --- #960 Plan ---

    #[test]
    fn loopback_plan_maps_to_plan_update() {
        use zeph_core::channel::PlanItemStatus;
        let event = LoopbackEvent::Plan(vec![
            ("step 1".to_owned(), PlanItemStatus::Pending),
            ("step 2".to_owned(), PlanItemStatus::InProgress),
            ("step 3".to_owned(), PlanItemStatus::Completed),
        ]);
        let updates = loopback_event_to_updates(event);
        assert_eq!(updates.len(), 1);
        match &updates[0] {
            acp::SessionUpdate::Plan(plan) => {
                assert_eq!(plan.entries.len(), 3);
                assert!(matches!(
                    plan.entries[0].status,
                    acp::PlanEntryStatus::Pending
                ));
                assert!(matches!(
                    plan.entries[1].status,
                    acp::PlanEntryStatus::InProgress
                ));
                assert!(matches!(
                    plan.entries[2].status,
                    acp::PlanEntryStatus::Completed
                ));
            }
            _ => panic!("expected Plan update"),
        }
    }

    #[test]
    fn loopback_plan_empty_entries() {
        let event = LoopbackEvent::Plan(vec![]);
        let updates = loopback_event_to_updates(event);
        assert_eq!(updates.len(), 1);
        assert!(matches!(
            &updates[0],
            acp::SessionUpdate::Plan(p) if p.entries.is_empty()
        ));
    }

    // --- #958 SetSessionModel ---

    #[cfg(feature = "unstable-session-model")]
    #[tokio::test]
    async fn set_session_model_no_factory_errors() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, mut rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                while let Ok((_, ack)) = rx.try_recv() {
                    let _ = ack.send(());
                }
                let result = agent
                    .set_session_model(acp::SetSessionModelRequest::new(
                        resp.session_id,
                        "some:model",
                    ))
                    .await;
                assert!(result.is_err());
            })
            .await;
    }

    #[cfg(feature = "unstable-session-model")]
    #[tokio::test]
    async fn set_session_model_rejects_unknown_model() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
                let conn_slot = std::rc::Rc::new(std::cell::RefCell::new(None));
                let factory: ProviderFactory = Arc::new(|_| None);
                let agent = ZephAcpAgent::new(make_spawner(), tx, conn_slot, 4, 1800, None)
                    .with_provider_factory(factory, vec!["claude:claude-3-5-sonnet".to_owned()]);
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let result = agent
                    .set_session_model(acp::SetSessionModelRequest::new(
                        resp.session_id,
                        "ollama:llama3",
                    ))
                    .await;
                assert!(result.is_err());
            })
            .await;
    }
}
