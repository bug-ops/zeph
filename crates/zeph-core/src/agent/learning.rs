// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::{Agent, Channel, LlmProvider};

use super::{Message, Role, SemanticMemory};
use crate::config::LearningConfig;
use zeph_llm::provider::MessageMetadata;

use std::path::PathBuf;

impl<C: Channel> Agent<C> {
    pub(super) fn is_learning_enabled(&self) -> bool {
        self.learning_engine.is_enabled()
    }

    async fn is_skill_trusted_for_learning(&self, skill_name: &str) -> bool {
        let Some(memory) = &self.memory_state.memory else {
            return true;
        };
        let Ok(Some(row)) = memory.sqlite().load_skill_trust(skill_name).await else {
            return true; // no trust record = local skill = trusted
        };
        matches!(row.trust_level.as_str(), "trusted" | "verified")
    }

    pub(super) async fn record_skill_outcomes(
        &self,
        outcome: &str,
        error_context: Option<&str>,
        outcome_detail: Option<&str>,
    ) {
        if self.skill_state.active_skill_names.is_empty() {
            return;
        }
        let Some(memory) = &self.memory_state.memory else {
            return;
        };
        if let Err(e) = memory
            .sqlite()
            .record_skill_outcomes_batch(
                &self.skill_state.active_skill_names,
                self.memory_state.conversation_id,
                outcome,
                error_context,
                outcome_detail,
            )
            .await
        {
            tracing::warn!("failed to record skill outcomes: {e:#}");
        }

        if outcome != "success" {
            for name in &self.skill_state.active_skill_names {
                self.check_rollback(name).await;
            }
        }

        let names: Vec<String> = self.skill_state.active_skill_names.clone();
        for name in &names {
            self.check_trust_transition(name).await;
        }
        self.update_skill_confidence_metrics().await;
    }

    pub(super) async fn update_skill_confidence_metrics(&self) {
        let Some(memory) = &self.memory_state.memory else {
            return;
        };
        let Ok(stats) = memory.sqlite().load_skill_outcome_stats().await else {
            return;
        };
        let confidences: Vec<crate::metrics::SkillConfidence> = stats
            .iter()
            .map(|s| {
                let suc = u32::try_from(s.successes).unwrap_or(0);
                let fail = u32::try_from(s.failures).unwrap_or(0);
                crate::metrics::SkillConfidence {
                    name: s.skill_name.clone(),
                    posterior: zeph_skills::trust_score::posterior_mean(suc, fail),
                    total_uses: u32::try_from(s.total).unwrap_or(0),
                }
            })
            .collect();
        self.update_metrics(|m| m.skill_confidence = confidences);
    }

    async fn check_trust_transition(&self, skill_name: &str) {
        let Some(memory) = &self.memory_state.memory else {
            return;
        };
        let Some(config) = &self.learning_engine.config else {
            return;
        };
        let Ok(Some(metrics)) = memory.sqlite().skill_metrics(skill_name).await else {
            return;
        };
        let successes = u32::try_from(metrics.successes).unwrap_or(0);
        let failures = u32::try_from(metrics.failures).unwrap_or(0);
        let total = u32::try_from(metrics.total).unwrap_or(0);
        let posterior = zeph_skills::trust_score::posterior_mean(successes, failures);

        if total >= config.auto_promote_min_uses && posterior > config.auto_promote_threshold {
            let trust_level = memory
                .sqlite()
                .load_skill_trust(skill_name)
                .await
                .ok()
                .flatten()
                .map(|r| r.trust_level);
            // Skip promotion only if explicitly blocked; promote even if no record exists.
            if trust_level.as_deref() != Some("trusted")
                && trust_level.as_deref() != Some("blocked")
            {
                tracing::info!(
                    skill = skill_name,
                    posterior = format!("{posterior:.3}"),
                    total,
                    "auto-promoting skill to trusted"
                );
                if trust_level.is_none() {
                    // No existing record — create one via upsert.
                    let _ = memory
                        .sqlite()
                        .upsert_skill_trust(
                            skill_name,
                            "trusted",
                            zeph_memory::sqlite::SourceKind::Local,
                            None,
                            None,
                            "",
                        )
                        .await;
                } else {
                    let _ = memory
                        .sqlite()
                        .set_skill_trust_level(skill_name, "trusted")
                        .await;
                }
            }
        }

        if total >= config.auto_demote_min_uses && posterior < config.auto_demote_threshold {
            let Ok(Some(trust_row)) = memory.sqlite().load_skill_trust(skill_name).await else {
                return;
            };
            if trust_row.trust_level == "trusted" || trust_row.trust_level == "verified" {
                tracing::warn!(
                    skill = skill_name,
                    posterior = format!("{posterior:.3}"),
                    total,
                    "auto-demoting skill to quarantined"
                );
                let _ = memory
                    .sqlite()
                    .set_skill_trust_level(skill_name, "quarantined")
                    .await;
            }
        }
    }

    pub(super) async fn attempt_self_reflection(
        &mut self,
        error_context: &str,
        tool_output: &str,
    ) -> Result<bool, super::error::AgentError> {
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

        let Ok(skill) = self
            .skill_state
            .registry
            .read()
            .expect("registry read lock")
            .get_skill(&name)
        else {
            return Ok(false);
        };

        let prompt = zeph_skills::evolution::build_reflection_prompt(
            skill.name(),
            &skill.body,
            error_context,
            tool_output,
        );

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

        if retry_succeeded {
            let successful_response = self
                .msg
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Assistant)
                .map(|m| m.content.clone())
                .unwrap_or_default();

            self.generate_improved_skill(&name, error_context, &successful_response, None)
                .await
                .ok();
        }

