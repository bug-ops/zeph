// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! ACP-native slash-command handlers for `ZephAcpAgentState`.
//!
//! Groups the `/help`, `/mode`, `/clear`, and `/model` command dispatch and the
//! `AvailableCommandsUpdate` notification helper so the slash-command surface is isolated
//! from session lifecycle and prompt-turn logic in [`super`]. `/review`'s prompt-text builder
//! (`build_review_prompt`) also lives here, but its dispatch is handled by `do_prompt`
//! (`turn.rs`) directly rather than by [`ZephAcpAgentState::handle_slash_command`] below
//! (#6673 — see `is_acp_native_slash_command`'s doc in `mod.rs` for why).

use agent_client_protocol as acp;
use zeph_core::channel::ChannelMessage;

#[cfg(test)]
use super::{AgentSpawner, ProviderFactory, SessionConfigSeed, ZephAcpAgent};
use super::{ZephAcpAgentState, build_available_commands};
#[cfg(test)]
use tokio::sync::mpsc;

/// Build the expanded `/review [path]` prompt text, validating `arg` first.
///
/// Pure and side-effect-free — used by `do_prompt` (`turn.rs`) to expand `/review` into a real
/// prompt that flows through the normal `acquire_prompt_channels`/drain turn machinery (#6673),
/// rather than being dispatched fire-and-forget the way the other ACP-native slash commands are.
pub(crate) fn build_review_prompt(arg: &str) -> acp::Result<String> {
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
    Ok(if arg.is_empty() {
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
    })
}

impl ZephAcpAgentState {
    /// Dispatch a slash command, returning a short-circuit `PromptResponse`.
    pub(crate) async fn handle_slash_command(
        &self,
        session_id: &acp::schema::v1::SessionId,
        text: &str,
    ) -> acp::Result<acp::schema::v1::PromptResponse> {
        let mut parts = text.splitn(2, ' ');
        let cmd = parts.next().unwrap_or("").trim();
        let arg = parts.next().unwrap_or("").trim();

        let reply = match cmd {
            // #5986: render from the same `zeph_commands::COMMANDS` registry the CLI/TUI
            // `CommandRegistry` uses, instead of a hand-rolled 5-command literal that drifted
            // out of sync with the real command set (49 commands).
            "/help" => zeph_commands::render_help_text(),
            "/model" => self.handle_model_command(session_id, arg).await?,
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
                    *entry.current_mode.lock() = acp::schema::v1::SessionModeId::new(arg);
                }
                let update = acp::schema::v1::SessionUpdate::CurrentModeUpdate(
                    acp::schema::v1::CurrentModeUpdate::new(acp::schema::v1::SessionModeId::new(
                        arg,
                    )),
                );
                let notification =
                    acp::schema::v1::SessionNotification::new(session_id.clone(), update);
                if let Err(e) = self.send_notification(session_id, notification).await {
                    tracing::warn!(error = %e, "failed to send current_mode_update from /mode");
                }
                format!("Switched to mode: {arg}")
            }
            "/clear" => {
                if let Some(ref store) = self.store {
                    let sid = session_id.to_string();
                    let store = store.clone();
                    let owner_key = self.owner_key.clone();
                    // EXEMPT(#5144): fire-and-forget DB delete+recreate; independent per-session
                    // operation — supervisor adds no meaningful lifecycle observability here.
                    tokio::spawn(async move {
                        // Scoped to owner (#5868): this is our own session (already established
                        // for this connection), so `_for_owner` here is a defense-in-depth match
                        // of the create below, not a new access-control decision.
                        if let Err(e) = store.delete_acp_session_for_owner(&sid, &owner_key).await {
                            tracing::warn!(error = %e, "failed to clear session history");
                        }
                        if let Err(e) = store.create_acp_session(&sid, Some(&owner_key)).await {
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
                        // #6419: see do_prompt above.
                        owner_key: Some(self.owner_key.clone()),
                    });
                }
                "Session history cleared.".to_owned()
            }
            _ => {
                return Err(acp::Error::invalid_request().data(format!("unknown command: {cmd}")));
            }
        };

        let update = acp::schema::v1::SessionUpdate::AgentMessageChunk(
            acp::schema::v1::ContentChunk::new(reply.clone().into()),
        );
        let notification = acp::schema::v1::SessionNotification::new(session_id.clone(), update);
        if let Err(e) = self.send_notification(session_id, notification).await {
            tracing::warn!(error = %e, "failed to send command reply");
        }

