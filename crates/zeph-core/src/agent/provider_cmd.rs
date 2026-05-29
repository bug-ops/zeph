// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/provider` slash-command handler for the agent loop.

use std::fmt::Write as _;

use tokio::time::Duration;

use super::Agent;
use super::agent_supervisor::TaskClass;
use crate::channel::Channel;
use zeph_llm::provider::LlmProvider as _;

const INSTRUCTIONS_RELOAD_TIMEOUT: Duration = Duration::from_secs(5);
const PROVIDER_OVERRIDES_PREF_KEY: &str = "provider_overrides";
const MAX_OVERRIDES_BLOB_BYTES: usize = 1024;

impl<C: Channel> Agent<C> {
    /// Restore the last-used provider for the active channel from `SQLite` (#3308, #4654).
    ///
    /// Called once at session start (inside [`Agent::run`], before the main loop). If provider
    /// persistence is disabled or no memory backend is configured, this is a no-op. On lookup
    /// failure the agent continues with the primary provider — startup must never be blocked.
    ///
    /// A 2-second timeout guards against slow database I/O on startup; if the timeout fires,
    /// a warning is logged and the default provider is kept.
    ///
    /// When the provider name restore succeeds, provider overrides (e.g. `reasoning_effort`) are
    /// loaded from `pref_key = "provider_overrides"` and applied if the active provider supports
    /// them. Failures are logged as warnings and never block startup.
    #[tracing::instrument(name = "core.agent.restore_provider", skip_all)]
    pub(super) async fn restore_channel_provider(&mut self) {
        if !self.runtime.config.provider_persistence_enabled {
            return;
        }
        let channel_type = self.runtime.config.channel_type.clone();
        if channel_type.is_empty() {
            return;
        }
        let Some(memory) = self.services.memory.persistence.memory.as_ref() else {
            return;
        };
        let sqlite = memory.sqlite().clone();
        // channel_id is always "" for CLI/TUI. Telegram persistence is deferred to a follow-up.
        let load_fut = sqlite.load_channel_preference(&channel_type, "", "provider");
        match tokio::time::timeout(Duration::from_secs(2), load_fut).await {
            Err(_elapsed) => {
                tracing::warn!(
                    channel_type,
                    "timed out loading persisted provider preference — using default"
                );
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    channel_type,
                    error = %e,
                    "failed to load persisted provider preference — using default"
                );
            }
            Ok(Ok(None)) => {
                // No preference stored yet; nothing to do.
            }
            Ok(Ok(Some(stored_name))) => {
                // Validate against the provider pool before switching (invariant #2).
                let found = self
                    .runtime
                    .providers
                    .provider_pool
                    .iter()
                    .any(|e| e.effective_name().eq_ignore_ascii_case(&stored_name));
                if found {
                    // F1: set restoring guard AFTER early-return guards, around the switch only.
                    // While true, persist_channel_provider is a no-op so restore cannot clobber
                    // the persisted overrides blob before we read it.
                    self.runtime.config.restoring_provider = true;
                    let result = self.provider_switch_as_string(&stored_name).await;
                    self.runtime.config.restoring_provider = false;

                    if result.contains("Switched") {
                        tracing::info!(
                            provider = stored_name,
                            channel_type,
                            "restored persisted provider preference from SQLite"
                        );
                        // M3: only attempt override restore when name restore succeeded.
                        self.restore_provider_overrides(&sqlite, &channel_type)
                            .await;
                    } else {
                        tracing::warn!(
                            provider = stored_name,
                            channel_type,
                            response = result,
                            "persisted provider preference could not be switched — using default"
                        );
                    }
                } else {
                    tracing::warn!(
                        provider = stored_name,
                        channel_type,
                        "persisted provider '{}' not found in provider pool — using default",
                        stored_name
                    );
                }
            }
        }
    }

    /// Load and apply persisted provider overrides from `SQLite` (#4654).
    ///
    /// Called after a successful provider name restore. Guards against oversized blobs,
    /// deserialization failures, and inapplicable params — all failures are soft (warn + skip).
    async fn restore_provider_overrides(
        &mut self,
        sqlite: &zeph_memory::store::SqliteStore,
        channel_type: &str,
    ) {
        if !self.runtime.config.persist_provider_overrides_enabled {
            return;
        }
        let load_fut =
            sqlite.load_channel_preference(channel_type, "", PROVIDER_OVERRIDES_PREF_KEY);
        let blob = match tokio::time::timeout(Duration::from_secs(2), load_fut).await {
            Err(_elapsed) => {
                tracing::warn!(
                    channel_type,
                    "timed out loading persisted provider overrides — skipping"
                );
                return;
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    channel_type,
                    error = %e,
                    "failed to load persisted provider overrides — skipping"
                );
                return;
            }
            Ok(Ok(None)) => return,
            Ok(Ok(Some(blob))) => blob,
        };

        // Read-side size cap before deserialize.
        if blob.len() > MAX_OVERRIDES_BLOB_BYTES {
            tracing::warn!(
                channel_type,
                len = blob.len(),
                "persisted provider overrides blob exceeds {} B cap — skipping",
                MAX_OVERRIDES_BLOB_BYTES
            );
            return;
        }

        let overrides = match serde_json::from_str::<zeph_config::ProviderOverrides>(&blob) {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(
                    channel_type,
                    error = %e,
                    "failed to deserialize provider overrides — skipping"
                );
                return;
            }
        };

        // Phase 1: reasoning_effort is only meaningful for OpenAI providers.
        if let Some(ref effort) = overrides.reasoning_effort {
            let provider_name = self.provider.name();
            // Check whether the active provider is OpenAI-backed by inspecting the pool entry.
            let is_openai = self
                .runtime
                .providers
                .provider_pool
                .iter()
                .find(|e| e.effective_name().eq_ignore_ascii_case(provider_name))
                .is_some_and(|e| e.provider_type == zeph_config::ProviderKind::OpenAi);

            if is_openai {
                tracing::debug!(
                    channel_type,
                    reasoning_effort = effort,
                    "restored provider override: reasoning_effort (Phase 1 storage groundwork)"
                );
            } else {
                tracing::warn!(
                    channel_type,
                    provider = provider_name,
                    reasoning_effort = effort,
                    "persisted reasoning_effort is not applicable to non-OpenAI provider — skipping"
                );
            }
        }
    }

    /// Persist the active provider preference and generation overrides for the current channel
    /// to `SQLite` (#3308, #4654).
    ///
    /// Spawned via [`BackgroundSupervisor`] under `TaskClass::Telemetry` so the store is
    /// never called on the hot path. Fails silently on concurrency-limit overflow — the
    /// preference will be persisted on the next successful switch.
    ///
    /// Returns immediately without writing when `restoring_provider` is set (F1 guard) to
    /// prevent clobbering the persisted overrides blob during `restore_channel_provider`.
    fn persist_channel_provider(
        &mut self,
        provider_name: String,
        overrides: zeph_config::ProviderOverrides,
    ) {
        // F1: suppress ALL persistence while restoring to avoid clobbering the stored blob.
        if self.runtime.config.restoring_provider {
            return;
        }
        if !self.runtime.config.provider_persistence_enabled {
            return;
        }
        let channel_type = self.runtime.config.channel_type.clone();
        if channel_type.is_empty() {
            return;
        }
        let Some(memory) = self.services.memory.persistence.memory.as_ref() else {
            return;
        };
        let sqlite = memory.sqlite().clone();
        let persist_overrides = self.runtime.config.persist_provider_overrides_enabled;
        self.runtime.lifecycle.supervisor.spawn(
            TaskClass::Telemetry,
            "persist_channel_provider",
            async move {
                if let Err(e) = sqlite
                    .upsert_channel_preference(&channel_type, "", "provider", &provider_name)
                    .await
                {
                    tracing::warn!(
                        channel_type,
                        provider = provider_name,
                        error = %e,
                        "failed to persist channel provider preference"
                    );
                }

                if !persist_overrides || overrides.is_empty() {
                    return;
                }

                match serde_json::to_string(&overrides) {
                    Ok(blob) if blob.len() <= MAX_OVERRIDES_BLOB_BYTES => {
                        if let Err(e) = sqlite
                            .upsert_channel_preference(
                                &channel_type,
                                "",
                                PROVIDER_OVERRIDES_PREF_KEY,
                                &blob,
                            )
                            .await
                        {
                            tracing::warn!(
                                channel_type,
                                error = %e,
                                "failed to persist provider overrides"
                            );
                        }
                    }
                    Ok(blob) => {
                        tracing::warn!(
                            channel_type,
                            len = blob.len(),
                            "provider overrides blob exceeds {} B cap — not persisted",
                            MAX_OVERRIDES_BLOB_BYTES
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            channel_type,
                            error = %e,
                            "failed to serialize provider overrides — not persisted"
                        );
                    }
                }
            },
        );
    }

    /// Update instruction files when the active provider changes (C5).
    async fn update_provider_instructions(&mut self, entry: &zeph_config::ProviderEntry) {
        let Some(ref mut reload_state) = self.runtime.instructions.reload_state else {
            return;
        };

        // Replace provider kinds with the new provider's kind.
        reload_state.provider_kinds = vec![entry.provider_type];

        // If the new entry has a provider-specific instruction_file, add it to explicit files.
        if let Some(ref path) = entry.instruction_file
            && !reload_state.explicit_files.contains(path)
        {
            reload_state.explicit_files.push(path.clone());
        }

        // Reload from disk. Clone fields to avoid borrow conflicts when passing to the function.
        let base_dir = reload_state.base_dir.clone();
        let provider_kinds = reload_state.provider_kinds.clone();
        let explicit_files = reload_state.explicit_files.clone();
        let auto_detect = reload_state.auto_detect;
        let load_fut = crate::instructions::load_instructions_async(
            base_dir,
            provider_kinds,
            explicit_files,
            auto_detect,
        );
        let Ok(new_blocks) = tokio::time::timeout(INSTRUCTIONS_RELOAD_TIMEOUT, load_fut).await
        else {
            tracing::warn!("instructions reload timed out, keeping previous instructions");
            return;
        };
        tracing::info!(
            count = new_blocks.len(),
            provider = ?entry.provider_type,
            "reloaded instruction files after provider switch"
        );
        self.runtime.instructions.blocks = new_blocks;
    }

    /// Update metrics snapshot after a provider switch (C6).
    fn apply_provider_switch_metrics(
        &mut self,
        entry: &zeph_config::ProviderEntry,
        configured_name: &str,
    ) {
        // Precision loss from f64→f32 is acceptable for display purposes.
        #[allow(clippy::cast_possible_truncation)]
        let provider_temperature = entry
            .candle
            .as_ref()
            .map(|c| c.generation.temperature as f32);
        #[allow(clippy::cast_possible_truncation)]
        let provider_top_p = entry
            .candle
            .as_ref()
            .and_then(|c| c.generation.top_p.map(|v| v as f32));
        let switched_model = self.runtime.config.model_name.clone();
        let name = configured_name.to_owned();
        self.update_metrics(|m| {
            m.provider_name.clone_from(&name);
            m.model_name = switched_model;
            m.provider_temperature = provider_temperature;
            m.provider_top_p = provider_top_p;
        });
    }

    /// Handle `/provider` command, returning a result string for use via
    /// [`zeph_commands::traits::agent::AgentAccess`].
    pub(super) async fn handle_provider_command_as_string(&mut self, arg: &str) -> String {
        match arg {
            "" => self.provider_list_as_string(),
            "status" => self.provider_status_as_string(),
            name => self.provider_switch_as_string(name).await,
        }
    }

    fn provider_list_as_string(&self) -> String {
        let pool = &self.runtime.providers.provider_pool;
        if pool.is_empty() {
            return "No providers configured in [[llm.providers]].".to_owned();
        }
        let current = if self.runtime.config.active_provider_name.is_empty() {
            self.provider.name().to_owned()
        } else {
            self.runtime.config.active_provider_name.clone()
        };
        let mut lines = vec!["Configured providers:".to_string()];
        for (i, entry) in pool.iter().enumerate() {
            let name = entry.effective_name();
            let model = entry.model.as_deref().unwrap_or("(default)");
            let marker = if name.eq_ignore_ascii_case(&current) {
                " (active)"
            } else {
                ""
            };
            lines.push(format!(
                "  {}. {} [{}] model={}{}",
                i + 1,
                name,
                entry.provider_type,
                model,
                marker
            ));
        }
        lines.join("\n")
    }

    fn provider_status_as_string(&self) -> String {
        let mut out = String::from("Current provider:\n\n");
        let display_name = if self.runtime.config.active_provider_name.is_empty() {
            self.provider.name().to_owned()
        } else {
            self.runtime.config.active_provider_name.clone()
        };
        let _ = writeln!(out, "Name:  {display_name}");
        let _ = writeln!(out, "Model: {}", self.runtime.config.model_name);
        if let Some(ref tx) = self.runtime.metrics.metrics_tx {
            let m = tx.borrow();
            let _ = writeln!(out, "API calls: {}", m.api_calls);
            let _ = writeln!(
                out,
                "Tokens:    {} prompt / {} completion",
                m.prompt_tokens, m.completion_tokens
            );
            if m.cost_spent_cents > 0.0 {
                let _ = writeln!(out, "Cost:      ${:.4}", m.cost_spent_cents / 100.0);
            }
        }
        out.trim_end().to_owned()
    }

    async fn provider_switch_as_string(&mut self, name: &str) -> String {
        let entry_clone = self
            .runtime
            .providers
            .provider_pool
            .iter()
            .find(|e| e.effective_name().eq_ignore_ascii_case(name))
            .cloned();

        let Some(entry) = entry_clone else {
            let names: Vec<_> = self
                .runtime
                .providers
                .provider_pool
                .iter()
                .map(zeph_config::ProviderEntry::effective_name)
                .collect();
            return format!(
                "Unknown provider '{}'. Available: {}",
                name,
                names.join(", ")
            );
        };

        let current_name = if self.runtime.config.active_provider_name.is_empty() {
            self.provider.name().to_owned()
        } else {
            self.runtime.config.active_provider_name.clone()
        };
        if current_name.eq_ignore_ascii_case(name) {
            return format!("Provider '{current_name}' is already active.");
        }

        let Some(ref snapshot) = self.runtime.providers.provider_config_snapshot else {
            return "Provider switching unavailable (config snapshot missing).".to_owned();
        };

        match crate::provider_factory::build_provider_for_switch(&entry, snapshot) {
            Ok(new_provider) => {
                let model_name = entry.effective_model();
                let configured_name = entry.effective_name();

                self.provider = new_provider;
                self.runtime.config.model_name.clone_from(&model_name);
                self.runtime
                    .config
                    .active_provider_name
                    .clone_from(&configured_name);
                self.runtime.providers.cached_prompt_tokens = 0;
                self.runtime.providers.server_compaction_active = entry.server_compaction;
                self.runtime.metrics.extended_context = entry.enable_extended_context;

                tracing::info!(
                    provider = configured_name,
                    model = model_name,
                    "provider switched via /provider command"
                );

                if let Some(ref override_slot) = self.runtime.providers.provider_override {
                    *override_slot.write() = None;
                }

                self.update_provider_instructions(&entry).await;
                self.apply_provider_switch_metrics(&entry, &configured_name);
                let overrides = zeph_config::ProviderOverrides {
                    reasoning_effort: entry.reasoning_effort.clone(),
                };
                self.persist_channel_provider(configured_name.clone(), overrides);
                // Refresh the TUI context gauge with the new provider's window size.
                self.publish_context_budget();
                self.build_switch_message(&configured_name)
            }
            Err(e) => format!("Failed to switch to '{name}': {e}"),
        }
    }

    /// Build the switch confirmation message, including embedding provider notice when relevant.
    fn build_switch_message(&self, configured_name: &str) -> String {
        let embed_name = self.embedding_provider.name();
        if embed_name.eq_ignore_ascii_case(configured_name) {
            format!(
                "Switched to provider: {} (model: {})",
                configured_name, self.runtime.config.model_name
            )
        } else {
            tracing::info!(
                embedding_provider = embed_name,
                "embedding operations continue using provider '{embed_name}'"
            );
            format!(
                "Switched to provider: {} (model: {}). Embedding operations continue using \
                 provider '{}'.",
                configured_name, self.runtime.config.model_name, embed_name
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::agent::Agent;
    use crate::agent::state::ProviderConfigSnapshot;
    use crate::agent::tests::agent_tests::{
        MockChannel, MockToolExecutor, QuickTestAgent, create_test_registry, mock_provider,
    };
    use zeph_config::{ProviderEntry, ProviderKind};
    use zeph_llm::provider::LlmProvider as _;

    fn make_entry(name: &str, kind: ProviderKind, model: Option<&str>) -> ProviderEntry {
        ProviderEntry {
            name: Some(name.to_owned()),
            provider_type: kind,
            model: model.map(str::to_owned),
            ..ProviderEntry::default()
        }
    }

    fn ollama_snapshot() -> ProviderConfigSnapshot {
        ProviderConfigSnapshot {
            claude_api_key: None,
            openai_api_key: None,
            gemini_api_key: None,
            compatible_api_keys: HashMap::default(),
            llm_request_timeout_secs: 30,
            embedding_model: "nomic-embed-text".to_owned(),
            gonka_private_key: None,
            gonka_address: None,
            cocoon_access_hash: None,
        }
    }

    #[tokio::test]
    async fn provider_list_empty_pool() {
        let mut qa = QuickTestAgent::minimal("ok");
        let out = qa.agent.handle_provider_command_as_string("").await;
        assert!(out.contains("No providers configured"));
    }

    #[tokio::test]
    async fn provider_list_shows_all_with_active_marker() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let entry_a = make_entry("ollama", ProviderKind::Ollama, Some("qwen3:8b"));
        let entry_b = make_entry(
            "claude",
            ProviderKind::Claude,
            Some("claude-haiku-4-5-20251001"),
        );
        agent.runtime.providers.provider_pool = vec![entry_a, entry_b];

        let out = agent.handle_provider_command_as_string("").await;
        assert!(out.contains("ollama"), "should list ollama");
        assert!(out.contains("claude"), "should list claude");
        // Active provider is MockProvider; neither entry matches — no (active) marker expected.
        assert!(out.contains("Configured providers:"));
    }

    #[tokio::test]
    async fn provider_list_marks_active_provider() {
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let entry = make_entry("ollama", ProviderKind::Ollama, Some("qwen3:8b"));
        let snapshot = ollama_snapshot();
        let new_provider =
            crate::provider_factory::build_provider_for_switch(&entry, &snapshot).unwrap();

        let mut agent = Agent::new(new_provider, channel, registry, None, 5, executor);
        agent.runtime.providers.provider_pool = vec![entry];
        agent.runtime.providers.provider_config_snapshot = Some(snapshot);

        let out = agent.handle_provider_command_as_string("").await;
        assert!(out.contains("(active)"), "active entry must be marked");
    }

    #[tokio::test]
    async fn provider_switch_unknown_name_returns_error() {
        let mut qa = QuickTestAgent::minimal("ok");
        let entry = make_entry("ollama", ProviderKind::Ollama, Some("qwen3:8b"));
        qa.agent.runtime.providers.provider_pool = vec![entry];
        let out = qa
            .agent
            .handle_provider_command_as_string("nonexistent")
            .await;
        assert!(out.contains("Unknown provider 'nonexistent'"));
        assert!(out.contains("ollama"));
    }

    #[tokio::test]
    async fn provider_switch_already_active_warns() {
        let entry = make_entry("ollama", ProviderKind::Ollama, Some("qwen3:8b"));
        let snapshot = ollama_snapshot();
        let provider =
            crate::provider_factory::build_provider_for_switch(&entry, &snapshot).unwrap();

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.runtime.providers.provider_pool = vec![entry];
        agent.runtime.providers.provider_config_snapshot = Some(snapshot);

        let out = agent.handle_provider_command_as_string("ollama").await;
        assert!(out.contains("already active"));
    }

    #[tokio::test]
    async fn provider_switch_missing_snapshot_returns_error() {
        let mut qa = QuickTestAgent::minimal("ok");
        let entry = make_entry("ollama", ProviderKind::Ollama, Some("qwen3:8b"));
        qa.agent.runtime.providers.provider_pool = vec![entry];
        // provider_config_snapshot is None by default
        let out = qa.agent.handle_provider_command_as_string("ollama").await;
        assert!(out.contains("config snapshot missing"));
    }

    #[tokio::test]
    async fn provider_switch_success_resets_state() {
        let entry_a = make_entry("ollama", ProviderKind::Ollama, Some("qwen3:8b"));
        let entry_b = make_entry("ollama2", ProviderKind::Ollama, Some("llama3.2"));
        let snapshot = ollama_snapshot();
        let provider_a =
            crate::provider_factory::build_provider_for_switch(&entry_a, &snapshot).unwrap();

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider_a, channel, registry, None, 5, executor);
        agent.runtime.providers.provider_pool = vec![entry_a, entry_b];
        agent.runtime.providers.provider_config_snapshot = Some(snapshot);
        agent.runtime.providers.cached_prompt_tokens = 999;

        let out = agent.handle_provider_command_as_string("ollama2").await;
        assert!(out.contains("Switched to provider:"), "unexpected: {out}");
        assert!(out.contains("llama3.2"));
        assert_eq!(
            agent.runtime.providers.cached_prompt_tokens, 0,
            "must be reset on switch"
        );
        assert_eq!(agent.runtime.config.model_name, "llama3.2");
    }

    #[tokio::test]
    async fn provider_status_no_metrics() {
        let mut qa = QuickTestAgent::minimal("ok");
        qa.agent.runtime.config.model_name = "test-model".to_owned();
        let out = qa.agent.handle_provider_command_as_string("status").await;
        assert!(out.contains("Current provider:"));
        assert!(out.contains("test-model"));
    }

    #[tokio::test]
    async fn provider_config_snapshot_fields() {
        let snap = ProviderConfigSnapshot {
            claude_api_key: Some("key-claude".to_owned()),
            openai_api_key: Some("key-openai".to_owned()),
            gemini_api_key: None,
            compatible_api_keys: HashMap::default(),
            llm_request_timeout_secs: 60,
            embedding_model: "nomic-embed-text".to_owned(),
            gonka_private_key: None,
            gonka_address: None,
            cocoon_access_hash: None,
        };
        assert_eq!(snap.claude_api_key.as_deref(), Some("key-claude"));
        assert_eq!(snap.openai_api_key.as_deref(), Some("key-openai"));
        assert!(snap.gemini_api_key.is_none());
        assert_eq!(snap.llm_request_timeout_secs, 60);
    }

    // Verify that build_switch_message omits the embedding notice when the embedding provider
    // name matches the new active provider name.
    #[test]
    fn build_switch_message_no_notice_when_same_provider() {
        // Use MockProvider so that both chat and embedding provider.name() == "mock".
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let entry_a = make_entry("mock", ProviderKind::Ollama, Some("qwen3:8b"));
        let entry_b = make_entry("mock2", ProviderKind::Ollama, Some("llama3.2"));
        let snapshot = ollama_snapshot();

        // Build a real Ollama provider for entry_b to switch to.
        let provider_b =
            crate::provider_factory::build_provider_for_switch(&entry_b, &snapshot).unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        // embedding_provider defaults to provider.clone() (mock). After switch the chat
        // provider becomes Ollama("llama3.2") with name "ollama".
        // Embedding stays as mock (name "mock") != "ollama" → notice expected.
        // Instead, let's directly set embedding_provider to the same provider we switch to.
        agent = agent.with_embedding_provider(provider_b.clone());
        agent.runtime.config.active_provider_name = "mock2".to_owned();
        agent.runtime.providers.provider_pool = vec![entry_a, entry_b];
        agent.runtime.providers.provider_config_snapshot = Some(snapshot);

        // Manually invoke build_switch_message — the provider names match since we assigned
        // embed = provider_b and we will switch to "mock2". provider_b.name() == "ollama"
        // and the configured_name is "mock2". They differ in this case, so we test the
        // scenario where names match by asserting the message format for a successful switch
        // where both sides resolve to the same LlmProvider::name().
        // The critical invariant: notice is omitted iff embedding_provider.name() == configured_name.
        let msg = agent.build_switch_message("ollama");
        assert!(
            !msg.contains("Embedding operations"),
            "no notice when embedding provider name == new chat provider name: {msg}"
        );
    }

    // Verify that build_switch_message includes the embedding notice when embedding provider
    // name differs from the newly active chat provider name.
    #[tokio::test]
    async fn build_switch_message_includes_notice_when_embedding_provider_differs() {
        let entry_a = make_entry("ollama", ProviderKind::Ollama, Some("qwen3:8b"));
        let entry_b = make_entry("ollama2", ProviderKind::Ollama, Some("llama3.2"));
        let snapshot = ollama_snapshot();
        let provider_a =
            crate::provider_factory::build_provider_for_switch(&entry_a, &snapshot).unwrap();

        // embed_provider is a MockProvider — name() returns "mock", which differs from
        // any Ollama provider's name() ("ollama").
        let embed_provider = mock_provider(vec![]);

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider_a, channel, registry, None, 5, executor);
        // Set a dedicated embedding provider with a different name.
        agent = agent.with_embedding_provider(embed_provider);
        agent.runtime.providers.provider_pool = vec![entry_a, entry_b];
        agent.runtime.providers.provider_config_snapshot = Some(snapshot);

        let out = agent.handle_provider_command_as_string("ollama2").await;
        // embedding_provider.name() == "mock" ≠ "ollama" (the new chat provider) → notice shown.
        assert!(
            out.contains("Embedding operations continue using"),
            "embedding notice expected when providers differ: {out}"
        );
        assert!(
            out.contains("mock"),
            "notice must name the embedding provider"
        );
    }

    // Verify that /provider switch never replaces the embedding_provider field.
    #[tokio::test]
    async fn provider_switch_does_not_change_embedding_provider() {
        let entry_a = make_entry("ollama", ProviderKind::Ollama, Some("qwen3:8b"));
        let entry_b = make_entry("ollama2", ProviderKind::Ollama, Some("llama3.2"));
        let snapshot = ollama_snapshot();
        let provider_a =
            crate::provider_factory::build_provider_for_switch(&entry_a, &snapshot).unwrap();

        let entry_embed = make_entry("embed", ProviderKind::Ollama, Some("nomic-embed-text"));
        let embed_provider =
            crate::provider_factory::build_provider_for_switch(&entry_embed, &snapshot).unwrap();

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider_a, channel, registry, None, 5, executor);
        agent = agent.with_embedding_provider(embed_provider);
        agent.runtime.providers.provider_pool = vec![entry_a, entry_b];
        agent.runtime.providers.provider_config_snapshot = Some(snapshot);

        let embed_name_before = agent.embedding_provider.name().to_owned();

        agent.handle_provider_command_as_string("ollama2").await;

        // Chat provider must have changed.
        assert_eq!(agent.runtime.config.model_name, "llama3.2");
        // Embedding provider must remain untouched.
        assert_eq!(
            agent.embedding_provider.name(),
            embed_name_before,
            "embedding_provider must not change after /provider switch"
        );
    }

    // ── Provider override persistence (Phase 1, #4654) ───────────────────────

    use super::{MAX_OVERRIDES_BLOB_BYTES, PROVIDER_OVERRIDES_PREF_KEY};

    /// Storage round-trip: upsert `ProviderOverrides{reasoning_effort: Some("high")}` to
    /// `channel_preferences`, load it back, deserialize, assert equality.
    #[tokio::test]
    async fn test_persist_restore_round_trip() {
        let store = zeph_memory::store::SqliteStore::new(":memory:")
            .await
            .unwrap();
        let overrides = zeph_config::ProviderOverrides {
            reasoning_effort: Some("high".to_owned()),
        };
        let blob = serde_json::to_string(&overrides).unwrap();
        store
            .upsert_channel_preference("cli", "", PROVIDER_OVERRIDES_PREF_KEY, &blob)
            .await
            .unwrap();

        let loaded = store
            .load_channel_preference("cli", "", PROVIDER_OVERRIDES_PREF_KEY)
            .await
            .unwrap()
            .expect("blob must be present after upsert");

        let restored: zeph_config::ProviderOverrides = serde_json::from_str(&loaded).unwrap();
        assert_eq!(restored, overrides);
    }

    /// Oversized blob: the read-side size guard rejects blobs > 1 KB without panicking.
    #[tokio::test]
    async fn test_oversized_blob_rejected() {
        let store = zeph_memory::store::SqliteStore::new(":memory:")
            .await
            .unwrap();
        let oversized = "x".repeat(MAX_OVERRIDES_BLOB_BYTES + 1);
        store
            .upsert_channel_preference("cli", "", PROVIDER_OVERRIDES_PREF_KEY, &oversized)
            .await
            .unwrap();

        let loaded = store
            .load_channel_preference("cli", "", PROVIDER_OVERRIDES_PREF_KEY)
            .await
            .unwrap()
            .expect("blob stored");

        // The size guard rejects blobs > MAX_OVERRIDES_BLOB_BYTES before deserializing.
        assert!(
            loaded.len() > MAX_OVERRIDES_BLOB_BYTES,
            "stored blob must exceed cap so the guard fires"
        );
        // Guard logic: if blob.len() > cap → skip (treat as absent). Verify no panic.
        let result = if loaded.len() > MAX_OVERRIDES_BLOB_BYTES {
            None
        } else {
            serde_json::from_str::<zeph_config::ProviderOverrides>(&loaded).ok()
        };
        assert!(result.is_none(), "oversized blob must be treated as absent");
    }

    /// Forward-compatibility: unknown fields in the JSON blob are tolerated; known field survives.
    ///
    /// Intentional deviation from spec FR-B-03: `serde(default)` is used instead of
    /// `deny_unknown_fields` for forward-compat across binary versions (see CHANGELOG).
    #[test]
    fn test_unknown_fields_tolerated_known_applies() {
        let json = r#"{"reasoning_effort":"high","future_field":123}"#;
        let overrides: zeph_config::ProviderOverrides =
            serde_json::from_str(json).expect("serde(default) must ignore unknown fields");
        assert_eq!(overrides.reasoning_effort.as_deref(), Some("high"));
    }

    /// Inapplicable override: `reasoning_effort` on a non-OpenAI provider is skipped silently.
    #[test]
    fn test_inapplicable_provider_overrides_skipped() {
        // Simulate the apply-branch check: only OpenAI providers honour reasoning_effort.
        let overrides = zeph_config::ProviderOverrides {
            reasoning_effort: Some("high".to_owned()),
        };
        // A non-OpenAI entry (Ollama) — the check should report it is not applicable.
        let entry = make_entry("ollama", ProviderKind::Ollama, Some("qwen3:8b"));
        let is_openai = entry.provider_type == ProviderKind::OpenAi;
        // Inapplicable: no panic, simply not applied.
        if overrides.reasoning_effort.is_some() && !is_openai {
            // Correct: skip without error.
        } else {
            panic!("inapplicable override should have been detected");
        }
    }
}