        Ok(retry_succeeded)
    }

    #[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
    pub(super) async fn generate_improved_skill(
        &self,
        skill_name: &str,
        error_context: &str,
        successful_response: &str,
        user_feedback: Option<&str>,
    ) -> Result<(), super::error::AgentError> {
        if !self.is_learning_enabled() {
            return Ok(());
        }
        if !self.is_skill_trusted_for_learning(skill_name).await {
            return Ok(());
        }

        let Some(memory) = &self.memory_state.memory else {
            return Ok(());
        };
        let Some(config) = self.learning_engine.config.as_ref() else {
            return Ok(());
        };

        let skill = self
            .skill_state
            .registry
            .read()
            .expect("registry read lock")
            .get_skill(skill_name)?;

        memory
            .sqlite()
            .ensure_skill_version_exists(skill_name, &skill.body, skill.description())
            .await?;

        if !self
            .check_improvement_allowed(memory, config, skill_name, user_feedback)
            .await?
        {
            return Ok(());
        }

        // Structured evaluation: ask LLM whether improvement is actually needed
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

        self.store_improved_version(
            memory,
            config,
            skill_name,
            generated_body,
            skill.description(),
            error_context,
        )
        .await
    }

    #[allow(clippy::cast_precision_loss)]
    async fn check_improvement_allowed(
        &self,
        memory: &SemanticMemory,
        config: &LearningConfig,
        skill_name: &str,
        user_feedback: Option<&str>,
    ) -> Result<bool, super::error::AgentError> {
        if let Some(last_time) = memory.sqlite().last_improvement_time(skill_name).await?
            && let Ok(last) = chrono_parse_sqlite(&last_time)
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
    ) -> Result<String, super::error::AgentError> {
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
    ) -> Result<(), super::error::AgentError> {
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
    pub(super) async fn check_pending_rollbacks(&self) {
        if !self.is_learning_enabled() {
            return;
        }
        let Some(memory) = &self.memory_state.memory else {
            return;
        };
        let Ok(versions) = memory.sqlite().list_active_auto_versions().await else {
            return;
        };
        for skill_name in versions {
            self.check_rollback(&skill_name).await;
        }
    }

    #[allow(clippy::cast_precision_loss)]
    async fn check_rollback(&self, skill_name: &str) {
        if !self.is_learning_enabled() {
            return;
        }
        let Some(memory) = &self.memory_state.memory else {
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

    pub(super) async fn handle_skill_command(
        &mut self,
        args: &str,
    ) -> Result<(), super::error::AgentError> {
        let parts: Vec<&str> = args.split_whitespace().collect();
        match parts.first().copied() {
            Some("stats") => self.handle_skill_stats().await,
            Some("versions") => self.handle_skill_versions(parts.get(1).copied()).await,
            Some("activate") => {
                self.handle_skill_activate(parts.get(1).copied(), parts.get(2).copied())
                    .await
            }
            Some("approve") => self.handle_skill_approve(parts.get(1).copied()).await,
            Some("reset") => self.handle_skill_reset(parts.get(1).copied()).await,
            Some("trust") => self.handle_skill_trust_command(&parts[1..]).await,
            Some("block") => self.handle_skill_block(parts.get(1).copied()).await,
            Some("unblock") => self.handle_skill_unblock(parts.get(1).copied()).await,
            Some("install") => self.handle_skill_install(parts.get(1).copied()).await,
            Some("remove") => self.handle_skill_remove(parts.get(1).copied()).await,
            Some("scan") => self.handle_skill_scan().await,
            Some("reject") => {
                let tail = if parts.len() > 2 { &parts[2..] } else { &[] };
                self.handle_skill_reject(parts.get(1).copied(), tail).await
            }
            _ => {
                self.channel
                    .send("Unknown /skill subcommand. Available: stats, versions, activate, approve, reset, trust, block, unblock, install, remove, reject, scan")
                    .await?;
                Ok(())
            }
        }
    }

    async fn handle_skill_reject(
        &mut self,
        name: Option<&str>,
        reason_parts: &[&str],
    ) -> Result<(), super::error::AgentError> {
        let Some(name) = name else {
            self.channel
                .send("Usage: /skill reject <name> <reason>")
                .await?;
            return Ok(());
        };
        // SEC-PH1-001: validate skill exists in registry before writing to DB
        if self
            .skill_state
            .registry
            .read()
            .expect("registry read lock")
            .get_skill(name)
            .is_err()
        {
            self.channel
                .send(&format!("Unknown skill: \"{name}\"."))
                .await?;
            return Ok(());
        }
        let reason = reason_parts.join(" ");
        if reason.is_empty() {
            self.channel
                .send("Usage: /skill reject <name> <reason>")
                .await?;
            return Ok(());
        }
        // SEC-PH1-002: cap reason length to prevent oversized LLM prompts
        let reason = if reason.len() > 500 {
            reason[..500].to_string()
        } else {
            reason
        };
        let Some(memory) = &self.memory_state.memory else {
            self.channel.send("Memory not available.").await?;
            return Ok(());
        };
        // REV-001: resolve active version_id for consistency with batch path
        let version_id = memory
            .sqlite()
            .active_skill_version(name)
            .await
            .ok()
            .flatten()
            .map(|v| v.id);
        memory
            .sqlite()
            .record_skill_outcome(
                name,
                version_id,
                self.memory_state.conversation_id,
                "user_rejection",
                Some(&reason),
                Some("user_rejection"), // REV-002: structured outcome_detail
            )
            .await?;
        if self.is_learning_enabled() {
            self.generate_improved_skill(name, &reason, "", Some(&reason))
                .await
                .ok();
        }
        self.channel
            .send(&format!("Rejection recorded for \"{name}\"."))
            .await?;
        Ok(())
    }

    async fn handle_skill_stats(&mut self) -> Result<(), super::error::AgentError> {
        use std::fmt::Write;

        let Some(memory) = &self.memory_state.memory else {
            self.channel.send("Memory not available.").await?;
            return Ok(());
        };

        let stats = memory.sqlite().load_skill_outcome_stats().await?;
        if stats.is_empty() {
            self.channel.send("No skill outcome data yet.").await?;
            return Ok(());
        }

        let mut output = String::from("Skill outcome statistics:\n\n");
        #[allow(clippy::cast_precision_loss)]
        for row in &stats {
            let rate = if row.total == 0 {
                0.0
            } else {
                row.successes as f64 / row.total as f64 * 100.0
            };
            let _ = writeln!(
                output,
                "- {}: {} total, {} ok, {} fail ({rate:.0}%)",
                row.skill_name, row.total, row.successes, row.failures,
            );
        }

        self.channel.send(&output).await?;
        Ok(())
    }

    async fn handle_skill_versions(
        &mut self,
        name: Option<&str>,
    ) -> Result<(), super::error::AgentError> {
        use std::fmt::Write;

        let Some(name) = name else {
            self.channel.send("Usage: /skill versions <name>").await?;
            return Ok(());
        };
        let Some(memory) = &self.memory_state.memory else {
            self.channel.send("Memory not available.").await?;
            return Ok(());
        };

        let versions = memory.sqlite().load_skill_versions(name).await?;
        if versions.is_empty() {
            self.channel
                .send(&format!("No versions found for \"{name}\"."))
                .await?;
            return Ok(());
        }

        let mut output = format!("Versions for \"{name}\":\n\n");
        for v in &versions {
            let active_tag = if v.is_active { ", active" } else { "" };
            let _ = writeln!(
                output,
                "  v{} ({}{active_tag}) — success: {}, failure: {}",
                v.version, v.source, v.success_count, v.failure_count,
            );
        }

        self.channel.send(&output).await?;
        Ok(())
    }

    async fn handle_skill_activate(
        &mut self,
        name: Option<&str>,
        version_str: Option<&str>,
    ) -> Result<(), super::error::AgentError> {
        let (Some(name), Some(ver_str)) = (name, version_str) else {
            self.channel
                .send("Usage: /skill activate <name> <version>")
                .await?;
            return Ok(());
        };
        let Ok(ver) = ver_str.parse::<i64>() else {
            self.channel.send("Invalid version number.").await?;
            return Ok(());
        };
        let Some(memory) = &self.memory_state.memory else {
            self.channel.send("Memory not available.").await?;
            return Ok(());
        };

        let versions = memory.sqlite().load_skill_versions(name).await?;
        let Some(target) = versions.iter().find(|v| v.version == ver) else {
            self.channel
                .send(&format!("Version {ver} not found for \"{name}\"."))
                .await?;
            return Ok(());
        };

        memory
            .sqlite()
            .activate_skill_version(name, target.id)
            .await?;

        write_skill_file(
            &self.skill_state.skill_paths,
            name,
            &target.description,
            &target.body,
        )
        .await?;

        self.channel
            .send(&format!("Activated v{ver} for \"{name}\"."))
            .await?;
        Ok(())
    }

    async fn handle_skill_approve(
        &mut self,
        name: Option<&str>,
    ) -> Result<(), super::error::AgentError> {
        let Some(name) = name else {
            self.channel.send("Usage: /skill approve <name>").await?;
            return Ok(());
        };
        let Some(memory) = &self.memory_state.memory else {
            self.channel.send("Memory not available.").await?;
            return Ok(());
        };

        let versions = memory.sqlite().load_skill_versions(name).await?;
        let pending = versions
            .iter()
            .rfind(|v| v.source == "auto" && !v.is_active);

        let Some(target) = pending else {
            self.channel
                .send(&format!("No pending auto version for \"{name}\"."))
                .await?;
            return Ok(());
        };

        memory
            .sqlite()
            .activate_skill_version(name, target.id)
            .await?;

        write_skill_file(
            &self.skill_state.skill_paths,
            name,
            &target.description,
            &target.body,
        )
        .await?;

        self.channel
            .send(&format!(
                "Approved and activated v{} for \"{name}\".",
                target.version
            ))
            .await?;
        Ok(())
    }

    async fn handle_skill_reset(
        &mut self,
        name: Option<&str>,
    ) -> Result<(), super::error::AgentError> {
        let Some(name) = name else {
            self.channel.send("Usage: /skill reset <name>").await?;
            return Ok(());
        };
        let Some(memory) = &self.memory_state.memory else {
            self.channel.send("Memory not available.").await?;
            return Ok(());
        };

        let versions = memory.sqlite().load_skill_versions(name).await?;
        let Some(v1) = versions.iter().find(|v| v.version == 1) else {
            self.channel
                .send(&format!("Original version not found for \"{name}\"."))
                .await?;
            return Ok(());
        };

        memory.sqlite().activate_skill_version(name, v1.id).await?;

        write_skill_file(
            &self.skill_state.skill_paths,
            name,
            &v1.description,
            &v1.body,
        )
        .await?;

        self.channel
            .send(&format!("Reset \"{name}\" to original v1."))
            .await?;
        Ok(())
    }
}

pub(super) async fn write_skill_file(
    skill_paths: &[PathBuf],
    skill_name: &str,
    description: &str,
    body: &str,
) -> Result<(), super::error::AgentError> {
    if skill_name.contains('/') || skill_name.contains('\\') || skill_name.contains("..") {
        return Err(super::error::AgentError::Other(format!(
            "invalid skill name: {skill_name}"
        )));
    }
    for base in skill_paths {
        let skill_dir = base.join(skill_name);
        let skill_file = skill_dir.join("SKILL.md");
        if skill_file.exists() {
            let content =
                format!("---\nname: {skill_name}\ndescription: {description}\n---\n{body}\n");
            tokio::fs::write(&skill_file, content).await?;
            return Ok(());
        }
    }
    Err(super::error::AgentError::Other(format!(
        "skill directory not found for {skill_name}"
    )))
}

// ── Preference inference ──────────────────────────────────────────────────────

/// Minimum evidence count required before a preference is emitted.
const MIN_EVIDENCE: i64 = 3;
/// Minimum confidence threshold for persisting a preference.
const PERSIST_THRESHOLD: f64 = 0.7;
/// Maximum number of new corrections to process per analysis run.
const CORRECTIONS_BATCH: u32 = 50;
/// Maximum number of preferences injected into the system prompt.
const MAX_INJECTED_PREFS: usize = 3;

/// A preference inferred from user corrections.
#[derive(Debug, PartialEq)]
pub(super) struct InferredPreference {
    pub key: String,
    pub value: String,
    pub confidence: f64,
    pub evidence_count: i64,
}

impl<C: Channel> Agent<C> {
    /// Run one preference analysis cycle.
    ///
    /// Loads corrections stored since the last watermark, infers preferences,
    /// and persists high-confidence ones to the `learned_preferences` table.
    /// The watermark (`last_analyzed_correction_id`) is advanced so the same
    /// corrections are never processed twice.
    pub(super) async fn analyze_and_learn(&mut self) {
        if !self.learning_engine.should_analyze() {
            return;
        }
        let Some(memory) = &self.memory_state.memory else {
            self.learning_engine.mark_analyzed();
            return;
        };
        let after_id = self.learning_engine.last_analyzed_correction_id;
        let corrections = match memory
            .sqlite()
            .load_corrections_after(after_id, CORRECTIONS_BATCH)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("learning engine: failed to load corrections: {e:#}");
                self.learning_engine.mark_analyzed();
                return;
            }
        };

        if corrections.is_empty() {
            self.learning_engine.mark_analyzed();
            return;
        }

        // Advance watermark to the highest id in this batch.
        if let Some(max_id) = corrections.iter().map(|r| r.id).max() {
            self.learning_engine.last_analyzed_correction_id = max_id;
        }

        let preferences = infer_preferences(&corrections);

        for pref in preferences
            .iter()
            .filter(|p| p.confidence >= PERSIST_THRESHOLD)
        {
            if let Err(e) = memory
                .sqlite()
                .upsert_learned_preference(
                    &pref.key,
                    &pref.value,
                    pref.confidence,
                    pref.evidence_count,
                )
                .await
            {
                tracing::warn!(key = %pref.key, "learning engine: failed to persist preference: {e:#}");
            }
        }

        if !preferences.is_empty() {
            tracing::info!(
                count = preferences.len(),
                watermark = self.learning_engine.last_analyzed_correction_id,
                "learning engine: analyzed corrections, persisted preferences"
            );
        }

        self.learning_engine.mark_analyzed();
    }

    /// Load high-confidence learned preferences and inject them into the
    /// system prompt after the `<!-- cache:volatile -->` marker.
    pub(super) async fn inject_learned_preferences(&self, prompt: &mut String) {
        let Some(memory) = &self.memory_state.memory else {
            return;
        };
        let prefs = match memory.sqlite().load_learned_preferences().await {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!("learning engine: failed to load preferences for injection: {e:#}");
                return;
            }
        };

        let high_confidence: Vec<_> = prefs
            .into_iter()
            .filter(|p| p.confidence >= PERSIST_THRESHOLD)
            // TODO(skill-affinity): implement when skill_outcomes tracking is wired
            .take(MAX_INJECTED_PREFS)
            .collect();

        if high_confidence.is_empty() {
            return;
        }

        prompt.push_str("\n\n## Learned User Preferences\n");
        for pref in &high_confidence {
            // Sanitize value to prevent prompt injection via embedded newlines.
            let sanitized_value = pref.preference_value.replace(['\n', '\r'], " ");
            prompt.push_str("- ");
            prompt.push_str(&pref.preference_key);
            prompt.push_str(": ");
            prompt.push_str(&sanitized_value);
            prompt.push('\n');
        }
    }
}

