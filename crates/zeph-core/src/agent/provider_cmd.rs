// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/provider` slash-command handler for the agent loop.

use std::fmt::Write as _;

use super::Agent;
use crate::channel::Channel;
use zeph_llm::provider::LlmProvider as _;

impl<C: Channel> Agent<C> {
    /// Dispatch `/provider`, `/provider <name>`, and `/provider status` commands.
    pub(super) async fn handle_provider_command(&mut self, trimmed: &str) {
        let arg = trimmed.strip_prefix("/provider").map_or("", str::trim);
        match arg {
            "" => self.handle_provider_list().await,
            "status" => self.handle_provider_status().await,
            name => self.handle_provider_switch(name).await,
        }
    }

    async fn handle_provider_list(&mut self) {
        let pool = &self.providers.provider_pool;
        if pool.is_empty() {
            let _ = self
                .channel
                .send("No providers configured in [[llm.providers]].")
                .await;
            return;
        }
        let current = if self.runtime.active_provider_name.is_empty() {
            self.provider.name().to_owned()
        } else {
            self.runtime.active_provider_name.clone()
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
        let _ = self.channel.send(&lines.join("\n")).await;
    }

    async fn handle_provider_status(&mut self) {
        let mut out = String::from("Current provider:\n\n");
        let display_name = if self.runtime.active_provider_name.is_empty() {
            self.provider.name().to_owned()
        } else {
            self.runtime.active_provider_name.clone()
        };
        let _ = writeln!(out, "Name:  {display_name}");
        let _ = writeln!(out, "Model: {}", self.runtime.model_name);
        if let Some(ref tx) = self.metrics.metrics_tx {
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
        let _ = self.channel.send(out.trim_end()).await;
    }

    async fn handle_provider_switch(&mut self, name: &str) {
        // Case-insensitive lookup.
        let entry_clone = self
            .providers
            .provider_pool
            .iter()
            .find(|e| e.effective_name().eq_ignore_ascii_case(name))
            .cloned();

        let Some(entry) = entry_clone else {
            let names: Vec<_> = self
                .providers
                .provider_pool
                .iter()
                .map(zeph_config::ProviderEntry::effective_name)
                .collect();
            let _ = self
                .channel
                .send(&format!(
                    "Unknown provider '{}'. Available: {}",
                    name,
                    names.join(", ")
                ))
                .await;
            return;
        };

        // Warn if the provider is already active.
        let current_name = if self.runtime.active_provider_name.is_empty() {
            self.provider.name().to_owned()
        } else {
            self.runtime.active_provider_name.clone()
        };
        if current_name.eq_ignore_ascii_case(name) {
            let _ = self
                .channel
                .send(&format!("Provider '{current_name}' is already active."))
                .await;
            return;
        }

        let Some(ref snapshot) = self.providers.provider_config_snapshot else {
            let _ = self
                .channel
                .send("Provider switching unavailable (config snapshot missing).")
                .await;
            return;
        };

        match crate::provider_factory::build_provider_for_switch(&entry, snapshot) {
            Ok(new_provider) => {
                // Resolve actual model name: use the entry's effective model (explicit or
                // provider-type default) instead of the provider type string returned by name().
                let model_name = entry.effective_model();
                // Use the configured name from [[llm.providers]] for display and metrics.
                let configured_name = entry.effective_name();

                self.provider = new_provider;
                self.runtime.model_name.clone_from(&model_name);
                self.runtime
                    .active_provider_name
                    .clone_from(&configured_name);

                // Reset state that is provider-specific.
                self.providers.cached_prompt_tokens = 0;
                self.providers.server_compaction_active = entry.server_compaction;

                // C1: Reset extended context flag (Claude-specific feature).
                self.metrics.extended_context = entry.enable_extended_context;

                // C2: Log provider switch in metrics for cost-tracking boundary awareness.
                tracing::info!(
                    provider = configured_name,
                    model = model_name,
                    "provider switched via /provider command"
                );

                // C3: Clear ACP provider override so the explicit switch takes effect.
                if let Some(ref override_slot) = self.providers.provider_override {
                    *override_slot.write() = None;
                }

                // C5: Update instruction file list for the new provider's kind.
                self.update_provider_instructions(&entry);

                self.apply_provider_switch_metrics(&entry, &configured_name);
                let _ = self
                    .channel
                    .send(&self.build_switch_message(&configured_name))
                    .await;
            }
            Err(e) => {
                let _ = self
                    .channel
                    .send(&format!("Failed to switch to '{name}': {e}"))
                    .await;
            }
        }
    }

    /// Update instruction files when the active provider changes (C5).
    fn update_provider_instructions(&mut self, entry: &zeph_config::ProviderEntry) {
        let Some(ref mut reload_state) = self.instructions.reload_state else {
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
        let new_blocks = crate::instructions::load_instructions(
            &base_dir,
            &provider_kinds,
            &explicit_files,
            auto_detect,
        );
        tracing::info!(
            count = new_blocks.len(),
            provider = ?entry.provider_type,
            "reloaded instruction files after provider switch"
        );
        self.instructions.blocks = new_blocks;
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
        let switched_model = self.runtime.model_name.clone();
        let name = configured_name.to_owned();
        self.update_metrics(|m| {
            m.provider_name.clone_from(&name);
            m.model_name = switched_model;
            m.provider_temperature = provider_temperature;
            m.provider_top_p = provider_top_p;
        });
    }

    /// Channel-free version of [`Self::handle_provider_command`] for use via
    /// [`zeph_commands::traits::agent::AgentAccess`].
    pub(super) fn handle_provider_command_as_string(&mut self, arg: &str) -> String {
        match arg {
            "" => self.provider_list_as_string(),
            "status" => self.provider_status_as_string(),
            name => self.provider_switch_as_string(name),
        }
    }

    fn provider_list_as_string(&self) -> String {
        let pool = &self.providers.provider_pool;
        if pool.is_empty() {
            return "No providers configured in [[llm.providers]].".to_owned();
        }
        let current = if self.runtime.active_provider_name.is_empty() {
            self.provider.name().to_owned()
        } else {
            self.runtime.active_provider_name.clone()
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
        let display_name = if self.runtime.active_provider_name.is_empty() {
            self.provider.name().to_owned()
        } else {
            self.runtime.active_provider_name.clone()
        };
        let _ = writeln!(out, "Name:  {display_name}");
        let _ = writeln!(out, "Model: {}", self.runtime.model_name);
        if let Some(ref tx) = self.metrics.metrics_tx {
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

    fn provider_switch_as_string(&mut self, name: &str) -> String {
        let entry_clone = self
            .providers
            .provider_pool
            .iter()
            .find(|e| e.effective_name().eq_ignore_ascii_case(name))
            .cloned();

        let Some(entry) = entry_clone else {
            let names: Vec<_> = self
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

        let current_name = if self.runtime.active_provider_name.is_empty() {
            self.provider.name().to_owned()
        } else {
            self.runtime.active_provider_name.clone()
        };
        if current_name.eq_ignore_ascii_case(name) {
            return format!("Provider '{current_name}' is already active.");
        }

        let Some(ref snapshot) = self.providers.provider_config_snapshot else {
            return "Provider switching unavailable (config snapshot missing).".to_owned();
        };

        match crate::provider_factory::build_provider_for_switch(&entry, snapshot) {
            Ok(new_provider) => {
                let model_name = entry.effective_model();
                let configured_name = entry.effective_name();

                self.provider = new_provider;
                self.runtime.model_name.clone_from(&model_name);
                self.runtime
                    .active_provider_name
                    .clone_from(&configured_name);
                self.providers.cached_prompt_tokens = 0;
                self.providers.server_compaction_active = entry.server_compaction;
                self.metrics.extended_context = entry.enable_extended_context;

                tracing::info!(
                    provider = configured_name,
                    model = model_name,
                    "provider switched via /provider command"
                );

                if let Some(ref override_slot) = self.providers.provider_override {
                    *override_slot.write() = None;
                }

                self.update_provider_instructions(&entry);
                self.apply_provider_switch_metrics(&entry, &configured_name);
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
                configured_name, self.runtime.model_name
            )
        } else {
            tracing::info!(
                embedding_provider = embed_name,
                "embedding operations continue using provider '{embed_name}'"
            );
            format!(
                "Switched to provider: {} (model: {}). Embedding operations continue using \
                 provider '{}'.",
                configured_name, self.runtime.model_name, embed_name
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
        }
    }

    #[tokio::test]
    async fn provider_list_empty_pool() {
        let mut qa = QuickTestAgent::minimal("ok");
        qa.agent.handle_provider_command("/provider").await;
        let msgs = qa.sent_messages();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].contains("No providers configured"));
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
        agent.providers.provider_pool = vec![entry_a, entry_b];

        agent.handle_provider_command("/provider").await;
        let msgs = agent.channel.sent_messages();
        assert_eq!(msgs.len(), 1);
        let out = &msgs[0];
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
        agent.providers.provider_pool = vec![entry];
        agent.providers.provider_config_snapshot = Some(snapshot);

        agent.handle_provider_command("/provider").await;
        let msgs = agent.channel.sent_messages();
        assert!(msgs[0].contains("(active)"), "active entry must be marked");
    }

    #[tokio::test]
    async fn provider_switch_unknown_name_returns_error() {
        let mut qa = QuickTestAgent::minimal("ok");
        let entry = make_entry("ollama", ProviderKind::Ollama, Some("qwen3:8b"));
        qa.agent.providers.provider_pool = vec![entry];
        qa.agent
            .handle_provider_command("/provider nonexistent")
            .await;
        let msgs = qa.sent_messages();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].contains("Unknown provider 'nonexistent'"));
        assert!(msgs[0].contains("ollama"));
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
        agent.providers.provider_pool = vec![entry];
        agent.providers.provider_config_snapshot = Some(snapshot);

        agent.handle_provider_command("/provider ollama").await;
        let msgs = agent.channel.sent_messages();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].contains("already active"));
    }

    #[tokio::test]
    async fn provider_switch_missing_snapshot_returns_error() {
        let mut qa = QuickTestAgent::minimal("ok");
        let entry = make_entry("ollama", ProviderKind::Ollama, Some("qwen3:8b"));
        qa.agent.providers.provider_pool = vec![entry];
        // provider_config_snapshot is None by default
        qa.agent.handle_provider_command("/provider ollama").await;
        let msgs = qa.sent_messages();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].contains("config snapshot missing"));
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
        agent.providers.provider_pool = vec![entry_a, entry_b];
        agent.providers.provider_config_snapshot = Some(snapshot);
        agent.providers.cached_prompt_tokens = 999;

        agent.handle_provider_command("/provider ollama2").await;
        let msgs = agent.channel.sent_messages();
        assert_eq!(msgs.len(), 1, "should send success message");
        assert!(
            msgs[0].contains("Switched to provider:"),
            "unexpected: {}",
            msgs[0]
        );
        assert!(msgs[0].contains("llama3.2"));
        assert_eq!(
            agent.providers.cached_prompt_tokens, 0,
            "must be reset on switch"
        );
        assert_eq!(agent.runtime.model_name, "llama3.2");
    }

