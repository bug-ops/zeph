// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::super::{Agent, Channel, Role};
use super::background::AriseTaskArgs;
use zeph_llm::provider::MessagePart;

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
        let Some(ref snapshot) = self.providers.provider_config_snapshot else {
            return self.provider.clone();
        };
        match crate::provider_factory::build_provider_for_switch(&entry, snapshot) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("failed to build provider '{provider_name}': {e:#}, using primary");
                self.provider.clone()
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
        let Some(config) = self.learning_engine.config.as_ref() else {
            return;
        };
        if !config.arise_enabled {
            return;
        }
        let tool_names = self.extract_last_turn_tool_names();
        if tool_names.len() < config.arise_min_tool_calls as usize {
            return;
        }
        let Some(memory) = self.memory_state.persistence.memory.clone() else {
            return;
        };
        let Ok(skill) = self.skill_state.registry.read().get_skill(skill_name) else {
            return;
        };
        let status_tx = self.session.status_tx.clone();
        if let Some(ref tx) = self.session.status_tx {
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
            skill_paths: self.skill_state.skill_paths.clone(),
            auto_activate: config.auto_activate,
            max_versions: config.max_versions,
            domain_success_gate: config.domain_success_gate,
            status_tx,
        };
        self.try_spawn_learning_task(super::background::arise_trace_task(args));
    }
}