use std::sync::OnceLock;

static CONCISE_RE: OnceLock<regex::Regex> = OnceLock::new();
static VERBOSE_RE: OnceLock<regex::Regex> = OnceLock::new();
static BULLET_RE: OnceLock<regex::Regex> = OnceLock::new();
static NO_MD_RE: OnceLock<regex::Regex> = OnceLock::new();
static HEADERS_RE: OnceLock<regex::Regex> = OnceLock::new();
static CODE_ONLY_RE: OnceLock<regex::Regex> = OnceLock::new();
static LANG_RE: OnceLock<regex::Regex> = OnceLock::new();

fn correction_weight(kind: &str) -> i64 {
    if kind == "alternative_request" { 2 } else { 1 }
}

struct EvidenceCounts {
    concise: i64,
    verbose: i64,
    bullet: i64,
    no_md: i64,
    headers: i64,
    code_only: i64,
    lang: std::collections::HashMap<String, i64>,
}

fn count_evidence(
    corrections: &[zeph_memory::sqlite::corrections::UserCorrectionRow],
) -> EvidenceCounts {
    let concise_re = CONCISE_RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)\b(too\s+long|too\s+verbose|be\s+concise|be\s+brief|shorter\s+response|more\s+concise|less\s+verbose|tldr|tl;dr)\b",
        )
        .expect("static regex")
    });
    let verbose_re = VERBOSE_RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)\b(more\s+detail|explain\s+more|elaborate|expand\s+on|give\s+more\s+context)\b",
        )
        .expect("static regex")
    });
    let bullet_re = BULLET_RE.get_or_init(|| {
        regex::Regex::new(r"(?i)\b(use\s+bullet\s+points?|bullet\s+list|as\s+a\s+list)\b")
            .expect("static regex")
    });
    let no_md_re = NO_MD_RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)\b(no\s+markdown|plain\s+text|without\s+markdown|remove\s+formatting)\b",
        )
        .expect("static regex")
    });
    let headers_re = HEADERS_RE.get_or_init(|| {
        regex::Regex::new(r"(?i)\b(use\s+headers?|add\s+headers?|with\s+headers?)\b")
            .expect("static regex")
    });
    let code_only_re = CODE_ONLY_RE.get_or_init(|| {
        regex::Regex::new(r"(?i)\b(code\s+only|just\s+the\s+code|only\s+code|no\s+explanation)\b")
            .expect("static regex")
    });
    let lang_re = LANG_RE.get_or_init(|| {
        regex::Regex::new(r"(?i)\b(respond|answer|reply|write|speak)\s+in\s+([a-z]+)\b")
            .expect("static regex")
    });

    let mut counts = EvidenceCounts {
        concise: 0,
        verbose: 0,
        bullet: 0,
        no_md: 0,
        headers: 0,
        code_only: 0,
        lang: std::collections::HashMap::new(),
    };

    for row in corrections {
        if row.correction_kind == "self_correction" {
            continue;
        }
        let text = &row.correction_text;
        let w = correction_weight(&row.correction_kind);

        if concise_re.is_match(text) {
            counts.concise += w;
        }
        if verbose_re.is_match(text) {
            counts.verbose += w;
        }
        if bullet_re.is_match(text) {
            counts.bullet += w;
        }
        if no_md_re.is_match(text) {
            counts.no_md += w;
        }
        if headers_re.is_match(text) {
            counts.headers += w;
        }
        if code_only_re.is_match(text) {
            counts.code_only += w;
        }
        if let Some(caps) = lang_re.captures(text) {
            let lang = caps[2].to_lowercase();
            *counts.lang.entry(lang).or_default() += w;
        }
    }
    counts
}