    #[tokio::test]
    async fn provider_status_no_metrics() {
        let mut qa = QuickTestAgent::minimal("ok");
        qa.agent.runtime.model_name = "test-model".to_owned();
        qa.agent.handle_provider_command("/provider status").await;
        let msgs = qa.sent_messages();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].contains("Current provider:"));
        assert!(msgs[0].contains("test-model"));
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
        };
        assert_eq!(snap.claude_api_key.as_deref(), Some("key-claude"));
        assert_eq!(snap.openai_api_key.as_deref(), Some("key-openai"));
        assert!(snap.gemini_api_key.is_none());
        assert_eq!(snap.llm_request_timeout_secs, 60);
    }

    // Verify that build_switch_message omits the embedding notice when the embedding provider
    // name matches the new active provider name.
    #[tokio::test]
    async fn build_switch_message_no_notice_when_same_provider() {
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
        agent.runtime.active_provider_name = "mock2".to_owned();
        agent.providers.provider_pool = vec![entry_a, entry_b];
        agent.providers.provider_config_snapshot = Some(snapshot);

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
        agent.providers.provider_pool = vec![entry_a, entry_b];
        agent.providers.provider_config_snapshot = Some(snapshot);

        agent.handle_provider_command("/provider ollama2").await;
        let msgs = agent.channel.sent_messages();
        assert_eq!(msgs.len(), 1);
        // embedding_provider.name() == "mock" ≠ "ollama" (the new chat provider) → notice shown.
        assert!(
            msgs[0].contains("Embedding operations continue using"),
            "embedding notice expected when providers differ: {}",
            msgs[0]
        );
        assert!(
            msgs[0].contains("mock"),
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
        agent.providers.provider_pool = vec![entry_a, entry_b];
        agent.providers.provider_config_snapshot = Some(snapshot);

        let embed_name_before = agent.embedding_provider.name().to_owned();

        agent.handle_provider_command("/provider ollama2").await;

        // Chat provider must have changed.
        assert_eq!(agent.runtime.model_name, "llama3.2");
        // Embedding provider must remain untouched.
        assert_eq!(
            agent.embedding_provider.name(),
            embed_name_before,
            "embedding_provider must not change after /provider switch"
        );
    }
}
