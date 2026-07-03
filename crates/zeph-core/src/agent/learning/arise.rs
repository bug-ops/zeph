// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::super::{Agent, Channel, Role};
use super::background::AriseTaskArgs;
use zeph_llm::provider::MessagePart;

/// Outcome of [`Agent::resolve_pool_entry_provider`]. See that method's doc comment for the
/// rationale behind each variant.
pub(crate) enum PoolProviderResolution {
    /// The provider-pool registry was never wired for this `Agent` (empty pool, no config
    /// snapshot) or `provider_name` was empty — the caller may fall back to
    /// [`Agent::resolve_background_provider`]'s existing convention.
    RegistryNotWired,
    /// The registry IS wired, but `provider_name` does not resolve to a usable provider
    /// (absent from `provider_pool`, no config snapshot, or construction failed).
    Unresolvable,
    /// `provider_name` resolved to a distinct, usable provider.
    Resolved(Box<zeph_llm::any::AnyProvider>),
}

impl<C: Channel> Agent<C> {
    /// Resolve a named provider from the pool, falling back to the primary provider.
    /// Returns a clone of the primary provider if the name is empty, unknown, or resolution fails.
    pub(crate) fn resolve_background_provider(
        &self,
        provider_name: &str,
    ) -> zeph_llm::any::AnyProvider {
        if provider_name.is_empty() {
            return self.provider.clone();
        }
        let Some(entry) = self
            .runtime
            .providers
            .provider_pool
            .iter()
            .find(|e| e.effective_name().eq_ignore_ascii_case(provider_name))
            .cloned()
        else {
            tracing::warn!(
                provider = provider_name,
                "provider not found in [[llm.providers]], falling back to primary"
            );
            return self.provider.clone();
        };
        let Some(ref snapshot) = self.runtime.providers.provider_config_snapshot else {
            return self.provider.clone();
        };
        match crate::provider_factory::build_provider_for_switch(
            &entry,
            snapshot,
            self.services.security.secret_registry.as_ref(),
        ) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("failed to build provider '{provider_name}': {e:#}, using primary");
                self.provider.clone()
            }
        }
    }

    /// Attempt to build the provider registered under `provider_name` in `provider_pool`,
    /// without ever substituting the primary provider for a real misconfiguration.
    ///
    /// Distinguishes three outcomes a caller like `reformat_tool_call` (#5600, follow-up to
    /// #5478) must not conflate:
    ///
    /// - [`PoolProviderResolution::RegistryNotWired`]: `provider_name` is empty, or
    ///   `provider_pool`/`provider_config_snapshot` were never populated for this `Agent` at
    ///   all. `zeph_config::providers::validate_pool` rejects an empty `[[llm.providers]]` list
    ///   at config-validation time, so a genuinely empty pool never occurs for a fully
    ///   constructed production `Agent` (#5450 populates it on every construction path) — this
    ///   only happens for lightweight test/bootstrap agents that skip provider-pool wiring
    ///   entirely. Falling back to [`Agent::resolve_background_provider`]'s existing convention
    ///   is safe here since there is no registry to have misconfigured against.
    /// - [`PoolProviderResolution::Unresolvable`]: the registry IS wired (non-empty pool) but
    ///   `provider_name` does not match any entry, or the matched entry fails to build, or no
    ///   `provider_config_snapshot` is available. This is a real misconfiguration: silently
    ///   substituting the primary provider would mask the original error behind a "corrected"
    ///   call made with the wrong model.
    /// - [`PoolProviderResolution::Resolved`]: the name matched and the provider built
    ///   successfully.
    pub(crate) fn resolve_pool_entry_provider(
        &self,
        provider_name: &str,
    ) -> PoolProviderResolution {
        if provider_name.is_empty() {
            return PoolProviderResolution::RegistryNotWired;
        }
        let registry_wired = !self.runtime.providers.provider_pool.is_empty()
            || self.runtime.providers.provider_config_snapshot.is_some();
        if !registry_wired {
            return PoolProviderResolution::RegistryNotWired;
        }
        let Some(entry) = self
            .runtime
            .providers
            .provider_pool
            .iter()
            .find(|e| e.effective_name().eq_ignore_ascii_case(provider_name))
            .cloned()
        else {
            return PoolProviderResolution::Unresolvable;
        };
        let Some(ref snapshot) = self.runtime.providers.provider_config_snapshot else {
            return PoolProviderResolution::Unresolvable;
        };
        match crate::provider_factory::build_provider_for_switch(
            &entry,
            snapshot,
            self.services.security.secret_registry.as_ref(),
        ) {
            Ok(p) => PoolProviderResolution::Resolved(Box::new(p)),
            Err(e) => {
                tracing::warn!("failed to build provider '{provider_name}': {e:#}");
                PoolProviderResolution::Unresolvable
            }
        }
    }

    /// Extract tool names used in the most recent assistant turn from message history.
    ///
    /// Scans messages in reverse until the previous user message boundary, collecting
    /// all `ToolUse` part names. Returns an empty vec when no tool calls are found.
    pub(crate) fn extract_last_turn_tool_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        let mut past_assistant = false;
        for msg in self.msg.messages.iter().rev() {
            match msg.role {
                Role::Assistant => {
                    past_assistant = true;
                    for part in &msg.parts {
                        if let MessagePart::ToolUse { name, .. } = part {
                            names.push(name.clone());
                        }
                    }
                }
                Role::User if past_assistant => {
                    // Stop at the user message that preceded the assistant turn.
                    break;
                }
                _ => {}
            }
        }
        names.reverse();
        names
    }

    /// Fire-and-forget ARISE trace improvement after a successful multi-tool turn.
    ///
    /// All three features (ARISE, STEM, ERL) MUST be background tasks — never awaited inline.
    pub(crate) fn spawn_arise_trace_improvement(&mut self, skill_name: &str) {
        let Some(config) = self.services.learning_engine.config.as_ref() else {
            return;
        };
        if !config.arise_enabled {
            return;
        }
        let tool_names = self.extract_last_turn_tool_names();
        if tool_names.len() < config.arise_min_tool_calls as usize {
            return;
        }
        let Some(memory) = self.services.memory.persistence.memory.clone() else {
            return;
        };
        let Ok(skill) = self.services.skill.registry.read().skill(skill_name) else {
            return;
        };
        let status_tx = self.services.session.status_tx.clone();
        if let Some(ref tx) = self.services.session.status_tx {
            let _ = tx.send(format!("Evolving skill: {skill_name}…"));
        }
        let args = AriseTaskArgs {
            provider: self.resolve_background_provider(config.arise_trace_provider.as_str()),
            memory,
            skill_name: skill_name.to_string(),
            skill_body: skill.body.clone(),
            skill_desc: skill.description().to_string(),
            trace: tool_names.join(" \u{2192} "),
            max_auto_sections: config.max_auto_sections,
            skill_paths: self.services.skill.skill_paths.clone(),
            auto_activate: config.auto_activate,
            max_versions: config.max_versions,
            domain_success_gate: config.domain_success_gate,
            status_tx,
        };
        self.try_spawn_learning_task(super::background::arise_trace_task(args));
    }
}
