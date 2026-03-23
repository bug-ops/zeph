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
        let current = self.provider.name().to_owned();
        let mut lines = vec!["Configured providers:".to_string()];
        for (i, entry) in pool.iter().enumerate() {
            let name = entry.effective_name();
            let model = entry.model.as_deref().unwrap_or("(default)");
            let marker = if name == current { " (active)" } else { "" };
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
        let _ = writeln!(out, "Name:  {}", self.provider.name());
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
        if self.provider.name().eq_ignore_ascii_case(name) {
            let _ = self
                .channel
                .send(&format!(
                    "Provider '{}' is already active.",
                    self.provider.name()
                ))
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

        match crate::bootstrap::build_provider_for_switch(&entry, snapshot) {
            Ok(new_provider) => {
                // Resolve actual model name: provider knows its default, entry overrides it.
                let model_name = entry
                    .model
                    .clone()
                    .unwrap_or_else(|| new_provider.name().to_owned());

                self.provider = new_provider;
                self.runtime.model_name = model_name.clone();

                // Reset state that is provider-specific.
                self.providers.cached_prompt_tokens = 0;
                self.providers.server_compaction_active = entry.server_compaction;

                // C1: Reset extended context flag (Claude-specific feature).
                self.metrics.extended_context = entry.enable_extended_context;

                // C2: Log provider switch in metrics for cost-tracking boundary awareness.
                tracing::info!(
                    provider = self.provider.name(),
                    model = model_name,
                    "provider switched via /provider command"
                );

                // C3: Clear ACP provider override so the explicit switch takes effect.
                if let Some(ref override_slot) = self.providers.provider_override
                    && let Ok(mut slot) = override_slot.write()
                {
                    *slot = None;
                }

                // C5: Update instruction file list for the new provider's kind.
                self.update_provider_instructions(&entry);

                let _ = self
                    .channel
                    .send(&format!(
                        "Switched to provider: {} (model: {})",
                        self.provider.name(),
                        self.runtime.model_name
                    ))
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
}