        Ok(acp::schema::v1::PromptResponse::new(
            acp::schema::v1::StopReason::EndTurn,
        ))
    }

    async fn handle_model_command(
        &self,
        session_id: &acp::schema::v1::SessionId,
        arg: &str,
    ) -> acp::Result<String> {
        let available_models = self.available_models_snapshot();
        if arg.is_empty() {
            let models = available_models.join(", ");
            return Ok(format!("Available models: {models}"));
        }
        // #5986: previously fell through to `resolve_model_fuzzy("refresh")`, which failed with
        // an "no matching model found" error instead of refreshing the model list — unlike the
        // CLI/TUI's documented `/model refresh` behavior.
        if arg == "refresh" {
            return Ok(self.model_refresh_as_string(session_id).await);
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

    /// Fire-and-forget the `AvailableCommandsUpdate` notification for a session.
    pub(crate) fn send_commands_update_nowait(&self, session_id: &acp::schema::v1::SessionId) {
        let cmds_update = acp::schema::v1::SessionUpdate::AvailableCommandsUpdate(
            acp::schema::v1::AvailableCommandsUpdate::new(build_available_commands()),
        );
        self.send_notification_nowait(
            session_id,
            acp::schema::v1::SessionNotification::new(session_id.clone(), cmds_update),
        );
    }
}

/// Regression tests for #5986: ACP's `/help` and `/model refresh` slash commands.
#[cfg(test)]
mod slash_command_wiring_tests {
    use std::sync::Arc;

    use parking_lot::RwLock;
    use zeph_core::channel::LoopbackChannel;
    use zeph_llm::any::AnyProvider;

    use super::*;

    /// Builds a bare agent (no registered session), optionally with a provider factory.
    fn make_agent(
        provider_factory: Option<ProviderFactory>,
        available_models: Vec<String>,
    ) -> (ZephAcpAgent, acp::schema::v1::SessionId) {
        let spawner: AgentSpawner = Arc::new(|_ch, _ctx, _sc| Box::pin(async {}));
        let mut agent = ZephAcpAgent::new(spawner, 4, 1800, None);
        if let Some(factory) = provider_factory {
            agent = agent.with_provider_factory(factory, Arc::new(RwLock::new(available_models)));
        }
        let session_id = acp::schema::v1::SessionId::new("slash-cmd-test".to_owned());
        (agent, session_id)
    }

    /// Builds an agent with one registered session, returning a receiver that yields the next
    /// `SessionNotification` `send_notification` pushes for that session — draining and acking
    /// it immediately, mirroring `spawn_notify_drainer`'s ack contract without a real ACP
    /// connection.
    fn make_agent_with_captured_session(
        provider_factory: Option<ProviderFactory>,
        available_models: Vec<String>,
    ) -> (
        ZephAcpAgent,
        acp::schema::v1::SessionId,
        tokio::sync::oneshot::Receiver<acp::schema::v1::SessionNotification>,
    ) {
        let (agent, session_id) = make_agent(provider_factory, available_models);

        let (_, handle) = LoopbackChannel::pair(4);
        let provider_override = Arc::new(RwLock::new(None::<AnyProvider>));
        let (notify_tx, notify_rx) = mpsc::channel(4);
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
        agent.sessions.lock().insert(session_id.clone(), entry);

        let mut taken_rx = agent
            .sessions
            .lock()
            .get(&session_id)
            .expect("session was just inserted")
            .notify_rx
            .lock()
            .take()
            .expect("notify_rx must not yet be consumed");

        let (captured_tx, captured_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            if let Some((notif, ack)) = taken_rx.recv().await {
                ack.send(()).ok();
                captured_tx.send(notif).ok();
            }
        });

        (agent, session_id, captured_rx)
    }

    /// #5986: before this PR, ACP's `/help` returned a hand-rolled 5-command literal
    /// (`/help`, `/model`, `/mode`, `/clear`, `/review`) that had drifted out of sync with the
    /// real `zeph_commands::COMMANDS` registry the CLI/TUI `/help` renders from. Confirms
    /// `handle_slash_command` now renders exactly what `zeph_commands::render_help_text()`
    /// produces, and that commands absent from the old hardcoded list are present.
    #[tokio::test]
    async fn help_command_renders_full_command_registry_not_hardcoded_five() {
        let (agent, session_id, captured_rx) = make_agent_with_captured_session(None, vec![]);

        let response = agent
            .handle_slash_command(&session_id, "/help")
            .await
            .expect("/help must succeed");
        assert_eq!(response.stop_reason, acp::schema::v1::StopReason::EndTurn);

        let notification = captured_rx
            .await
            .expect("handle_slash_command must push a notification carrying the /help reply");
        let acp::schema::v1::SessionUpdate::AgentMessageChunk(chunk) = notification.update else {
            panic!("expected AgentMessageChunk carrying the /help reply");
        };
        let acp::schema::v1::ContentBlock::Text(text) = chunk.content else {
            panic!("expected ContentBlock::Text");
        };

        assert_eq!(
            text.text,
            zeph_commands::render_help_text(),
            "ACP's /help reply must match zeph_commands::render_help_text() verbatim"
        );
        for cmd in ["/skills", "/memory", "/compact"] {
            assert!(
                text.text.contains(cmd),
                "/help must list {cmd}, which is absent from the old hardcoded 5-command string"
            );
        }
        assert!(
            !text.text.contains("Available commands:"),
            "/help must no longer render the old hardcoded heading"
        );
    }

    /// #5986: `/model refresh` previously fell through to `resolve_model_fuzzy("refresh")`,
    /// which failed with "no matching model found" since `"refresh"` never matches a real model
    /// key. With no provider factory configured at all, the new refresh path must short-circuit
    /// before touching session state or any network call and return an informational message,
    /// not an error.
    #[tokio::test]
    async fn model_refresh_with_no_provider_factory_returns_ok_not_fuzzy_match_error() {
        let (agent, session_id) = make_agent(None, vec![]);

        let reply = agent
            .handle_model_command(&session_id, "refresh")
            .await
            .expect("/model refresh must succeed, not error via resolve_model_fuzzy");
        assert_eq!(reply, "model switching not configured");
    }

    /// #5986 M1 (critic finding): a prior implementation looped sequentially over every
    /// configured provider slug with an independent 5s timeout each, which could block a
    /// session's `do_prompt` handler for up to 5s * N providers. The fix refreshes only the
    /// session's currently active provider — mirroring `Agent::model_refresh_as_string`'s
    /// single-active-provider semantics — via the shared `warm_model_caches` helper. When the
    /// session's active model key does not resolve through the provider factory, the reply must
    /// still be `Ok`, naming the unresolved model, without ever reaching `list_models_remote`.
    #[tokio::test]
    async fn model_refresh_unresolvable_active_model_returns_ok_with_model_name() {
        let factory: ProviderFactory = Arc::new(|_key: &str| None);
        let (agent, session_id, _captured_rx) =
            make_agent_with_captured_session(Some(factory), vec!["testslug:model-a".to_owned()]);

        let reply = agent
            .handle_model_command(&session_id, "refresh")
            .await
            .expect("/model refresh must succeed");
        assert_eq!(reply, "unknown model: test-model");
    }

    /// #5986 M1 companion: an unregistered/stale session id must not panic or reach the
    /// provider factory — `model_refresh_as_string` looks up the session before resolving any
    /// provider.
    #[tokio::test]
    async fn model_refresh_missing_session_returns_ok_session_not_found() {
        let factory: ProviderFactory = Arc::new(|_key: &str| None);
        let (agent, session_id) = make_agent(Some(factory), vec![]);

        let reply = agent
            .handle_model_command(&session_id, "refresh")
            .await
            .expect("/model refresh must succeed");
        assert_eq!(reply, "session not found");
    }

    /// #5986 success-path companion: when the session's active model *does* resolve through the
    /// provider factory, `/model refresh` must reach `warm_model_caches` and report the live
    /// fetch count. Uses `AnyProvider::Mock` (`zeph_llm::mock::MockProvider`) rather than a real
    /// network-backed provider — `list_models_remote()` on the `Mock` variant returns
    /// `Ok(p.models.clone())` synchronously (`zeph_llm::any`'s `AnyProvider::list_models_remote`
    /// match arm), so this exercises the real success branch with zero network I/O, closing the
    /// gap the developer's handoff flagged as needing a `wiremock`-backed provider.
    #[tokio::test]
    async fn model_refresh_active_provider_success_reports_fetched_count() {
        let mock_provider = zeph_llm::any::AnyProvider::Mock(
            zeph_llm::mock::MockProvider::with_responses(vec![]).with_models(vec![
                zeph_llm::model_cache::RemoteModelInfo {
                    id: "model-a".to_owned(),
                    display_name: "Model A".to_owned(),
                    context_window: None,
                    created_at: None,
                },
                zeph_llm::model_cache::RemoteModelInfo {
                    id: "model-b".to_owned(),
                    display_name: "Model B".to_owned(),
                    context_window: None,
                    created_at: None,
                },
            ]),
        );
        let factory: ProviderFactory = Arc::new(move |_key: &str| Some(mock_provider.clone()));
        let (agent, session_id, _captured_rx) = make_agent_with_captured_session(
            Some(factory),
            vec!["acp-test-refresh-mock-slug:model-a".to_owned()],
        );

        let reply = agent
            .handle_model_command(&session_id, "refresh")
            .await
            .expect("/model refresh must succeed");
        assert_eq!(
            reply, "Fetched 2 models.",
            "must report the live list_models_remote() count from the resolved active provider"
        );
    }
}

