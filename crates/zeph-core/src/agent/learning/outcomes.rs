// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::super::{Agent, Channel, LlmProvider};
use super::super::{Message, Role, SemanticMemory};
use super::background::write_skill_file;
use crate::config::LearningConfig;
use zeph_llm::provider::MessageMetadata;

impl<C: Channel> Agent<C> {
    pub(crate) async fn attempt_self_reflection(
        &mut self,
        error_context: &str,
        tool_output: &str,
    ) -> Result<bool, super::super::error::AgentError> {
        if self.learning_engine.was_reflection_used() || !self.is_learning_enabled() {
            return Ok(false);
        }
        self.learning_engine.mark_reflection_used();

        let skill_name = self.skill_state.active_skill_names.first().cloned();

        let Some(name) = skill_name else {
            return Ok(false);
        };

        if !self.is_skill_trusted_for_learning(&name).await {
            return Ok(false);
        }

        let Ok(skill) = self.skill_state.registry.read().get_skill(&name) else {
            return Ok(false);
        };

        let mut prompt = zeph_skills::evolution::build_reflection_prompt(
            skill.name(),
            &skill.body,
            error_context,
            tool_output,
        );

        // D2Skill: inject matching step corrections before reflection.
        // Pass empty tool_name — the call site has error text but not a specific tool name.
        // The SQL query uses `AND (tool_name = '' OR tool_name = ?)` so this matches
        // corrections that apply to any tool.
        let correction_hints = self
            .build_step_correction_hints(&name, error_context, "")
            .await;
        if !correction_hints.is_empty() {
            prompt.push_str("\n\nKnown corrections:\n");
            for (_, hint) in &correction_hints {
                prompt.push_str("- ");
                prompt.push_str(hint);
                prompt.push('\n');
            }
        }

        self.push_message(Message {
            role: Role::User,
            content: prompt,
            parts: vec![],
            metadata: MessageMetadata::default(),
        });

        let messages_before = self.msg.messages.len();
        let _ = self.channel.send_status("reflecting...").await;
        // Box::pin to break async recursion cycle (process_response -> attempt_self_reflection -> process_response)
        if let Err(e) = Box::pin(self.process_response()).await {
            let _ = self.channel.send_status("").await;
            return Err(e);
        }
        let _ = self.channel.send_status("").await;
        let retry_succeeded = self.msg.messages.len() > messages_before;

        // D2Skill: record whether injected corrections led to success.
        if !correction_hints.is_empty() {
            let ids: Vec<i64> = correction_hints.iter().map(|(id, _)| *id).collect();
            self.record_correction_usages(&ids, retry_succeeded).await;
        }

        if retry_succeeded {
            let successful_response = self
                .msg
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Assistant)
                .map(|m| m.content.clone())
                .unwrap_or_default();

            // D2Skill: extract correction from this failure→success pair.
            self.spawn_d2skill_correction_extraction(
                &name,
                error_context,
                tool_output,
                &successful_response,
            );

            self.generate_improved_skill(&name, error_context, &successful_response, None)
                .await
                .ok();
        }

