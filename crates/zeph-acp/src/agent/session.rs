// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Session-lifecycle methods for `ZephAcpAgentState`.
//!
//! Groups session creation, loading, forking, resuming, closing, deletion, listing, and
//! per-session config/mode updates — the largest single cluster of ACP protocol handlers —
//! so session-lifecycle logic is isolated from prompt-turn handling, slash-command dispatch,
//! and model resolution in [`super`].

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::{Mutex, RwLock};

use agent_client_protocol as acp;
use tokio::sync::{mpsc, oneshot};
use tracing::Instrument as _;
use zeph_core::channel::{LoopbackChannel, LoopbackHandle};
use zeph_core::text::truncate_to_chars;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider as _;
use zeph_memory::ConversationId;

use crate::mcp_bridge::acp_mcp_servers_to_entries;
use crate::terminal::AcpShellExecutor;

#[cfg(feature = "unstable-session-usage")]
use super::SessionUsageAccumulator;
#[cfg(feature = "unstable-elicitation")]
use super::elicitation;
#[cfg(test)]
use super::{AgentSpawner, ZephAcpAgent};
use super::{
    DEFAULT_MODE_ID, NotifyReceiver, NotifySender, SESSION_ENTRY_GENERATION, SessionConfigSeed,
    SessionContext, SessionEntry, ZephAcpAgentState, build_config_options, build_mode_state,
    model_meta, session_event_to_updates,
};

const LOOPBACK_CHANNEL_CAPACITY: usize = 64;

