// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::super::{Agent, Channel};
use super::background::{ErlTaskArgs, erl_reflection_task};

impl<C: Channel> Agent<C> {
    /// Build a heuristics suffix to append to the skills prompt.
    ///
    /// Queries `skill_heuristics` for each active skill and returns a formatted section.
    /// Returns an empty string when ERL is disabled, memory is unavailable, or no heuristics exist.
    pub(crate) async fn build_erl_heuristics_prompt(&self) -> String {
        let Some(config) = self.learning_engine.config.as_ref() else {
            return String::new();
        };
        if !config.erl_enabled {
            return String::new();
        }
        let Some(memory) = &self.memory_state.memory else {
            return String::new();
        };

        let mut sections = String::new();
        for skill_name in &self.skill_state.active_skill_names {
            let heuristics = match memory
                .sqlite()
                .load_skill_heuristics(
                    skill_name,
                    config.erl_min_confidence,
                    config.erl_max_heuristics_per_skill,
                )
                .await
            {
                Ok(rows) => rows,
                Err(e) => {
                    tracing::warn!("ERL: load_skill_heuristics failed for {skill_name}: {e:#}");
                    continue;
                }
            };

            if heuristics.is_empty() {
                continue;
            }

            let texts: Vec<String> = heuristics.into_iter().map(|(_, text, _, _)| text).collect();
            let section = zeph_skills::erl::format_heuristics_section(&texts);
            if !section.is_empty() {
                sections.push_str("\n\n<!-- ERL heuristics for skill: ");
                sections.push_str(skill_name);
                sections.push_str(" -->\n");
                sections.push_str(&section);
            }
        }
        sections
    }

    /// Fire-and-forget ERL heuristic extraction after a successful skill+tool turn.
    pub(crate) fn spawn_erl_reflection(&mut self, skill_name: &str) {
        let Some(config) = self.learning_engine.config.as_ref() else {
            return;
        };
        if !config.erl_enabled {
            return;
        }
        let tool_names = self.extract_last_turn_tool_names();
        if tool_names.is_empty() {
            return;
        }
        let Some(memory) = self.memory_state.memory.clone() else {
            return;
        };
        let task_summary = self
            .msg
            .messages
            .iter()
            .rev()
            .find(|m| m.role == super::super::Role::User)
            .map(|m| m.content.chars().take(512).collect::<String>())
            .unwrap_or_default();
        let status_tx = self.session.status_tx.clone();
        if let Some(ref tx) = self.session.status_tx {
            let _ = tx.send("Reflecting on experience…".to_string());
        }
        let args = ErlTaskArgs {
            provider: self.resolve_background_provider(config.erl_extract_provider.as_str()),
            memory,
            skill_name: skill_name.to_string(),
            task_summary,
            tool_calls_str: tool_names.join(", "),
            dedup_threshold: config.erl_dedup_threshold,
            status_tx,
        };
        self.try_spawn_learning_task(erl_reflection_task(args));
    }
}