        Ok(retry_succeeded)
    }

    #[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
    pub(crate) async fn generate_improved_skill(
        &mut self,
        skill_name: &str,
        error_context: &str,
        successful_response: &str,
        user_feedback: Option<&str>,
    ) -> Result<(), super::super::error::AgentError> {
        if !self.is_learning_enabled() {
            return Ok(());
        }
        if !self.is_skill_trusted_for_learning(skill_name).await {
            return Ok(());
        }

        // Clone Arc before any .await so no &self fields are held across suspension points.
        let memory = self.memory_state.persistence.memory.clone();
        let Some(memory) = memory else {
            return Ok(());
        };
        let config = self.learning_engine.config.clone();
        let Some(config) = config else {
            return Ok(());
        };

        let skill = self.skill_state.registry.read().get_skill(skill_name)?;

        memory
            .sqlite()
            .ensure_skill_version_exists(skill_name, &skill.body, skill.description())
            .await?;

        if !self
            .check_improvement_allowed(&memory, &config, skill_name, user_feedback)
            .await?
        {
            return Ok(());
        }

        // Structured evaluation: ask LLM whether improvement is actually needed.
        if user_feedback.is_none() {
            let metrics_row = memory.sqlite().skill_metrics(skill_name).await?;
            if let Some(row) = metrics_row {
                let metrics = zeph_skills::evolution::SkillMetrics {
                    skill_name: row.skill_name.clone(),
                    version: row.version_id.unwrap_or(0),
                    total: row.total,
                    successes: row.successes,
                    failures: row.failures,
                };
                let eval_prompt = zeph_skills::evolution::build_evaluation_prompt(
                    skill_name,
                    &skill.body,
                    error_context,
                    successful_response,
                    &metrics,
                );
                let eval_messages = vec![Message {
                    role: Role::User,
                    content: eval_prompt,
                    parts: vec![],
                    metadata: MessageMetadata::default(),
                }];
                match self
                    .provider
                    .chat_typed_erased::<zeph_skills::evolution::SkillEvaluation>(&eval_messages)
                    .await
                {
                    Ok(eval) if !eval.should_improve => {
                        tracing::info!(
                            skill = %skill_name,
                            issues = ?eval.issues,
                            "evaluation: skip improvement"
                        );
                        return Ok(());
                    }
                    Ok(eval) => {
                        tracing::info!(
                            skill = %skill_name,
                            severity = %eval.severity,
                            "evaluation: proceed with improvement"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            "skill evaluation failed, proceeding with improvement: {e:#}"
                        );
                    }
                }
            }
        }

        let generated_body = self
            .call_improvement_llm(
                skill_name,
                &skill.body,
                error_context,
                successful_response,
                user_feedback,
            )
            .await?;
        let generated_body = generated_body.trim();

        if generated_body.is_empty()
            || !zeph_skills::evolution::validate_body_size(&skill.body, generated_body)
        {
            tracing::warn!("improvement for {skill_name} rejected (empty or too large)");
            return Ok(());
        }

        if !zeph_skills::evolution::validate_body_sections(generated_body, config.max_auto_sections)
        {
            tracing::warn!(
                "improvement for {skill_name} rejected (exceeds {} sections)",
                config.max_auto_sections
            );
            return Ok(());
        }

        self.store_improved_version(
            &memory,
            &config,
            skill_name,
            generated_body,
            skill.description(),
            error_context,
        )
        .await
    }

    #[allow(clippy::cast_precision_loss)]
    pub(crate) async fn check_improvement_allowed(
        &self,
        memory: &SemanticMemory,
        config: &LearningConfig,
        skill_name: &str,
        user_feedback: Option<&str>,
    ) -> Result<bool, super::super::error::AgentError> {
        if let Some(last_time) = memory.sqlite().last_improvement_time(skill_name).await?
            && let Ok(last) = super::background::chrono_parse_sqlite(&last_time)
        {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let elapsed_minutes = (now.saturating_sub(last)) / 60;
            if elapsed_minutes < config.cooldown_minutes {
                tracing::debug!(
                    "cooldown active for {skill_name}: {elapsed_minutes}m < {}m",
                    config.cooldown_minutes
                );
                return Ok(false);
            }
        }

        if user_feedback.is_none()
            && let Some(metrics) = memory.sqlite().skill_metrics(skill_name).await?
        {
            if metrics.failures < i64::from(config.min_failures) {
                return Ok(false);
            }
            let rate = if metrics.total == 0 {
                1.0
            } else {
                metrics.successes as f64 / metrics.total as f64
            };
            if rate >= config.improve_threshold {
                return Ok(false);
            }
        }

        Ok(true)
    }

    async fn call_improvement_llm(
        &self,
        skill_name: &str,
        original_body: &str,
        error_context: &str,
        successful_response: &str,
        user_feedback: Option<&str>,
    ) -> Result<String, super::super::error::AgentError> {
        let prompt = zeph_skills::evolution::build_improvement_prompt(
            skill_name,
            original_body,
            error_context,
            successful_response,
            user_feedback,
        );

        let messages = vec![
            Message {
                role: Role::System,
                content:
                    "You are a skill improvement assistant. Output only the improved skill body."
                        .into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message {
                role: Role::User,
                content: prompt,
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
        ];

        self.provider.chat(&messages).await.map_err(Into::into)
    }

    async fn store_improved_version(
        &self,
        memory: &SemanticMemory,
        config: &LearningConfig,
        skill_name: &str,
        generated_body: &str,
        description: &str,
        error_context: &str,
    ) -> Result<(), super::super::error::AgentError> {
        let active = memory.sqlite().active_skill_version(skill_name).await?;
        let predecessor_id = active.as_ref().map(|v| v.id);

        let next_ver = memory.sqlite().next_skill_version(skill_name).await?;
        let version_id = memory
            .sqlite()
            .save_skill_version(
                skill_name,
                next_ver,
                generated_body,
                description,
                "auto",
                Some(error_context),
                predecessor_id,
            )
            .await?;

        tracing::info!("generated v{next_ver} for skill {skill_name} (id={version_id})");

        if config.domain_success_gate {
            let gate_prompt = zeph_skills::evolution::build_domain_gate_prompt(
                skill_name,
                description,
                generated_body,
            );
            let gate_messages = vec![Message {
                role: Role::User,
                content: gate_prompt,
                parts: vec![],
                metadata: MessageMetadata::default(),
            }];
            match self
                .provider
                .chat_typed_erased::<zeph_skills::evolution::DomainGateResult>(&gate_messages)
                .await
            {
                Ok(gate) if !gate.domain_relevant => {
                    tracing::warn!(
                        skill = skill_name,
                        reasoning = %gate.reasoning,
                        "domain gate: generated skill drifts from original domain, skipping activation"
                    );
                    // Version is saved but not activated; return early.
                    return Ok(());
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        "domain gate check failed for {skill_name}, proceeding with activation: {e:#}"
                    );
                }
            }
        }

        if config.auto_activate {
            memory
                .sqlite()
                .activate_skill_version(skill_name, version_id)
                .await?;
            write_skill_file(
                &self.skill_state.skill_paths,
                skill_name,
                description,
                generated_body,
            )
            .await?;
            tracing::info!("auto-activated v{next_ver} for {skill_name}");
        }

        memory
            .sqlite()
            .prune_skill_versions(skill_name, config.max_versions)
            .await?;

        Ok(())
    }

    #[allow(clippy::cast_precision_loss)]
    /// Check rollback eligibility for all skills that have an active auto-generated version.
    /// Called once per turn before processing the user message so that accumulated outcome
    /// data can trigger rollback even when no new tool executions occur in the current turn.
    pub(crate) async fn check_pending_rollbacks(&self) {
        if !self.is_learning_enabled() {
            return;
        }
        let Some(memory) = &self.memory_state.persistence.memory else {
            return;
        };
        let Ok(versions) = memory.sqlite().list_active_auto_versions().await else {
            return;
        };
        for skill_name in versions {
            self.check_rollback(&skill_name).await;
        }
    }

    pub(crate) async fn check_rollback(&self, skill_name: &str) {
        if let Err(_elapsed) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            self.check_rollback_inner(skill_name),
        )
        .await
        {
            tracing::warn!(skill = skill_name, "check_rollback timed out after 2s");
        }
    }

    #[allow(clippy::cast_precision_loss)]
    async fn check_rollback_inner(&self, skill_name: &str) {
        if !self.is_learning_enabled() {
            return;
        }
        let Some(memory) = &self.memory_state.persistence.memory else {
            return;
        };
        let Some(config) = &self.learning_engine.config else {
            return;
        };
        let Ok(Some(metrics)) = memory.sqlite().skill_metrics(skill_name).await else {
            return;
        };

        if metrics.total < i64::from(config.min_evaluations) {
            return;
        }

        let rate = if metrics.total == 0 {
            1.0
        } else {
            metrics.successes as f64 / metrics.total as f64
        };

        if rate >= config.rollback_threshold {
            return;
        }

        let Ok(Some(active)) = memory.sqlite().active_skill_version(skill_name).await else {
            return;
        };
        if active.source != "auto" {
            return;
        }
        let Ok(Some(predecessor)) = memory.sqlite().predecessor_version(active.id).await else {
            return;
        };

        tracing::warn!(
            "rolling back {skill_name} from v{} to v{} (rate: {rate:.0}%)",
            active.version,
            predecessor.version,
        );

        if memory
            .sqlite()
            .activate_skill_version(skill_name, predecessor.id)
            .await
            .is_ok()
        {
            write_skill_file(
                &self.skill_state.skill_paths,
                skill_name,
                &predecessor.description,
                &predecessor.body,
            )
            .await
            .ok();
        }
    }
}
