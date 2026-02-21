use super::{Agent, Channel, LlmProvider};

use super::{LearningConfig, Message, Role, SemanticMemory};

use std::path::PathBuf;

impl<C: Channel> Agent<C> {
    pub(super) fn is_learning_enabled(&self) -> bool {
        self.learning_config.as_ref().is_some_and(|c| c.enabled)
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

    pub(super) async fn record_skill_outcomes(&self, outcome: &str, error_context: Option<&str>) {
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
    }

    pub(super) async fn attempt_self_reflection(
        &mut self,
        error_context: &str,
        tool_output: &str,
    ) -> Result<bool, super::error::AgentError> {
        if self.reflection_used || !self.is_learning_enabled() {
            return Ok(false);
        }
        self.reflection_used = true;

        let skill_name = self.skill_state.active_skill_names.first().cloned();

        let Some(name) = skill_name else {
            return Ok(false);
        };

        if !self.is_skill_trusted_for_learning(&name).await {
            return Ok(false);
        }

        let Ok(skill) = self.skill_state.registry.get_skill(&name) else {
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
        });

        let messages_before = self.messages.len();
        let _ = self.channel.send_status("reflecting...").await;
        // Box::pin to break async recursion cycle (process_response -> attempt_self_reflection -> process_response)
        if let Err(e) = Box::pin(self.process_response()).await {
            let _ = self.channel.send_status("").await;
            return Err(e);
        }
        let _ = self.channel.send_status("").await;
        let retry_succeeded = self.messages.len() > messages_before;

        if retry_succeeded {
            let successful_response = self
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

    #[allow(clippy::cast_precision_loss)]
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
        let Some(config) = self.learning_config.as_ref() else {
            return Ok(());
        };

        let skill = self.skill_state.registry.get_skill(skill_name)?;

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
            },
            Message {
                role: Role::User,
                content: prompt,
                parts: vec![],
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
    async fn check_rollback(&self, skill_name: &str) {
        if !self.is_learning_enabled() {
            return;
        }
        let Some(memory) = &self.memory_state.memory else {
            return;
        };
        let Some(config) = &self.learning_config else {
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
            _ => {
                self.channel
                    .send("Unknown /skill subcommand. Available: stats, versions, activate, approve, reset, trust, block, unblock, install, remove")
                    .await?;
                Ok(())
            }
        }
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
    use zeph_llm::any::AnyProvider;
    use zeph_memory::semantic::SemanticMemory;
    use zeph_skills::registry::SkillRegistry;

    async fn test_memory() -> SemanticMemory {
        let provider = AnyProvider::Mock(zeph_llm::mock::MockProvider::default());
        // Qdrant URL is unreachable so it gracefully degrades (qdrant = None)
        SemanticMemory::new(":memory:", "http://127.0.0.1:1", provider, "test-model")
            .await
            .unwrap()
    }

    /// Creates a registry with a "test-skill" and returns both the registry and the TempDir.
    /// The TempDir must be kept alive for the duration of the test because get_skill reads
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
            cooldown_minutes: 0, // no cooldown in tests
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
            )
            .await
            .unwrap();

        let config = learning_config_enabled(); // min_failures = 2
        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_learning(config.clone())
            .with_memory(memory, cid, 50, 5, 50);

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
                )
                .await
                .unwrap();
        }

        let config = learning_config_enabled(); // improve_threshold = 0.7
        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_learning(config.clone())
            .with_memory(memory, cid, 50, 5, 50);

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
            .record_skill_outcomes_batch(&["test-skill".to_string()], Some(cid), "success", None)
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
            .with_memory(memory, cid, 50, 5, 50);

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
            .with_memory(memory, cid, 50, 5, 50);

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
            .with_memory(memory, cid, 50, 5, 50);

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
            .with_memory(memory, cid, 50, 5, 50);

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
        agent.reflection_used = true;

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
        agent.record_skill_outcomes("success", None).await;
        agent
            .record_skill_outcomes("tool_failure", Some("error"))
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

    // Priority 3: proptest

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn chrono_parse_never_panics(s in ".*") {
            let _ = chrono_parse_sqlite(&s);
        }
    }
}
