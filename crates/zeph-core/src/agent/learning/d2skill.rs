// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::super::{Agent, Channel};
use super::super::{Message, Role};
use super::background::StemTaskArgs;
use zeph_llm::provider::MessageMetadata;

impl<C: Channel> Agent<C> {
    /// Fire-and-forget `D2Skill` correction extraction.
    ///
    /// Called when a skill succeeds after a prior reflection (failure + success pair).
    /// Extracts a `StepCorrection` via LLM and stores it in `step_corrections`.
    pub(crate) fn spawn_d2skill_correction_extraction(
        &mut self,
        skill_name: &str,
        failure_error: &str,
        failure_tool: &str,
        successful_response: &str,
    ) {
        let Some(config) = self.learning_engine.config.as_ref() else {
            return;
        };
        if !config.d2skill_enabled {
            return;
        }
        let Some(memory) = self.memory_state.memory.clone() else {
            return;
        };
        let provider = self.resolve_background_provider(config.d2skill_provider.as_str());
        let skill_name = skill_name.to_string();
        let failure_error = failure_error.to_string();
        let failure_tool = failure_tool.to_string();
        let successful_response = successful_response.to_string();

        self.try_spawn_learning_task(async move {
            let prompt = zeph_skills::evolution::build_correction_extraction_prompt(
                &skill_name,
                &failure_error,
                &failure_tool,
                &successful_response,
            );
            let messages = vec![Message {
                role: Role::User,
                content: prompt,
                parts: vec![],
                metadata: MessageMetadata::default(),
            }];
            let result = match provider
                .chat_typed_erased::<zeph_skills::evolution::CorrectionExtractionResult>(&messages)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!(skill = %skill_name, "d2skill: LLM extraction failed: {e:#}");
                    return;
                }
            };
            if result.hint.is_empty() {
                return;
            }
            if let Err(e) = memory
                .sqlite()
                .insert_step_correction(
                    &skill_name,
                    &result.failure_kind,
                    &result.error_substring,
                    &failure_tool,
                    &result.hint,
                )
                .await
            {
                tracing::debug!(skill = %skill_name, "d2skill: failed to store correction: {e:#}");
            }
        });
    }

    /// Fire-and-forget STEM tool usage log insert + pattern check.
    ///
    /// Called after every tool execution turn regardless of outcome.
    pub(crate) fn spawn_stem_detection(&mut self, outcome: &str) {
        let Some(config) = self.learning_engine.config.as_ref() else {
            return;
        };
        if !config.stem_enabled {
            return;
        }
        let tool_names = self.extract_last_turn_tool_names();
        if tool_names.is_empty() {
            return;
        }
        let Some(memory) = self.memory_state.memory.clone() else {
            return;
        };
        let tool_sequence = zeph_skills::stem::normalize_tool_sequence(
            &tool_names.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        let sequence_hash = zeph_skills::stem::sequence_hash(&tool_sequence);
        let user_msg = self
            .msg
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.content.clone())
            .unwrap_or_default();
        let ctx_bytes = blake3::hash(user_msg.chars().take(256).collect::<String>().as_bytes());
        let context_hash = ctx_bytes.to_hex()[..16].to_string();
        let outcome_owned = if outcome == "success" {
            "success"
        } else {
            "failure"
        }
        .to_string();
        let status_tx = self.session.status_tx.clone();
        if let Some(ref tx) = self.session.status_tx {
            let _ = tx.send("Learning from patterns…".to_string());
        }
        let args = StemTaskArgs {
            provider: self.resolve_background_provider(config.stem_provider.as_str()),
            memory,
            tool_sequence,
            sequence_hash,
            context_hash,
            outcome: outcome_owned,
            conv_id: self.memory_state.conversation_id,
            min_occurrences: config.stem_min_occurrences,
            min_success_rate: config.stem_min_success_rate,
            window_days: config.stem_pattern_window_days,
            retention_days: config.stem_retention_days,
            max_auto_sections: config.max_auto_sections,
            skill_paths: self.skill_state.skill_paths.clone(),
            status_tx,
        };
        self.try_spawn_learning_task(super::background::stem_detection_task(args));
    }

    /// Retrieve matching step corrections for the current tool failure.
    ///
    /// Returns `(correction_id, hint)` pairs for the first active skill.
    /// No-op when `d2skill_enabled = false` or memory is unavailable.
    pub(crate) async fn build_step_correction_hints(
        &self,
        skill_name: &str,
        error_context: &str,
        tool_name: &str,
    ) -> Vec<(i64, String)> {
        let Some(config) = self.learning_engine.config.as_ref() else {
            return vec![];
        };
        if !config.d2skill_enabled {
            return vec![];
        }
        let Some(memory) = &self.memory_state.memory else {
            return vec![];
        };
        let failure_kind = zeph_skills::evolution::FailureKind::from_error(error_context).as_str();
        match memory
            .sqlite()
            .find_step_corrections(
                skill_name,
                failure_kind,
                error_context,
                tool_name,
                config.d2skill_max_corrections,
            )
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                tracing::debug!("d2skill: failed to load corrections: {e:#}");
                vec![]
            }
        }
    }

    /// Record correction usage outcomes after a reflection attempt.
    ///
    /// `correction_ids`: IDs of corrections that were injected.
    /// `was_successful`: whether the subsequent reflection attempt succeeded.
    pub(crate) async fn record_correction_usages(
        &self,
        correction_ids: &[i64],
        was_successful: bool,
    ) {
        let Some(memory) = &self.memory_state.memory else {
            return;
        };
        for &id in correction_ids {
            if let Err(e) = memory
                .sqlite()
                .record_correction_usage(id, was_successful)
                .await
            {
                tracing::debug!("d2skill: failed to record correction usage {id}: {e:#}");
            }
        }
    }
}