/// Infer user preferences from a batch of correction rows.
///
/// Scans `correction_text` for recognizable patterns.
/// Rows with `correction_kind == "self_correction"` are skipped.
///
/// Returns at most one `InferredPreference` per preference category; the
/// caller is responsible for merging across batches via UPSERT semantics.
pub(super) fn infer_preferences(
    corrections: &[zeph_memory::sqlite::corrections::UserCorrectionRow],
) -> Vec<InferredPreference> {
    let c = count_evidence(corrections);
    let mut out = Vec::new();

    // Verbosity: require 3:1 dominance and minimum evidence.
    // Allow precision loss: evidence counts fit easily in f64 mantissa at realistic values.
    #[allow(clippy::cast_precision_loss)]
    if c.concise >= MIN_EVIDENCE && c.concise >= c.verbose * 3 {
        let total = c.concise + c.verbose;
        out.push(InferredPreference {
            key: "verbosity".to_string(),
            value: "concise".to_string(),
            confidence: c.concise as f64 / total as f64,
            evidence_count: c.concise,
        });
    } else if c.verbose >= MIN_EVIDENCE && c.verbose >= c.concise * 3 {
        #[allow(clippy::cast_precision_loss)]
        let total = c.concise + c.verbose;
        out.push(InferredPreference {
            key: "verbosity".to_string(),
            value: "verbose".to_string(),
            confidence: c.verbose as f64 / total as f64,
            evidence_count: c.verbose,
        });
    }

    // Format: pick the dominant format signal.
    let format_candidates = [
        ("bullet points", c.bullet),
        ("no markdown", c.no_md),
        ("use headers", c.headers),
        ("code only", c.code_only),
    ];
    if let Some((value, evidence)) = format_candidates
        .iter()
        .filter(|(_, e)| *e >= MIN_EVIDENCE)
        .max_by_key(|(_, e)| *e)
    {
        #[allow(clippy::cast_precision_loss)]
        let conf = (*evidence as f64 / (*evidence as f64 + 1.0)).min(0.95);
        out.push(InferredPreference {
            key: "format_preference".to_string(),
            value: (*value).to_string(),
            confidence: conf,
            evidence_count: *evidence,
        });
    }

    // Language: most-mentioned explicit language with minimum evidence.
    if let Some((lang, &count)) = c.lang.iter().max_by_key(|(_, v)| *v)
        && count >= MIN_EVIDENCE
    {
        #[allow(clippy::cast_precision_loss)]
        let conf = (count as f64 / (count as f64 + 1.0)).min(0.95);
        out.push(InferredPreference {
            key: "response_language".to_string(),
            value: lang.clone(),
            confidence: conf,
            evidence_count: count,
        });
    }

    out
}

/// Naive parser for `SQLite` datetime strings (e.g. "2024-01-15 10:30:00") to Unix seconds.
pub(super) fn chrono_parse_sqlite(s: &str) -> Result<u64, ()> {
    // Format: "YYYY-MM-DD HH:MM:SS"
    let parts: Vec<&str> = s.split(&['-', ' ', ':'][..]).collect();
    if parts.len() < 6 {
        return Err(());
    }
    let year: u64 = parts[0].parse().map_err(|_| ())?;
    let month: u64 = parts[1].parse().map_err(|_| ())?;
    let day: u64 = parts[2].parse().map_err(|_| ())?;
    let hour: u64 = parts[3].parse().map_err(|_| ())?;
    let min: u64 = parts[4].parse().map_err(|_| ())?;
    let sec: u64 = parts[5].parse().map_err(|_| ())?;

    // Rough approximation (sufficient for cooldown comparison)
    let days_approx = (year - 1970) * 365 + (month - 1) * 30 + (day - 1);
    Ok(days_approx * 86400 + hour * 3600 + min * 60 + sec)
}