/// Look up the `ConversationId` for an existing ACP session, creating one for legacy
/// sessions that predate migration 026 (where `conversation_id` is `NULL`).
///
/// Returns `None` when the store is unavailable or all creation attempts fail, allowing
/// the caller to proceed in ephemeral (no-history) mode rather than failing the session.
async fn resolve_conversation_id(
    store: &zeph_memory::store::SqliteStore,
    session_id: &acp::schema::v1::SessionId,
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

impl ZephAcpAgentState {
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
                let sent = async {
                    if cx_drain.send_notification(notif).is_err() {
                        tracing::warn!("session_notification send failed; drainer exiting");
                        return false;
                    }
                    ack.send(()).ok();
                    true
                }
                .instrument(tracing::info_span!("acp.session.notify"))
                .await;
                if !sent {
                    break;
                }
            }
            Ok(())
        })
    }

    /// Assemble the `NewSessionResponse` with config options and project rule metadata.
    fn build_new_session_response(
        &self,
        session_id: acp::schema::v1::SessionId,
        initial_model: &str,
    ) -> acp::schema::v1::NewSessionResponse {
        let available_models = self.available_models_snapshot();
        let config_options = build_config_options(
            &available_models,
            initial_model,
            false,
            "suggest",
            self.model_config.default_temperature_preset,
        );
        let default_mode_id = acp::schema::v1::SessionModeId::new(DEFAULT_MODE_ID);
        let mut resp = acp::schema::v1::NewSessionResponse::new(session_id)
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
        args: acp::schema::v1::NewSessionRequest,
        cx: &acp::ConnectionTo<acp::Client>,
    ) -> acp::Result<acp::schema::v1::NewSessionResponse> {
        self.validate_additional_directories(&args.additional_directories)
            .await?;
        self.evict_oldest_idle_session_if_full()?;

        let session_id = acp::schema::v1::SessionId::new(uuid::Uuid::new_v4().to_string());
        tracing::debug!(%session_id, "new ACP session");

        let (channel, handle) = LoopbackChannel::pair(LOOPBACK_CHANNEL_CAPACITY);
        let cancel_signal = Arc::clone(&handle.cancel_signal);
        let provider_override: Arc<RwLock<Option<AnyProvider>>> = Arc::new(RwLock::new(None));
        let provider_override_for_ctx = Arc::clone(&provider_override);
        // Bounded: prevents a misbehaving IDE from buffering notifications without limit.
        // 256 slots cover any realistic burst between drainer loop iterations. Created here
        // (not inside `make_session_entry`) so `notify_tx` can also seed `build_acp_context`'s
        // `SessionStatusNotifier`.
        let (notify_tx, notify_rx) = mpsc::channel(256);

        let session_cwd = args.cwd.clone();

        #[cfg(feature = "unstable-elicitation")]
        let (elicitation_tx, elicitation_bridge_handle) = if self
            .elicitation_supported
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            let (tx, rx) = elicitation::elicitation_channel();
            let handle = elicitation::spawn_elicitation_bridge(
                cx.clone(),
                rx,
                Arc::clone(&cancel_signal),
                self.timeouts.elicitation_secs,
            );
            (Some(tx), Some(handle))
        } else {
            (None, None)
        };

        let acp_ctx = self
            .build_acp_context(
                &session_id,
                cx,
                cancel_signal,
                provider_override_for_ctx,
                session_cwd.clone(),
                notify_tx.clone(),
                #[cfg(feature = "unstable-elicitation")]
                elicitation_tx,
            )
            .await;
        let shell_executor = acp_ctx.shell_executor.clone();
        let initial_model = self.initial_model();
        self.prime_provider_override(
            &provider_override,
            &initial_model,
            self.model_config.default_temperature_preset,
        );
        #[cfg_attr(not(feature = "unstable-elicitation"), allow(unused_mut))]
        let mut entry = Self::make_session_entry(
            handle,
            initial_model.clone(),
            session_cwd.clone(),
            shell_executor,
            provider_override,
            SessionConfigSeed {
                thinking_enabled: false,
                auto_approve_level: "suggest".to_owned(),
                temperature_preset: self.model_config.default_temperature_preset,
            },
            notify_tx,
            notify_rx,
        );
        #[cfg(feature = "unstable-elicitation")]
        {
            entry.elicitation_bridge_handle = elicitation_bridge_handle;
        }

        Self::spawn_notify_drainer(&entry, cx)?;
        self.sessions.lock().insert(session_id.clone(), entry);

        if let Some(ref manager) = self.mcp_manager {
            let entries =
                acp_mcp_servers_to_entries(&args.mcp_servers, self.timeouts.elicitation_secs);
            for server_entry in entries {
                let id = server_entry.id.clone();
                if let Err(e) = manager.add_server(&server_entry).await {
                    tracing::warn!(server_id = %id, error = %e, "failed to register IDE MCP server");
                }
            }
        }

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

    #[tracing::instrument(skip_all, name = "acp.handler.close_session", fields(session_id = %args.session_id))]
    pub(crate) async fn do_close_session(
        &self,
        args: acp::schema::v1::CloseSessionRequest,
    ) -> acp::Result<acp::schema::v1::CloseSessionResponse> {
        tracing::debug!(session_id = %args.session_id, "ACP session closed");
        // Send cumulative usage summary BEFORE removing the session so the notify_tx is still live.
        #[cfg(feature = "unstable-session-usage")]
        {
            use acp::schema::v1::{Cost, SessionNotification, SessionUpdate, UsageUpdate};
            let snapshot = self
                .sessions
                .lock()
                .get(&args.session_id)
                .map(|e| e.usage_accumulator.lock().clone());
            if let Some(acc) = snapshot {
                let used = acc
                    .total_input_tokens
                    .saturating_add(acc.total_output_tokens);
                let mut update = UsageUpdate::new(used, acc.last_context_window);
                if acc.last_cost_cents > 0.0 {
                    update = update.cost(Cost::new(acc.last_cost_cents / 100.0, "USD"));
                }
                let notification = SessionNotification::new(
                    args.session_id.clone(),
                    SessionUpdate::UsageUpdate(update),
                );
                if let Err(e) = self.send_notification(&args.session_id, notification).await {
                    tracing::warn!(error = %e, "failed to send session-close usage notification");
                }
            }
        }
        let removed = self.sessions.lock().remove(&args.session_id);
        if let Some(entry) = removed {
            entry.cancel_signal.notify_one();
            // Snapshot the session's config fields (#5373) so a later `session/resume` or
            // `session/fork` of this now-evicted session can inherit them instead of
            // resetting to configured defaults.
            if let Some(ref store) = self.store {
                let snapshot = zeph_memory::store::AcpSessionConfigSnapshot {
                    current_model: entry.current_model.lock().clone(),
                    temperature_preset: (*entry.temperature_preset.lock()).as_str().to_owned(),
                    thinking_enabled: entry.thinking_enabled.load(Ordering::Relaxed),
                    auto_approve_level: entry.auto_approve_level.lock().clone(),
                };
                if let Err(e) = store
                    .save_session_config(&args.session_id.to_string(), &snapshot)
                    .await
                {
                    tracing::warn!(error = %e, "failed to persist session config snapshot on close");
                }
            }
        }
        Ok(acp::schema::v1::CloseSessionResponse::default())
    }

    #[tracing::instrument(skip_all, name = "acp.handler.delete_session", fields(session_id = %args.session_id))]
    pub(crate) async fn do_delete_session(
        &self,
        args: acp::schema::v1::DeleteSessionRequest,
    ) -> acp::Result<acp::schema::v1::DeleteSessionResponse> {
        tracing::debug!(session_id = %args.session_id, "ACP session deleted");
        // Permanent deletion — no usage summary is sent. See do_close_session for graceful
        // close that emits a cumulative UsageUpdate before removing the session. In-memory
        // removal is unconditional and happens first: the id lookup here is not owner-scoped
        // (unlike the store delete below), which is benign only because `self.sessions` is
        // private to this connection's owner — if it is ever shared across owners, this
        // becomes a cross-owner eviction bug. Persisted-store deletion failure is surfaced as
        // an error rather than swallowed: `delete_acp_session_for_owner` deletes by id, so a
        // retry is safe (the in-memory removal above is already a no-op on retry), and a
        // silent failure here would let a transient DB error (lock/disk full/pool exhaustion)
        // report success to the client while the persisted row — and the resurrection risk it
        // carries — survives.
        if let Some(entry) = self.sessions.lock().remove(&args.session_id) {
            entry.cancel_signal.notify_one();
        }
        if let Some(ref store) = self.store
            && let Err(e) = store
                .delete_acp_session_for_owner(&args.session_id.to_string(), &self.owner_key)
                .await
        {
            tracing::warn!(error = %e, session_id = %args.session_id, "failed to delete persisted ACP session");
            // Static message only (matches do_load_session/do_fork_session) — the raw
            // MemoryError Display could leak DB URL/SQL error text to the client.
            return Err(acp::Error::internal_error().data("session deletion not persisted"));
        }
        Ok(acp::schema::v1::DeleteSessionResponse::default())
    }

    #[tracing::instrument(skip_all, name = "acp.handler.load_session", fields(session_id = %args.session_id))]
    pub(crate) async fn do_load_session(
        &self,
        args: acp::schema::v1::LoadSessionRequest,
        cx: &acp::ConnectionTo<acp::Client>,
    ) -> acp::Result<acp::schema::v1::LoadSessionResponse> {
        self.validate_additional_directories(&args.additional_directories)
            .await?;
        if self.sessions.lock().contains_key(&args.session_id) {
            return Ok(acp::schema::v1::LoadSessionResponse::new());
        }

        let Some(ref store) = self.store else {
            return Err(acp::Error::internal_error().data("session not found"));
        };

        // Atomic claim-on-load (#5868): scopes access to this connection's owner_key and
        // self-heals legacy NULL-owner rows by claiming them on first load. Returns false
        // uniformly for "doesn't exist" and "owned by a different owner" — no info leak.
        let claimed = store
            .claim_acp_session_for_owner(&args.session_id.to_string(), &self.owner_key)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, session_id = %args.session_id, "failed to check ACP session existence");
                acp::Error::internal_error().data("internal error")
            })?;

        if !claimed {
            return Err(acp::Error::internal_error().data("session not found"));
        }

        // spec-068 §12.3 / D-2: the legacy `acp_session_events` table is emptied by the write-path
        // cutover — post-cutover sessions must replay from the durable JSONL event log instead
        // (`self.session_data_dir`, wired the same way `do_fork_session` already reads it).
        let events = self
            .load_session_replay_events(&args.session_id.to_string())
            .await;

        let session_cwd = args.cwd.clone();
        let conversation_id = resolve_conversation_id(store, &args.session_id).await;

        let (channel, handle) = LoopbackChannel::pair(LOOPBACK_CHANNEL_CAPACITY);
        let cancel_signal = Arc::clone(&handle.cancel_signal);
        let provider_override: Arc<RwLock<Option<AnyProvider>>> = Arc::new(RwLock::new(None));
        let provider_override_for_ctx = Arc::clone(&provider_override);
        let (notify_tx, notify_rx) = mpsc::channel(256);
        let acp_ctx = self
            .build_acp_context(
                &args.session_id,
                cx,
                cancel_signal,
                provider_override_for_ctx,
                session_cwd.clone(),
                notify_tx.clone(),
                #[cfg(feature = "unstable-elicitation")]
                None,
            )
            .await;
        let shell_executor = acp_ctx.shell_executor.clone();
        let initial_model = self.initial_model();
        self.prime_provider_override(
            &provider_override,
            &initial_model,
            self.model_config.default_temperature_preset,
        );
        let entry = Self::make_session_entry(
            handle,
            initial_model,
            session_cwd.clone(),
            shell_executor,
            provider_override,
            SessionConfigSeed {
                thinking_enabled: false,
                auto_approve_level: "suggest".to_owned(),
                temperature_preset: self.model_config.default_temperature_preset,
            },
            notify_tx,
            notify_rx,
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

        let default_mode_id = acp::schema::v1::SessionModeId::new(DEFAULT_MODE_ID);
        let load_resp =
            acp::schema::v1::LoadSessionResponse::new().modes(build_mode_state(&default_mode_id));

        self.send_commands_update_nowait(&args.session_id);

        Ok(load_resp)
    }

    #[tracing::instrument(skip_all, name = "acp.handler.list_sessions")]
    pub(crate) async fn do_list_sessions(
        &self,
        args: acp::schema::v1::ListSessionsRequest,
    ) -> acp::Result<acp::schema::v1::ListSessionsResponse> {
        let mut result: std::collections::HashMap<String, acp::schema::v1::SessionInfo> = {
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
                    let mut info =
                        acp::schema::v1::SessionInfo::new(session_id.clone(), working_dir)
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
            match store
                .list_acp_sessions_for_owner(self.max_history, &self.owner_key)
                .await
            {
                Ok(persisted) => {
                    for persisted_info in persisted {
                        let sid = acp::schema::v1::SessionId::new(&*persisted_info.id);
                        if result.contains_key(&persisted_info.id) {
                            continue;
                        }
                        let info =
                            acp::schema::v1::SessionInfo::new(sid, std::path::PathBuf::new())
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

        let mut sessions_vec: Vec<acp::schema::v1::SessionInfo> = result.into_values().collect();
        sessions_vec.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

        Ok(acp::schema::v1::ListSessionsResponse::new(sessions_vec))
    }

    /// Resolve the config a new `SessionEntry` should inherit from `source_id` (#5373).
    ///
    /// Order of precedence: the source session's live in-memory state (source still resident
    /// in the LRU cache — the common case for `session/fork`), then the persisted close-time
    /// snapshot (source was gracefully closed — the common case for `session/resume`), then
    /// configured defaults (no snapshot available: the source was evicted rather than closed,
    /// or predates the config-snapshot migration).
    ///
    /// Returns `(model, thinking_enabled, auto_approve_level, temperature_preset)`.
    async fn inherited_session_config(
        &self,
        source_id: &acp::schema::v1::SessionId,
    ) -> (String, bool, String, zeph_config::AcpTemperaturePreset) {
        let inherited = if let Some(entry) = self.sessions.lock().get(source_id) {
            Some((
                entry.current_model.lock().clone(),
                entry.thinking_enabled.load(Ordering::Relaxed),
                entry.auto_approve_level.lock().clone(),
                *entry.temperature_preset.lock(),
            ))
        } else if let Some(ref store) = self.store {
            store
                .get_session_config(&source_id.to_string())
                .await
                .ok()
                .flatten()
                .map(|snapshot| {
                    let preset = snapshot
                        .temperature_preset
                        .parse()
                        .unwrap_or(self.model_config.default_temperature_preset);
                    (
                        snapshot.current_model,
                        snapshot.thinking_enabled,
                        snapshot.auto_approve_level,
                        preset,
                    )
                })
        } else {
            None
        };

        let (model, thinking_enabled, auto_approve_level, temperature_preset) = inherited
            .unwrap_or_else(|| {
                (
                    self.initial_model(),
                    false,
                    "suggest".to_owned(),
                    self.model_config.default_temperature_preset,
                )
            });

        // The inherited model may no longer be configured (removed from the provider list
        // since the source session was created) — fall back to the current default rather
        // than handing the spawner a dangling model key.
        let available_models = self.available_models_snapshot();
        let model = if model.is_empty() || available_models.iter().any(|m| m == &model) {
            model
        } else {
            self.initial_model()
        };

        (
            model,
            thinking_enabled,
            auto_approve_level,
            temperature_preset,
        )
    }

    #[cfg(feature = "unstable-session-fork")]
    #[allow(dead_code, clippy::too_many_lines)]
    #[tracing::instrument(skip_all, name = "acp.handler.fork_session")]
    pub(crate) async fn do_fork_session(
        &self,
        args: acp::schema::v1::ForkSessionRequest,
        cx: &acp::ConnectionTo<acp::Client>,
    ) -> acp::Result<acp::schema::v1::ForkSessionResponse> {
        self.validate_additional_directories(&args.additional_directories)
            .await?;
        let in_memory = self.sessions.lock().contains_key(&args.session_id);

        if !in_memory {
            match self.store.as_ref() {
                None => return Err(acp::Error::internal_error().data("session not found")),
                Some(s) => {
                    // Atomic claim-on-fork (#5868): same self-healing scoping as do_load_session.
                    let claimed = s
                        .claim_acp_session_for_owner(&args.session_id.to_string(), &self.owner_key)
                        .await
                        .map_err(|e| {
                            tracing::warn!(error = %e, "failed to check ACP session existence");
                            acp::Error::internal_error().data("internal error")
                        })?;
                    if !claimed {
                        return Err(acp::Error::internal_error().data("session not found"));
                    }
                }
            }
        }

        // Captured before the LRU eviction pass below, since (pre-existing behavior) that
        // pass does not exclude the fork source and could otherwise evict it out from under us.
        let (inherited_model, inherited_thinking, inherited_auto_approve, inherited_preset) =
            self.inherited_session_config(&args.session_id).await;

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

        let new_id = acp::schema::v1::SessionId::new(uuid::Uuid::new_v4().to_string());
        tracing::debug!(source = %args.session_id, new = %new_id, "forking ACP session");

        let new_conversation_id = self.fork_conversation(&args.session_id, &new_id).await?;

        let (channel, handle) = LoopbackChannel::pair(LOOPBACK_CHANNEL_CAPACITY);
        let cancel_signal = Arc::clone(&handle.cancel_signal);
        let provider_override: Arc<RwLock<Option<AnyProvider>>> = Arc::new(RwLock::new(None));
        let provider_override_for_ctx = Arc::clone(&provider_override);
        let (notify_tx, notify_rx) = mpsc::channel(256);
        let acp_ctx = self
            .build_acp_context(
                &new_id,
                cx,
                cancel_signal,
                provider_override_for_ctx,
                args.cwd.clone(),
                notify_tx.clone(),
                #[cfg(feature = "unstable-elicitation")]
                None,
            )
            .await;
        let shell_executor = acp_ctx.shell_executor.clone();
        let initial_model = inherited_model;
        self.prime_provider_override(&provider_override, &initial_model, inherited_preset);
        let entry = Self::make_session_entry(
            handle,
            initial_model.clone(),
            args.cwd.clone(),
            shell_executor,
            provider_override,
            SessionConfigSeed {
                thinking_enabled: inherited_thinking,
                auto_approve_level: inherited_auto_approve.clone(),
                temperature_preset: inherited_preset,
            },
            notify_tx,
            notify_rx,
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
        let config_options = build_config_options(
            &available_models,
            &initial_model,
            inherited_thinking,
            &inherited_auto_approve,
            inherited_preset,
        );
        let default_mode_id = acp::schema::v1::SessionModeId::new(DEFAULT_MODE_ID);
        let mut resp = acp::schema::v1::ForkSessionResponse::new(new_id)
            .modes(build_mode_state(&default_mode_id));
        if !config_options.is_empty() {
            resp = resp.config_options(config_options);
        }
        Ok(resp)
    }

    #[tracing::instrument(skip_all, name = "acp.handler.resume_session")]
    pub(crate) async fn do_resume_session(
        &self,
        args: acp::schema::v1::ResumeSessionRequest,
        cx: &acp::ConnectionTo<acp::Client>,
    ) -> acp::Result<acp::schema::v1::ResumeSessionResponse> {
        self.validate_additional_directories(&args.additional_directories)
            .await?;
        if self.sessions.lock().contains_key(&args.session_id) {
            return Ok(acp::schema::v1::ResumeSessionResponse::new());
        }

        let Some(ref store) = self.store else {
            return Err(acp::Error::internal_error().data("session not found"));
        };

        // Atomic claim-on-resume (#5868) — see do_load_session.
        let claimed = store
            .claim_acp_session_for_owner(&args.session_id.to_string(), &self.owner_key)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, session_id = %args.session_id, "failed to check ACP session existence");
                acp::Error::internal_error().data("internal error")
            })?;

        if !claimed {
            return Err(acp::Error::internal_error().data("session not found"));
        }

        // Resolved from the persisted close-time snapshot (#5373) — by construction the
        // session is not in memory here (the early return above handles that case), so this
        // always reads through to the store, falling back to config defaults if no snapshot
        // was ever saved for this session.
        let (inherited_model, inherited_thinking, inherited_auto_approve, inherited_preset) =
            self.inherited_session_config(&args.session_id).await;

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
        let (notify_tx, notify_rx) = mpsc::channel(256);
        let acp_ctx = self
            .build_acp_context(
                &args.session_id,
                cx,
                cancel_signal,
                provider_override_for_ctx,
                args.cwd.clone(),
                notify_tx.clone(),
                #[cfg(feature = "unstable-elicitation")]
                None,
            )
            .await;
        let shell_executor = acp_ctx.shell_executor.clone();
        let initial_model = inherited_model;
        self.prime_provider_override(&provider_override, &initial_model, inherited_preset);
        let entry = Self::make_session_entry(
            handle,
            initial_model,
            args.cwd.clone(),
            shell_executor,
            provider_override,
            SessionConfigSeed {
                thinking_enabled: inherited_thinking,
                auto_approve_level: inherited_auto_approve,
                temperature_preset: inherited_preset,
            },
            notify_tx,
            notify_rx,
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

        Ok(acp::schema::v1::ResumeSessionResponse::new())
    }

    #[allow(clippy::unused_async)]
    #[tracing::instrument(skip_all, name = "acp.handler.set_session_config_option")]
    pub(crate) async fn do_set_session_config_option(
        &self,
        args: acp::schema::v1::SetSessionConfigOptionRequest,
    ) -> acp::Result<acp::schema::v1::SetSessionConfigOptionResponse> {
        let config_id = args.config_id.0.clone();
        // `SessionConfigOptionValue::Boolean` was stabilized in acp-schema 1.1.0 and the enum
        // shape is now unconditional, so this always matches (no feature gate needed).
        let value_str: std::sync::Arc<str> = match &args.value {
            acp::schema::v1::SessionConfigOptionValue::ValueId { value } => value.0.clone(),
            acp::schema::v1::SessionConfigOptionValue::Boolean { value } => {
                if *value { "true" } else { "false" }.into()
            }
            _ => "".into(),
        };
        let value: &str = &value_str;

        let (current_model, thinking, auto_approve, temperature_preset) = {
            let sessions = self.sessions.lock();
            let entry = sessions
                .get(&args.session_id)
                .ok_or_else(|| acp::Error::invalid_request().data("session not found"))?;

            self.apply_session_config(entry, config_id.as_ref(), value, &args.session_id)?;

            (
                entry.current_model.lock().clone(),
                entry.thinking_enabled.load(Ordering::Relaxed),
                entry.auto_approve_level.lock().clone(),
                *entry.temperature_preset.lock(),
            )
        };

        let config_options = build_config_options(
            &self.available_models_snapshot(),
            &current_model,
            thinking,
            &auto_approve,
            temperature_preset,
        );

        let changed_option = config_options.iter().find(|o| o.id.0 == config_id).cloned();

        if let Some(option) = changed_option {
            let update = acp::schema::v1::SessionUpdate::ConfigOptionUpdate(
                acp::schema::v1::ConfigOptionUpdate::new(vec![option]),
            );
            self.send_notification_nowait(
                &args.session_id,
                acp::schema::v1::SessionNotification::new(args.session_id.clone(), update),
            );

            if config_id.as_ref() == "model" {
                let info_update = acp::schema::v1::SessionUpdate::SessionInfoUpdate(
                    acp::schema::v1::SessionInfoUpdate::new().meta(model_meta(&current_model)),
                );
                self.send_notification_nowait(
                    &args.session_id,
                    acp::schema::v1::SessionNotification::new(args.session_id.clone(), info_update),
                );
            }
        }

        Ok(acp::schema::v1::SetSessionConfigOptionResponse::new(
            config_options,
        ))
    }

    #[tracing::instrument(skip_all, name = "acp.handler.set_session_mode")]
    pub(crate) async fn do_set_session_mode(
        &self,
        args: acp::schema::v1::SetSessionModeRequest,
    ) -> acp::Result<acp::schema::v1::SetSessionModeResponse> {
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

        let update = acp::schema::v1::SessionUpdate::CurrentModeUpdate(
            acp::schema::v1::CurrentModeUpdate::new(args.mode_id.clone()),
        );
        let notification =
            acp::schema::v1::SessionNotification::new(args.session_id.clone(), update);
        if let Err(e) = self.send_notification(&args.session_id, notification).await {
            tracing::warn!(error = %e, "failed to send current_mode_update");
        }

        Ok(acp::schema::v1::SetSessionModeResponse::new())
    }

    /// Validate `requested` paths against the configured allowlist.
    ///
    /// Each requested path is canonicalized and checked with `Path::starts_with` (component-aware)
    /// against every entry in `self.additional_directories_allow`. Returns an `invalid_params`
    /// error if any path is not covered by the allowlist.
    async fn validate_additional_directories(
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
            let canon = tokio::fs::canonicalize(p).await.map_err(|e| {
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
}

impl ZephAcpAgentState {
    fn apply_session_config(
        &self,
        entry: &SessionEntry,
        config_id: &str,
        value: &str,
        session_id: &acp::schema::v1::SessionId,
    ) -> acp::Result<()> {
        match config_id {
            "model" => {
                let available_models = self.available_models_snapshot();
                if !available_models.iter().any(|m| m == value) {
                    return Err(acp::Error::invalid_request().data("model not in allowed list"));
                }
                let temperature_preset = *entry.temperature_preset.lock();
                let new_provider = self.provider_with_temperature(value, temperature_preset)?;
                *entry.provider_override.write() = Some(new_provider);
                value.clone_into(&mut *entry.current_model.lock());
                tracing::debug!(session_id = %session_id, model = %value, "ACP model switched");
            }
            "temperature" => {
                let preset: zeph_config::AcpTemperaturePreset = value.parse().map_err(|()| {
                    acp::Error::invalid_request()
                        .data("temperature must be precise, balanced, or creative")
                })?;
                let model_key = {
                    let current = entry.current_model.lock().clone();
                    if current.is_empty() {
                        self.initial_model()
                    } else {
                        current
                    }
                };
                if model_key.is_empty() {
                    return Err(acp::Error::internal_error().data("model switching not configured"));
                }
                let new_provider = self.provider_with_temperature(&model_key, preset)?;
                *entry.provider_override.write() = Some(new_provider);
                *entry.temperature_preset.lock() = preset;
                tracing::debug!(session_id = %session_id, temperature = %preset.as_str(), "ACP temperature preset changed");
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

    /// Create a forked conversation for `new_id` from `source_id`.
    ///
    /// Copies conversation history from the source session synchronously before the agent loop
    /// is spawned to eliminate race conditions where the agent starts `load_history()` before the
    /// copy completes.
    ///
    /// Session persistence (spec-068 P2, #5343): when `[session] enabled = true`
    /// (`self.session_data_dir` is `Some`), also forks the durable JSONL event log via
    /// [`zeph_session::ForkEngine::fork`] and links the new `SQLite` conversation to the
    /// `acp_sessions` row `ForkEngine::fork` already created (via `record_fork`) — rather than
    /// creating a second row. This retires the legacy `acp_session_events`
    /// `import_acp_events`/`load_acp_events` copy for new forks: `zeph-acp` no longer needs a
    /// second source of forked history once the JSONL log is the source of truth, matching the P1
    /// write-path cutover's philosophy. When persistence is disabled, behavior is unchanged from
    /// before spec-068 (`SQLite` `messages`/`conversations` copy only, `acp_sessions` row created
    /// directly).
    #[allow(dead_code)]
    async fn fork_conversation(
        &self,
        source_id: &acp::schema::v1::SessionId,
        new_id: &acp::schema::v1::SessionId,
    ) -> acp::Result<Option<ConversationId>> {
        let Some(s) = &self.store else {
            return Ok(None);
        };
        let new_id_str = new_id.to_string();

        if let Some(ref data_dir) = self.session_data_dir {
            let session_store = zeph_session::SessionStore::new(s.pool().clone());
            if let Err(e) = zeph_session::ForkEngine::fork(
                data_dir,
                &source_id.to_string(),
                &new_id_str,
                None,
                &session_store,
                Some(self.owner_key.as_str()),
            )
            .await
            {
                tracing::warn!(
                    error = %e,
                    "failed to fork session event log; SQLite-only fork continues"
                );
            }
        }

        match s.create_conversation().await {
            Ok(forked_cid) => {
                let forked_from_cid = s
                    .get_acp_session_conversation_id(&source_id.to_string())
                    .await
                    .unwrap_or(None);
                if self.session_data_dir.is_some() {
                    // `ForkEngine::fork` above already created the `acp_sessions` row (via
                    // `record_fork`) with `forked_from`/`forked_at_seq` set but no
                    // `conversation_id` — link it now rather than attempting a second
                    // (INSERT-IGNORE, silently-skipped) row creation.
                    let session_store = zeph_session::SessionStore::new(s.pool().clone());
                    if let Err(e) = session_store
                        .link_conversation(&new_id_str, forked_cid.0)
                        .await
                    {
                        tracing::warn!(error = %e, "failed to link conversation to forked session");
                    }
                } else if let Err(e) = s
                    .create_acp_session_with_conversation(
                        &new_id_str,
                        forked_cid,
                        Some(&self.owner_key),
                    )
                    .await
                {
                    tracing::warn!(error = %e, "failed to persist forked ACP session mapping");
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
                if self.session_data_dir.is_none()
                    && let Err(e2) = s
                        .create_acp_session(&new_id_str, Some(&self.owner_key))
                        .await
                {
                    tracing::warn!(error = %e2, "failed to persist forked ACP session");
                }
                Ok(None)
            }
        }
    }

    /// Spawn a background title-generation task for the session's first prompt.
    pub(crate) fn maybe_generate_session_title(
        &self,
        session_id: &acp::schema::v1::SessionId,
        user_text: &str,
    ) {
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
            // EXEMPT(#5144): one-off LLM title generation per new session; already has a 15s
            // timeout, errors are logged. Unique-naming each session's task floods the registry.
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
                let update = acp::schema::v1::SessionUpdate::SessionInfoUpdate(
                    acp::schema::v1::SessionInfoUpdate::new().title(title),
                );
                let notification = acp::schema::v1::SessionNotification::new(sid, update);
                let (tx, _rx) = oneshot::channel();
                if let Err(e) = notify_tx.send((notification, tx)).await {
                    tracing::debug!(error = %e, "session title notification dropped");
                }
            });
        }
    }

    /// Build a fresh `SessionEntry` from a `LoopbackHandle`, seeded with `config` (#5373).
    ///
    /// `notify_tx`/`notify_rx` are created by the caller (not internally) so `notify_tx` can
    /// also be handed to `build_acp_context` for [`SessionStatusNotifier`] before the entry
    /// exists — both must share the same channel.
    #[allow(clippy::too_many_arguments)] // function with many required inputs; a *Params struct would be more verbose without simplifying the call site
    pub(crate) fn make_session_entry(
        handle: LoopbackHandle,
        initial_model: String,
        cwd: PathBuf,
        shell_executor: Option<AcpShellExecutor>,
        provider_override: Arc<RwLock<Option<AnyProvider>>>,
        config: SessionConfigSeed,
        notify_tx: NotifySender,
        notify_rx: NotifyReceiver,
    ) -> SessionEntry {
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
            generation: SESSION_ENTRY_GENERATION.fetch_add(1, Ordering::Relaxed),
            cancel_signal: handle.cancel_signal,
            last_active_ms: AtomicU64::new(now_ms),
            created_at: chrono::Utc::now(),
            working_dir: Mutex::new(Some(cwd)),
            notify_tx,
            notify_rx: Mutex::new(Some(notify_rx)),
            provider_override,
            current_model: Mutex::new(initial_model),
            current_mode: Mutex::new(acp::schema::v1::SessionModeId::new(DEFAULT_MODE_ID)),
            first_prompt_done: AtomicBool::new(false),
            title: Mutex::new(None),
            thinking_enabled: AtomicBool::new(config.thinking_enabled),
            auto_approve_level: Mutex::new(config.auto_approve_level),
            temperature_preset: Mutex::new(config.temperature_preset),
            shell_executor,
            #[cfg(feature = "unstable-elicitation")]
            elicitation_bridge_handle: None,
            #[cfg(feature = "unstable-session-usage")]
            usage_accumulator: Mutex::new(SessionUsageAccumulator::default()),
        }
    }

    /// Read a session's durable JSONL event log for ACP replay (spec-068 §12.3, D-2).
    ///
    /// Returns an empty `Vec` (logging a warning, never erroring the caller) when
    /// `self.session_data_dir` is unset (`[session] enabled = false`) or the log can't be opened —
    /// matching `replay_session_events`'s existing tolerance of missing/legacy history: a session
    /// with no durable log still loads, it just has no client-visible replay.
    async fn load_session_replay_events(
        &self,
        session_id: &str,
    ) -> Vec<zeph_session::SessionEventEnvelope> {
        let Some(ref data_dir) = self.session_data_dir else {
            return Vec::new();
        };
        let session_path = zeph_session::session_dir(data_dir, session_id);
        match zeph_session::SessionEventLog::open(&session_path).await {
            Ok(log) => log.read_all().await.unwrap_or_else(|e| {
                tracing::warn!(error = %e, session_id, "failed to read session event log for replay");
                Vec::new()
            }),
            Err(e) => {
                tracing::warn!(error = %e, session_id, "failed to open session event log for replay");
                Vec::new()
            }
        }
    }

    /// Replay a session's durable `SessionEvent` log as ACP notifications (spec-068 §12.3, D-2).
    async fn replay_session_events(
        &self,
        session_id: &acp::schema::v1::SessionId,
        events: Vec<zeph_session::SessionEventEnvelope>,
    ) {
        for envelope in events {
            for update in session_event_to_updates(envelope.kind) {
                let notification =
                    acp::schema::v1::SessionNotification::new(session_id.clone(), update);
                if let Err(e) = self.send_notification(session_id, notification).await {
                    tracing::warn!(error = %e, "failed to replay notification");
                    return;
                }
            }
        }
    }

    /// Create a new conversation for `session_id` and persist the mapping.
    async fn create_session_conversation(
        &self,
        session_id: &acp::schema::v1::SessionId,
    ) -> Option<ConversationId> {
        let store = self.store.as_ref()?;
        let sid = session_id.to_string();
        match store.create_conversation().await {
            Ok(cid) => {
                if let Err(e) = store
                    .create_acp_session_with_conversation(&sid, cid, Some(&self.owner_key))
                    .await
                {
                    tracing::warn!(error = %e, "failed to persist ACP session mapping; history may not survive restart");
                }
                Some(cid)
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to create conversation for ACP session; session will have no persistent history");
                if let Err(e2) = store.create_acp_session(&sid, Some(&self.owner_key)).await {
                    tracing::warn!(error = %e2, "failed to persist ACP session");
                }
                None
            }
        }
    }
}

/// Regression tests for #5373: `inherited_session_config`'s fallback when the inherited model
/// is no longer configured.
#[cfg(test)]
mod inherited_session_config_tests {
    use std::sync::Arc;

    use parking_lot::RwLock;
    use zeph_core::channel::LoopbackChannel;
    use zeph_llm::any::AnyProvider;

    use super::*;

    /// The inherited model must fall back to `initial_model()` when it is absent from
    /// `available_models_snapshot()` (e.g. removed from `[[llm.providers]]`/`available_models`
    /// since the source session was created), rather than handing the spawner a dangling model
    /// key (#5373).
    #[tokio::test]
    async fn falls_back_to_initial_model_when_inherited_model_not_available() {
        let spawner: AgentSpawner = Arc::new(|_ch, _ctx, _sc| Box::pin(async {}));
        let agent = ZephAcpAgent::new(spawner, 4, 1800, None).with_provider_factory(
            Arc::new(|_key: &str| None),
            Arc::new(RwLock::new(vec!["claude:sonnet".to_owned()])),
        );

        let session_id = acp::schema::v1::SessionId::new("source-session".to_owned());
        let (_, handle) = LoopbackChannel::pair(4);
        let provider_override = Arc::new(RwLock::new(None::<AnyProvider>));
        let (notify_tx, notify_rx) = mpsc::channel(256);
        let entry = ZephAcpAgent::make_session_entry(
            handle,
            "claude:opus".to_owned(),
            std::path::PathBuf::from("."),
            None,
            provider_override,
            SessionConfigSeed {
                thinking_enabled: true,
                auto_approve_level: "auto-edit".to_owned(),
                temperature_preset: zeph_config::AcpTemperaturePreset::Creative,
            },
            notify_tx,
            notify_rx,
        );
        agent.sessions.lock().insert(session_id.clone(), entry);

        let (model, thinking_enabled, auto_approve_level, temperature_preset) =
            agent.inherited_session_config(&session_id).await;

        assert_eq!(
            model,
            agent.initial_model(),
            "model no longer in available_models must fall back to initial_model()"
        );
        // Non-model fields are unaffected by the model-availability check — they still carry
        // through from the source session.
        assert!(thinking_enabled);
        assert_eq!(auto_approve_level, "auto-edit");
        assert_eq!(
            temperature_preset,
            zeph_config::AcpTemperaturePreset::Creative
        );
    }
}