/// Regression tests for #6419: ACP already derives a real per-connection identity
/// (`ZephAcpAgentState::owner_key`, #5868) used to scope persisted ACP session
/// list/load/resume access. Before this fix, that identity was never threaded into the
/// `ChannelMessage.owner_key` sent to the agent loop, so every ACP turn silently fell back to
/// the shared `DEFAULT_OWNER_KEY = "local"` cross-thread-store bucket (#6389) — the same bucket
/// CLI/TUI/Telegram use — even though ACP already has a stronger per-caller identity available.
#[cfg(test)]
mod owner_key_threading_tests {
    use std::sync::Arc;

    use parking_lot::RwLock;
    use zeph_core::channel::{Channel as _, LoopbackChannel};
    use zeph_llm::any::AnyProvider;

    use super::*;

    /// Builds an agent with the given connection `owner_key` and one registered session,
    /// returning the `LoopbackChannel` half so the test can `recv()` whatever `ChannelMessage`
    /// the handler under test forwards to `input_tx`.
    fn make_agent_with_owner_key(
        owner_key: &str,
    ) -> (ZephAcpAgent, acp::schema::v1::SessionId, LoopbackChannel) {
        let spawner: AgentSpawner = Arc::new(|_ch, _ctx, _sc| Box::pin(async {}));
        let agent = ZephAcpAgent::new(spawner, 4, 1800, None).with_owner_key(owner_key);
        let session_id = acp::schema::v1::SessionId::new("owner-key-test".to_owned());

        let (channel, handle) = LoopbackChannel::pair(4);
        let provider_override = Arc::new(RwLock::new(None::<AnyProvider>));
        let (notify_tx, notify_rx) = mpsc::channel(4);
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
        agent.sessions.lock().insert(session_id.clone(), entry);
        (agent, session_id, channel)
    }