#[cfg(test)]
mod tests {
    use super::super::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider, mock_provider_failing,
    };
    #[allow(clippy::wildcard_imports)]
    use super::*;
    use crate::config::LearningConfig;
    use tokio::sync::watch;
    use zeph_llm::any::AnyProvider;
    use zeph_memory::semantic::SemanticMemory;
    use zeph_skills::registry::SkillRegistry;

    async fn test_memory() -> SemanticMemory {
        let provider = AnyProvider::Mock(zeph_llm::mock::MockProvider::default());
        SemanticMemory::new(":memory:", "http://127.0.0.1:1", provider, "test-model")
            .await
            .unwrap()
    }

    /// Creates a registry with a "test-skill" and returns both the registry and the `TempDir`.
    /// The `TempDir` must be kept alive for the duration of the test because `get_skill` reads
    /// the skill body lazily from the filesystem.
    fn create_registry_with_tempdir() -> (SkillRegistry, tempfile::TempDir) {
        let temp_dir = tempfile::tempdir().unwrap();
        let skill_dir = temp_dir.path().join("test-skill");
        std::fs::create_dir(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: test-skill\ndescription: A test skill\n---\nTest skill body",
        )
        .unwrap();
        let registry = SkillRegistry::load(&[temp_dir.path().to_path_buf()]);
        (registry, temp_dir)
    }

    fn learning_config_enabled() -> LearningConfig {
        LearningConfig {
            enabled: true,
            auto_activate: false,
            min_failures: 2,
            improve_threshold: 0.7,
            rollback_threshold: 0.3,
            min_evaluations: 3,
            max_versions: 5,
            cooldown_minutes: 0,
            correction_detection: true,
            correction_confidence_threshold: 0.6,
            detector_mode: crate::config::DetectorMode::default(),
            judge_model: String::new(),
            judge_adaptive_low: 0.5,
            judge_adaptive_high: 0.8,
            correction_recall_limit: 3,
            correction_min_similarity: 0.75,
            auto_promote_min_uses: 50,
            auto_promote_threshold: 0.95,
            auto_demote_min_uses: 30,
            auto_demote_threshold: 0.40,
        }
    }

    #[test]
    fn chrono_parse_valid_datetime() {
        let secs = chrono_parse_sqlite("2024-01-15 10:30:00").unwrap();
        assert!(secs > 0);
    }

    #[test]
    fn chrono_parse_ordering_preserved() {
        let earlier = chrono_parse_sqlite("2024-01-15 10:00:00").unwrap();
        let later = chrono_parse_sqlite("2024-01-15 11:00:00").unwrap();
        assert!(later > earlier);
    }

    #[test]
    fn chrono_parse_different_days() {
        let day1 = chrono_parse_sqlite("2024-06-01 00:00:00").unwrap();
        let day2 = chrono_parse_sqlite("2024-06-02 00:00:00").unwrap();
        assert_eq!(day2 - day1, 86400);
    }

    #[test]
    fn chrono_parse_invalid_format() {
        assert!(chrono_parse_sqlite("not-a-date").is_err());
        assert!(chrono_parse_sqlite("").is_err());
        assert!(chrono_parse_sqlite("2024-01").is_err());
    }

    #[tokio::test]
    async fn write_skill_file_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let result = write_skill_file(
            &[dir.path().to_path_buf()],
            "nonexistent-skill",
            "desc",
            "body",
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn write_skill_file_updates_existing() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("test-skill");
        std::fs::create_dir(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "old content").unwrap();

        write_skill_file(
            &[dir.path().to_path_buf()],
            "test-skill",
            "new desc",
            "new body",
        )
        .await
        .unwrap();

        let content = std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
        assert!(content.contains("new body"));
        assert!(content.contains("new desc"));
    }

    #[tokio::test]
    async fn write_skill_file_rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            write_skill_file(&[dir.path().to_path_buf()], "../evil", "d", "b")
                .await
                .is_err()
        );
        assert!(
            write_skill_file(&[dir.path().to_path_buf()], "a/b", "d", "b")
                .await
                .is_err()
        );
        assert!(
            write_skill_file(&[dir.path().to_path_buf()], "a\\b", "d", "b")
                .await
                .is_err()
        );
    }

    // Priority 2: is_learning_enabled

    #[test]
    fn is_learning_enabled_no_config_returns_false() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let agent = Agent::new(provider, channel, registry, None, 5, executor);
        // No learning config set → false
        assert!(!agent.is_learning_enabled());
    }

    #[test]
    fn is_learning_enabled_with_disabled_config_returns_false() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut config = learning_config_enabled();
        config.enabled = false;
        let agent =
            Agent::new(provider, channel, registry, None, 5, executor).with_learning(config);
        assert!(!agent.is_learning_enabled());
    }

    #[test]
    fn is_learning_enabled_with_enabled_config_returns_true() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_learning(learning_config_enabled());
        assert!(agent.is_learning_enabled());
    }

    // Priority 1: check_improvement_allowed

    #[tokio::test]
    async fn check_improvement_allowed_below_min_failures_returns_false() {
        let provider = mock_provider(vec!["improved skill body".into()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        // Record 1 failure (below min_failures = 2)
        memory
            .sqlite()
            .record_skill_outcomes_batch(
                &["test-skill".to_string()],
                Some(cid),
                "tool_failure",
                None,
                None,
            )
            .await
            .unwrap();

        let config = learning_config_enabled(); // min_failures = 2
        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_learning(config.clone())
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 50);

        let mem = agent.memory_state.memory.as_ref().unwrap();
        let allowed = agent
            .check_improvement_allowed(mem, &config, "test-skill", None)
            .await
            .unwrap();
        assert!(
            !allowed,
            "should be false when below min_failures threshold"
        );
    }

    #[tokio::test]
    async fn check_improvement_allowed_high_success_rate_returns_false() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        // Record 5 successes and 2 failures (success rate = 5/7 ≈ 0.71 >= improve_threshold 0.7)
        for _ in 0..5 {
            memory
                .sqlite()
                .record_skill_outcomes_batch(
                    &["test-skill".to_string()],
                    Some(cid),
                    "success",
                    None,
                    None,
                )
                .await
                .unwrap();
        }
        for _ in 0..2 {
            memory
                .sqlite()
                .record_skill_outcomes_batch(
                    &["test-skill".to_string()],
                    Some(cid),
                    "tool_failure",
                    None,
                    None,
                )
                .await
                .unwrap();
        }

        let config = learning_config_enabled(); // improve_threshold = 0.7
        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_learning(config.clone())
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 50);

        let mem = agent.memory_state.memory.as_ref().unwrap();
        let allowed = agent
            .check_improvement_allowed(mem, &config, "test-skill", None)
            .await
            .unwrap();
        assert!(
            !allowed,
            "should be false when success rate >= improve_threshold"
        );
    }

    #[tokio::test]
    async fn check_improvement_allowed_all_conditions_met_returns_true() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        // 1 success, 3 failures (success rate = 0.25 < 0.7, failures = 3 >= min_failures = 2)
        memory
            .sqlite()
            .record_skill_outcomes_batch(
                &["test-skill".to_string()],
                Some(cid),
                "success",
                None,
                None,
            )
            .await
            .unwrap();
        for _ in 0..3 {
            memory
                .sqlite()
                .record_skill_outcomes_batch(
                    &["test-skill".to_string()],
                    Some(cid),
                    "tool_failure",
                    None,
                    None,
                )
                .await
                .unwrap();
        }

        let config = LearningConfig {
            cooldown_minutes: 0,
            min_failures: 2,
            improve_threshold: 0.7,
            ..learning_config_enabled()
        };
        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_learning(config.clone())
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 50);

        let mem = agent.memory_state.memory.as_ref().unwrap();
        let allowed = agent
            .check_improvement_allowed(mem, &config, "test-skill", None)
            .await
            .unwrap();
        assert!(allowed, "should be true when all conditions are met");
    }

    #[tokio::test]
    async fn check_improvement_allowed_with_user_feedback_skips_metrics() {
        // When user_feedback is Some, metrics check is skipped entirely → returns true
        // (assuming no cooldown active)
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        // No skill outcomes recorded → metrics would block; but user_feedback bypasses it

        let config = learning_config_enabled();
        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_learning(config.clone())
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 50);

        let mem = agent.memory_state.memory.as_ref().unwrap();
        let allowed = agent
            .check_improvement_allowed(mem, &config, "test-skill", Some("please improve this"))
            .await
            .unwrap();
        assert!(allowed, "user_feedback bypasses metrics check");
    }

    // Priority 1: generate_improved_skill evaluation gate

    #[tokio::test]
    async fn generate_improved_skill_returns_early_when_learning_disabled() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        // No learning config → is_learning_enabled() = false → returns Ok(()) immediately
        let agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent
            .generate_improved_skill("test-skill", "error", "response", None)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn generate_improved_skill_returns_early_when_no_memory() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        // Learning enabled but no memory → returns Ok(()) early
        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_learning(learning_config_enabled());

        let result = agent
            .generate_improved_skill("test-skill", "error", "response", None)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn generate_improved_skill_should_improve_false_skips_improvement() {
        // Provider returns SkillEvaluation JSON with should_improve: false → returns Ok(()) early
        let eval_json = r#"{"should_improve": false, "issues": [], "severity": "low"}"#;
        let provider = mock_provider(vec![eval_json.into()]);
        let channel = MockChannel::new(vec![]);
        // Keep tempdir alive so get_skill can load body from filesystem
        let (registry, _tempdir) = create_registry_with_tempdir();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        // Add enough failures to pass check_improvement_allowed
        for _ in 0..3 {
            memory
                .sqlite()
                .record_skill_outcomes_batch(
                    &["test-skill".to_string()],
                    Some(cid),
                    "tool_failure",
                    None,
                    None,
                )
                .await
                .unwrap();
        }

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_learning(LearningConfig {
                cooldown_minutes: 0,
                min_failures: 2,
                improve_threshold: 0.7,
                ..learning_config_enabled()
            })
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 50);

        let result = agent
            .generate_improved_skill("test-skill", "exit code 1", "response", None)
            .await;
        // Should return Ok(()) without calling improvement LLM
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn generate_improved_skill_eval_error_proceeds_with_improvement() {
        // Provider fails for eval → logs warning, proceeds to call improvement LLM
        // Second call (improvement) also fails (failing provider) → error propagates
        let provider = mock_provider_failing();
        let channel = MockChannel::new(vec![]);
        // Keep tempdir alive so get_skill can load body from filesystem
        let (registry, _tempdir) = create_registry_with_tempdir();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        // Add enough failures
        for _ in 0..3 {
            memory
                .sqlite()
                .record_skill_outcomes_batch(
                    &["test-skill".to_string()],
                    Some(cid),
                    "tool_failure",
                    None,
                    None,
                )
                .await
                .unwrap();
        }

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_learning(LearningConfig {
                cooldown_minutes: 0,
                min_failures: 2,
                improve_threshold: 0.7,
                ..learning_config_enabled()
            })
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 50);

        let result = agent
            .generate_improved_skill("test-skill", "exit code 1", "response", None)
            .await;
        // eval fails (warn) → proceeds to call_improvement_llm → provider fails → Err
        assert!(result.is_err());
    }

    // Priority 2: attempt_self_reflection

    #[tokio::test]
    async fn attempt_self_reflection_learning_disabled_returns_false() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        // No learning config → is_learning_enabled() = false
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.attempt_self_reflection("error", "output").await;
        assert!(result.is_ok());
        assert!(!result.unwrap());
    }

    #[tokio::test]
    async fn attempt_self_reflection_reflection_used_returns_false() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_learning(learning_config_enabled());

        // Mark reflection as already used
        agent.learning_engine.mark_reflection_used();

        let result = agent.attempt_self_reflection("error", "output").await;
        assert!(result.is_ok());
        assert!(!result.unwrap());
    }

    // Priority 2: write_skill_file with multiple paths

    #[tokio::test]
    async fn write_skill_file_uses_first_matching_path() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();

        // Create skill only in dir2
        let skill_dir = dir2.path().join("my-skill");
        std::fs::create_dir(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "old").unwrap();

        // dir1 has no matching skill dir
        write_skill_file(
            &[dir1.path().to_path_buf(), dir2.path().to_path_buf()],
            "my-skill",
            "desc",
            "updated body",
        )
        .await
        .unwrap();

        let content = std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
        assert!(content.contains("updated body"));
    }

    #[tokio::test]
    async fn write_skill_file_empty_paths_returns_error() {
        let result = write_skill_file(&[], "any-skill", "desc", "body").await;
        assert!(result.is_err());
    }

    // Priority 3: handle_skill_command dispatch (no memory → early exit messages)

    #[tokio::test]
    async fn handle_skill_command_unknown_subcommand() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.handle_skill_command("unknown-cmd").await.unwrap();
        let sent = agent.channel.sent_messages();
        assert!(sent.iter().any(|s| s.contains("Unknown /skill subcommand")));
    }

    #[tokio::test]
    async fn handle_skill_command_stats_no_memory() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.handle_skill_command("stats").await.unwrap();
        let sent = agent.channel.sent_messages();
        assert!(sent.iter().any(|s| s.contains("Memory not available")));
    }

    #[tokio::test]
    async fn handle_skill_command_versions_no_name() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.handle_skill_command("versions").await.unwrap();
        let sent = agent.channel.sent_messages();
        assert!(sent.iter().any(|s| s.contains("Usage: /skill versions")));
    }

    #[tokio::test]
    async fn handle_skill_command_activate_no_args() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.handle_skill_command("activate").await.unwrap();
        let sent = agent.channel.sent_messages();
        assert!(sent.iter().any(|s| s.contains("Usage: /skill activate")));
    }

    #[tokio::test]
    async fn handle_skill_command_approve_no_name() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.handle_skill_command("approve").await.unwrap();
        let sent = agent.channel.sent_messages();
        assert!(sent.iter().any(|s| s.contains("Usage: /skill approve")));
    }

    #[tokio::test]
    async fn handle_skill_command_reset_no_name() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.handle_skill_command("reset").await.unwrap();
        let sent = agent.channel.sent_messages();
        assert!(sent.iter().any(|s| s.contains("Usage: /skill reset")));
    }

    #[tokio::test]
    async fn handle_skill_command_versions_no_memory() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent
            .handle_skill_command("versions test-skill")
            .await
            .unwrap();
        let sent = agent.channel.sent_messages();
        assert!(sent.iter().any(|s| s.contains("Memory not available")));
    }

    #[tokio::test]
    async fn handle_skill_command_activate_invalid_version() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent
            .handle_skill_command("activate test-skill not-a-number")
            .await
            .unwrap();
        let sent = agent.channel.sent_messages();
        assert!(sent.iter().any(|s| s.contains("Invalid version number")));
    }

    #[tokio::test]
    async fn record_skill_outcomes_no_active_skills_is_noop() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let agent = Agent::new(provider, channel, registry, None, 5, executor);

        // No active skills and no memory → should return immediately without panic
        agent.record_skill_outcomes("success", None, None).await;
        agent
            .record_skill_outcomes("tool_failure", Some("error"), None)
            .await;
    }

    // Priority 3: handle_skill_install / handle_skill_remove via handle_skill_command

    #[tokio::test]
    async fn handle_skill_command_install_no_source() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.handle_skill_command("install").await.unwrap();
        let sent = agent.channel.sent_messages();
        assert!(
            sent.iter().any(|s| s.contains("Usage: /skill install")),
            "expected usage hint, got: {sent:?}"
        );
    }

    #[tokio::test]
    async fn handle_skill_command_remove_no_name() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.handle_skill_command("remove").await.unwrap();
        let sent = agent.channel.sent_messages();
        assert!(
            sent.iter().any(|s| s.contains("Usage: /skill remove")),
            "expected usage hint, got: {sent:?}"
        );
    }

    #[tokio::test]
    async fn handle_skill_command_install_no_managed_dir() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        // No managed_dir configured
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent
            .handle_skill_command("install https://example.com/skill")
            .await
            .unwrap();
        let sent = agent.channel.sent_messages();
        assert!(
            sent.iter().any(|s| s.contains("not configured")),
            "expected not-configured message, got: {sent:?}"
        );
    }

    #[tokio::test]
    async fn handle_skill_command_remove_no_managed_dir() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        // No managed_dir configured
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.handle_skill_command("remove my-skill").await.unwrap();
        let sent = agent.channel.sent_messages();
        assert!(
            sent.iter().any(|s| s.contains("not configured")),
            "expected not-configured message, got: {sent:?}"
        );
    }

    #[tokio::test]
    async fn handle_skill_command_install_from_path_not_found() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let managed = tempfile::tempdir().unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_managed_skills_dir(managed.path().to_path_buf());

        agent
            .handle_skill_command("install /nonexistent/path/to/skill")
            .await
            .unwrap();
        let sent = agent.channel.sent_messages();
        assert!(
            sent.iter().any(|s| s.contains("Install failed")),
            "expected install failure message, got: {sent:?}"
        );
    }

    #[tokio::test]
    async fn handle_skill_command_remove_nonexistent_skill() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let managed = tempfile::tempdir().unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_managed_skills_dir(managed.path().to_path_buf());

        agent
            .handle_skill_command("remove nonexistent-skill")
            .await
            .unwrap();
        let sent = agent.channel.sent_messages();
        assert!(
            sent.iter().any(|s| s.contains("Remove failed")),
            "expected remove failure message, got: {sent:?}"
        );
    }

    #[tokio::test]
    async fn handle_skill_reject_records_outcome_and_replies() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let (registry, _tempdir) = create_registry_with_tempdir();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            50,
        );

        agent
            .handle_skill_command("reject test-skill the output was wrong")
            .await
            .unwrap();

        let sent = agent.channel.sent_messages();
        assert!(
            sent.iter().any(|s| s.contains("Rejection recorded")),
            "expected rejection confirmation, got: {sent:?}"
        );

        let mem = agent.memory_state.memory.as_ref().unwrap();
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT outcome FROM skill_outcomes WHERE skill_name = 'test-skill' LIMIT 1",
        )
        .fetch_optional(mem.sqlite().pool())
        .await
        .unwrap();
        assert!(row.is_some(), "outcome should be recorded in DB");
        assert_eq!(row.unwrap().0, "user_rejection");
    }

    #[tokio::test]
    async fn handle_skill_reject_unknown_skill_returns_error_message() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent
            .handle_skill_command("reject nonexistent-skill bad output")
            .await
            .unwrap();

        let sent = agent.channel.sent_messages();
        assert!(
            sent.iter().any(|s| s.contains("Unknown skill")),
            "expected unknown skill message, got: {sent:?}"
        );
    }

    #[tokio::test]
    async fn handle_skill_reject_missing_name_shows_usage() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.handle_skill_command("reject").await.unwrap();

        let sent = agent.channel.sent_messages();
        assert!(
            sent.iter().any(|s| s.contains("Usage")),
            "expected usage message, got: {sent:?}"
        );
    }

    #[tokio::test]
    async fn handle_skill_reject_missing_reason_shows_usage() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let (registry, _tempdir) = create_registry_with_tempdir();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent
            .handle_skill_command("reject test-skill")
            .await
            .unwrap();

        let sent = agent.channel.sent_messages();
        assert!(
            sent.iter().any(|s| s.contains("Usage")),
            "expected usage message, got: {sent:?}"
        );
    }

    // check_trust_transition: auto-promote and auto-demote

    async fn setup_skill_with_outcomes(
        memory: &SemanticMemory,
        skill_name: &str,
        successes: u32,
        failures: u32,
        initial_trust: &str,
    ) {
        use zeph_memory::sqlite::SourceKind;
        memory
            .sqlite()
            .upsert_skill_trust(
                skill_name,
                initial_trust,
                SourceKind::Local,
                None,
                None,
                "hash",
            )
            .await
            .unwrap();
        for _ in 0..successes {
            memory
                .sqlite()
                .record_skill_outcome(skill_name, None, None, "success", None, None)
                .await
                .unwrap();
        }
        for _ in 0..failures {
            memory
                .sqlite()
                .record_skill_outcome(skill_name, None, None, "tool_failure", None, None)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn check_trust_transition_auto_promotes_to_trusted() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        // 50 successes, 0 failures → posterior > 0.95 threshold
        setup_skill_with_outcomes(&memory, "test-skill", 50, 0, "local").await;

        let mut config = learning_config_enabled();
        config.auto_promote_min_uses = 50;
        config.auto_promote_threshold = 0.95;

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_learning(config)
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 50);

        let mem = agent.memory_state.memory.as_ref().unwrap();
        agent.check_trust_transition("test-skill").await;

        let row = mem
            .sqlite()
            .load_skill_trust("test-skill")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.trust_level, "trusted",
            "should auto-promote to trusted, got: {}",
            row.trust_level
        );
    }

    #[tokio::test]
    async fn check_trust_transition_auto_demotes_to_quarantined() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        // 5 successes, 30 failures → posterior < 0.40 threshold, starting as "trusted"
        setup_skill_with_outcomes(&memory, "test-skill", 5, 30, "trusted").await;

        let mut config = learning_config_enabled();
        config.auto_demote_min_uses = 30;
        config.auto_demote_threshold = 0.40;

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_learning(config)
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 50);

        let mem = agent.memory_state.memory.as_ref().unwrap();
        agent.check_trust_transition("test-skill").await;

        let row = mem
            .sqlite()
            .load_skill_trust("test-skill")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.trust_level, "quarantined",
            "should auto-demote to quarantined, got: {}",
            row.trust_level
        );
    }

    #[tokio::test]
    async fn check_trust_transition_does_not_promote_blocked() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        // High success rate but "blocked" — should NOT be promoted
        setup_skill_with_outcomes(&memory, "test-skill", 100, 0, "blocked").await;

        let mut config = learning_config_enabled();
        config.auto_promote_min_uses = 50;
        config.auto_promote_threshold = 0.95;

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_learning(config)
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 50);

        let mem = agent.memory_state.memory.as_ref().unwrap();
        agent.check_trust_transition("test-skill").await;

        let row = mem
            .sqlite()
            .load_skill_trust("test-skill")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.trust_level, "blocked",
            "blocked skill should never be auto-promoted, got: {}",
            row.trust_level
        );
    }

    // Priority 3: proptest

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn chrono_parse_never_panics(s in ".*") {
            let _ = chrono_parse_sqlite(&s);
        }
    }

    #[tokio::test]
    async fn skill_confidence_populated_before_first_outcome() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        // Record one success so load_skill_outcome_stats returns data.
        memory
            .sqlite()
            .record_skill_outcomes_batch(
                &["test-skill".to_string()],
                Some(cid),
                "success",
                None,
                None,
            )
            .await
            .unwrap();

        let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());
        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_metrics(tx)
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 50);

        // update_skill_confidence_metrics is called inside rebuild_system_prompt after
        // active_skills is set. Invoke directly to test the fix in isolation.
        agent.update_skill_confidence_metrics().await;

        let snapshot = rx.borrow().clone();
        assert!(
            !snapshot.skill_confidence.is_empty(),
            "skill_confidence must be populated after update_skill_confidence_metrics"
        );
        let entry = snapshot
            .skill_confidence
            .iter()
            .find(|c| c.name == "test-skill")
            .expect("test-skill confidence entry must exist");
        assert!(
            entry.total_uses > 0,
            "total_uses must reflect recorded outcome"
        );
    }

    // ── infer_preferences unit tests ──────────────────────────────────────────

    fn make_correction(
        id: i64,
        text: &str,
        kind: &str,
    ) -> zeph_memory::sqlite::corrections::UserCorrectionRow {
        zeph_memory::sqlite::corrections::UserCorrectionRow {
            id,
            session_id: None,
            original_output: String::new(),
            correction_text: text.to_string(),
            skill_name: None,
            correction_kind: kind.to_string(),
            created_at: String::new(),
        }
    }

    #[test]
    fn infer_verbosity_concise() {
        let rows = vec![
            make_correction(1, "be brief please", "explicit_rejection"),
            make_correction(2, "too long, be concise", "alternative_request"),
            make_correction(3, "shorter response next time", "explicit_rejection"),
        ];
        let prefs = super::infer_preferences(&rows);
        let verbosity = prefs.iter().find(|p| p.key == "verbosity");
        assert!(verbosity.is_some(), "should detect verbosity preference");
        assert_eq!(verbosity.unwrap().value, "concise");
        assert!(verbosity.unwrap().confidence >= 0.7);
    }

    #[test]
    fn infer_verbosity_requires_min_evidence() {
        // Only 2 corrections — below MIN_EVIDENCE
        let rows = vec![
            make_correction(1, "be brief", "explicit_rejection"),
            make_correction(2, "too long", "explicit_rejection"),
        ];
        let prefs = super::infer_preferences(&rows);
        assert!(!prefs.iter().any(|p| p.key == "verbosity"));
    }

    #[test]
    fn infer_skips_self_correction() {
        // 5 self_corrections with concise signals — should not emit verbosity
        let rows: Vec<_> = (1..=5)
            .map(|i| make_correction(i, "be concise", "self_correction"))
            .collect();
        let prefs = super::infer_preferences(&rows);
        assert!(!prefs.iter().any(|p| p.key == "verbosity"));
    }

    #[test]
    fn infer_format_bullet_points() {
        let rows: Vec<_> = (1..=4)
            .map(|i| make_correction(i, "use bullet points please", "alternative_request"))
            .collect();
        let prefs = super::infer_preferences(&rows);
        let fmt = prefs.iter().find(|p| p.key == "format_preference");
        assert!(fmt.is_some());
        assert_eq!(fmt.unwrap().value, "bullet points");
    }

    #[test]
    fn infer_language_russian() {
        let rows: Vec<_> = (1..=3)
            .map(|i| make_correction(i, "respond in russian please", "alternative_request"))
            .collect();
        let prefs = super::infer_preferences(&rows);
        let lang = prefs.iter().find(|p| p.key == "response_language");
        assert!(lang.is_some());
        assert_eq!(lang.unwrap().value, "russian");
    }

    #[test]
    fn infer_no_false_positive_from_unrelated_shorter() {
        // "shorter path" should not match — "shorter response" matches but "shorter path" doesn't
        let rows = vec![
            make_correction(1, "try a shorter path", "explicit_rejection"),
            make_correction(2, "no, wrong command", "explicit_rejection"),
        ];
        let prefs = super::infer_preferences(&rows);
        // Only 2 rows and no verbosity-specific patterns — should not emit verbosity
        assert!(!prefs.iter().any(|p| p.key == "verbosity"));
    }

    #[test]
    fn infer_no_result_on_empty_input() {
        let prefs = super::infer_preferences(&[]);
        assert!(prefs.is_empty());
    }

    #[test]
    fn infer_alternative_request_weighs_more() {
        // 2 alternative_request (weight 2 each = 4) + 0 verbose signals → total evidence 4 >= MIN 3
        let rows = vec![
            make_correction(1, "be brief", "alternative_request"),
            make_correction(2, "be concise", "alternative_request"),
        ];
        let prefs = super::infer_preferences(&rows);
        let verbosity = prefs.iter().find(|p| p.key == "verbosity");
        assert!(
            verbosity.is_some(),
            "alternative_request weight should push over threshold"
        );
        assert_eq!(verbosity.unwrap().value, "concise");
    }

    // ── analyze_and_learn / inject_learned_preferences integration tests ──────

    fn agent_with_memory(memory: std::sync::Arc<SemanticMemory>) -> Agent<MockChannel> {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            memory,
            zeph_memory::ConversationId(1),
            50,
            5,
            50,
        )
    }

    #[tokio::test]
    async fn analyze_and_learn_advances_watermark() {
        let memory = std::sync::Arc::new(test_memory().await);
        // Store 3 concise-signal corrections.
        for _ in 0..3u32 {
            memory
                .sqlite()
                .store_user_correction(None, "out", "be brief", None, "explicit_rejection")
                .await
                .unwrap();
        }

        let mut agent = agent_with_memory(memory.clone());
        agent.learning_engine.config = Some(LearningConfig {
            correction_detection: true,
            ..Default::default()
        });
        // Advance turn counter past the analysis interval (default 5).
        for _ in 0..5 {
            agent.learning_engine.tick();
        }
        assert!(agent.learning_engine.should_analyze());

        let watermark_before = agent.learning_engine.last_analyzed_correction_id;
        agent.analyze_and_learn().await;
        let watermark_after = agent.learning_engine.last_analyzed_correction_id;

        assert!(
            watermark_after > watermark_before,
            "watermark must advance after analysis"
        );
        assert!(
            !agent.learning_engine.should_analyze(),
            "should_analyze must return false immediately after mark_analyzed"
        );
    }

    #[tokio::test]
    async fn analyze_and_learn_persists_high_confidence_preference() {
        let memory = std::sync::Arc::new(test_memory().await);
        // 5 concise signals via alternative_request (weight 2 each = 10 evidence).
        for _ in 0..5 {
            memory
                .sqlite()
                .store_user_correction(None, "out", "be brief please", None, "alternative_request")
                .await
                .unwrap();
        }

        let mut agent = agent_with_memory(memory.clone());
        agent.learning_engine.config = Some(LearningConfig {
            correction_detection: true,
            ..Default::default()
        });
        for _ in 0..5 {
            agent.learning_engine.tick();
        }

        agent.analyze_and_learn().await;

        let prefs = memory.sqlite().load_learned_preferences().await.unwrap();
        let verbosity = prefs.iter().find(|p| p.preference_key == "verbosity");
        assert!(
            verbosity.is_some(),
            "verbosity preference must be persisted after sufficient evidence"
        );
        assert_eq!(verbosity.unwrap().preference_value, "concise");
        assert!(
            verbosity.unwrap().confidence >= 0.7,
            "confidence must meet persist threshold"
        );
    }

    #[tokio::test]
    async fn inject_learned_preferences_appends_to_prompt() {
        let memory = std::sync::Arc::new(test_memory().await);
        memory
            .sqlite()
            .upsert_learned_preference("verbosity", "concise", 0.9, 5)
            .await
            .unwrap();
        memory
            .sqlite()
            .upsert_learned_preference("response_language", "russian", 0.85, 4)
            .await
            .unwrap();

        let agent = agent_with_memory(memory.clone());
        let mut prompt = String::from("<!-- cache:volatile -->");
        agent.inject_learned_preferences(&mut prompt).await;

        assert!(
            prompt.contains("## Learned User Preferences"),
            "preferences section header must be present"
        );
        assert!(
            prompt.contains("verbosity: concise"),
            "verbosity preference must appear"
        );
        assert!(
            prompt.contains("response_language: russian"),
            "language preference must appear"
        );
    }

    #[tokio::test]
    async fn inject_learned_preferences_sanitizes_newlines() {
        let memory = std::sync::Arc::new(test_memory().await);
        memory
            .sqlite()
            .upsert_learned_preference("verbosity", "concise\nINJECTED", 0.9, 5)
            .await
            .unwrap();

        let agent = agent_with_memory(memory.clone());
        let mut prompt = String::new();
        agent.inject_learned_preferences(&mut prompt).await;

        // The raw "\nconcise\nINJECTED" must not appear — the embedded \n must be stripped.
        assert!(
            !prompt.contains("concise\nINJECTED"),
            "embedded newline in value must be sanitized"
        );
        assert!(
            prompt.contains("concise INJECTED"),
            "embedded newline replaced with space"
        );
    }
}