    #[tokio::test]
    async fn review_command_threads_connection_owner_key_into_channel_message() {
        let (agent, session_id, mut channel) = make_agent_with_owner_key("acp:alice");

        let review_request = acp::schema::v1::PromptRequest::new(
            session_id.clone(),
            vec![acp::schema::v1::ContentBlock::Text(
                acp::schema::v1::TextContent::new("/review".to_owned()),
            )],
        );

        // #6673: `/review` is now routed through the normal `do_prompt` turn machinery, so a
        // stand-in "agent loop" must receive the forwarded prompt and flush to end the turn —
        // unlike the pre-#6673 fire-and-forget dispatch this test used to exercise directly.
        let agent_loop = tokio::spawn(async move {
            let msg = channel
                .recv()
                .await
                .expect("recv must not error")
                .expect("do_prompt must forward the expanded /review prompt");
            channel.flush_chunks().await.unwrap();
            msg
        });

        agent
            .do_prompt(review_request)
            .await
            .expect("/review must succeed");

        let msg = agent_loop
            .await
            .expect("agent-loop stand-in must not panic");
        assert_eq!(
            msg.owner_key.as_deref(),
            Some("acp:alice"),
            "/review must carry the connection's owner_key, not fall back to None/local"
        );
        // Guards against a mutation that deletes the `build_review_prompt` expansion (e.g.
        // forwarding the raw "/review" text unchanged): only the owner_key threading would
        // still be exercised by the assertion above, since it doesn't depend on the prompt
        // text at all.
        assert_eq!(
            msg.text,
            build_review_prompt("").expect("empty arg must always build successfully"),
            "do_prompt must forward /review's build_review_prompt expansion, not raw text"
        );
    }

    #[tokio::test]
    async fn clear_command_threads_connection_owner_key_into_channel_message() {
        let (agent, session_id, mut channel) = make_agent_with_owner_key("acp:bob");

        agent
            .handle_slash_command(&session_id, "/clear")
            .await
            .expect("/clear must succeed");

        let msg = channel
            .recv()
            .await
            .expect("recv must not error")
            .expect("/clear must forward a sentinel ChannelMessage");
        assert_eq!(msg.text, "/clear");
        assert_eq!(
            msg.owner_key.as_deref(),
            Some("acp:bob"),
            "/clear sentinel must carry the connection's owner_key, not fall back to None/local"
        );
    }
}
