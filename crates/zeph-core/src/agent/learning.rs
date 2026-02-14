use super::{Agent, Channel, LlmProvider, ToolExecutor};
#[cfg(feature = "self-learning")]
use super::{LearningConfig, Message, Role, SemanticMemory};
#[cfg(feature = "self-learning")]
use std::path::PathBuf;

impl<P: LlmProvider + Clone + 'static, C: Channel, T: ToolExecutor> Agent<P, C, T> {
    #[cfg(feature = "self-learning")]
    pub(super) fn is_learning_enabled(&self) -> bool {
        self.learning_config.as_ref().is_some_and(|c| c.enabled)
    }

    #[cfg(not(feature = "self-learning"))]
    #[allow(dead_code, clippy::unused_self)]
    pub(super) fn is_learning_enabled(&self) -> bool {
        false
    }

    #[cfg(feature = "self-learning")]
    pub(super) async fn record_skill_outcomes(&self, outcome: &str, error_context: Option<&str>) {
        if self.active_skill_names.is_empty() {
            return;
        }
        let Some(memory) = &self.memory else { return };
        if let Err(e) = memory
            .sqlite()
            .record_skill_outcomes_batch(
                &self.active_skill_names,
                self.conversation_id,
                outcome,
                error_context,
            )
            .await
        {
            tracing::warn!("failed to record skill outcomes: {e:#}");
        }

        if outcome != "success" {
            for name in &self.active_skill_names {
                self.check_rollback(name).await;
            }
        }
    }

    #[cfg(not(feature = "self-learning"))]
    #[allow(clippy::unused_async)]
    pub(super) async fn record_skill_outcomes(&self, _outcome: &str, _error_context: Option<&str>) {
    }

    #[cfg(feature = "self-learning")]
    pub(super) async fn attempt_self_reflection(
        &mut self,
        error_context: &str,
        tool_output: &str,
    ) -> anyhow::Result<bool> {
        if self.reflection_used || !self.is_learning_enabled() {
            return Ok(false);
        }
        self.reflection_used = true;

        let skill_name = self.active_skill_names.first().cloned();

        let Some(name) = skill_name else {
            return Ok(false);
        };

        let Ok(skill) = self.registry.get_skill(&name) else {
            return Ok(false);
        };

        let prompt = zeph_skills::evolution::build_reflection_prompt(
            skill.name(),
            &skill.body,
            error_context,
            tool_output,
        );

        self.messages.push(Message {
            role: Role::User,
            content: prompt,
            parts: vec![],
        });

        let messages_before = self.messages.len();
        // Box::pin to break async recursion cycle (process_response -> attempt_self_reflection -> process_response)
        Box::pin(self.process_response()).await?;
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

    #[cfg(feature = "self-learning")]
    #[allow(clippy::cast_precision_loss)]
    pub(super) async fn generate_improved_skill(
        &self,
        skill_name: &str,
        error_context: &str,
        successful_response: &str,
        user_feedback: Option<&str>,
    ) -> anyhow::Result<()> {
        if !self.is_learning_enabled() {
            return Ok(());
        }

        let Some(memory) = &self.memory else {
            return Ok(());
        };
        let Some(config) = self.learning_config.as_ref() else {
            return Ok(());
        };

        let skill = self.registry.get_skill(skill_name)?;

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

    #[cfg(feature = "self-learning")]
    #[allow(clippy::cast_precision_loss)]
    async fn check_improvement_allowed(
        &self,
        memory: &SemanticMemory<P>,
        config: &LearningConfig,
        skill_name: &str,
        user_feedback: Option<&str>,
    ) -> anyhow::Result<bool> {
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

    #[cfg(feature = "self-learning")]
    async fn call_improvement_llm(
        &self,
        skill_name: &str,
        original_body: &str,
        error_context: &str,
        successful_response: &str,
        user_feedback: Option<&str>,
    ) -> anyhow::Result<String> {
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

        self.provider.chat(&messages).await
    }

    #[cfg(feature = "self-learning")]
    async fn store_improved_version(
        &self,
        memory: &SemanticMemory<P>,
        config: &LearningConfig,
        skill_name: &str,
        generated_body: &str,
        description: &str,
        error_context: &str,
    ) -> anyhow::Result<()> {
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
            write_skill_file(&self.skill_paths, skill_name, description, generated_body).await?;
            tracing::info!("auto-activated v{next_ver} for {skill_name}");
        }

        memory
            .sqlite()
            .prune_skill_versions(skill_name, config.max_versions)
            .await?;

        Ok(())
    }

    #[cfg(feature = "self-learning")]
    #[allow(clippy::cast_precision_loss)]
    async fn check_rollback(&self, skill_name: &str) {
        if !self.is_learning_enabled() {
            return;
        }
        let Some(memory) = &self.memory else { return };
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
                &self.skill_paths,
                skill_name,
                &predecessor.description,
                &predecessor.body,
            )
            .await
            .ok();
        }
    }

    #[cfg(feature = "self-learning")]
    pub(super) async fn handle_skill_command(&mut self, args: &str) -> anyhow::Result<()> {
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
            _ => {
                self.channel
                    .send("Unknown /skill subcommand. Available: stats, versions, activate, approve, reset")
                    .await
            }
        }
    }

    #[cfg(not(feature = "self-learning"))]
    pub(super) async fn handle_skill_command(&mut self, _args: &str) -> anyhow::Result<()> {
        self.channel
            .send("Self-learning feature is not enabled.")
            .await
    }

    #[cfg(feature = "self-learning")]
    async fn handle_skill_stats(&mut self) -> anyhow::Result<()> {
        use std::fmt::Write;

        let Some(memory) = &self.memory else {
            return self.channel.send("Memory not available.").await;
        };

        let stats = memory.sqlite().load_skill_outcome_stats().await?;
        if stats.is_empty() {
            return self.channel.send("No skill outcome data yet.").await;
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

        self.channel.send(&output).await
    }

    #[cfg(feature = "self-learning")]
    async fn handle_skill_versions(&mut self, name: Option<&str>) -> anyhow::Result<()> {
        use std::fmt::Write;

        let Some(name) = name else {
            return self.channel.send("Usage: /skill versions <name>").await;
        };
        let Some(memory) = &self.memory else {
            return self.channel.send("Memory not available.").await;
        };

        let versions = memory.sqlite().load_skill_versions(name).await?;
        if versions.is_empty() {
            return self
                .channel
                .send(&format!("No versions found for \"{name}\"."))
                .await;
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

        self.channel.send(&output).await
    }

    #[cfg(feature = "self-learning")]
    async fn handle_skill_activate(
        &mut self,
        name: Option<&str>,
        version_str: Option<&str>,
    ) -> anyhow::Result<()> {
        let (Some(name), Some(ver_str)) = (name, version_str) else {
            return self
                .channel
                .send("Usage: /skill activate <name> <version>")
                .await;
        };
        let Ok(ver) = ver_str.parse::<i64>() else {
            return self.channel.send("Invalid version number.").await;
        };
        let Some(memory) = &self.memory else {
            return self.channel.send("Memory not available.").await;
        };

        let versions = memory.sqlite().load_skill_versions(name).await?;
        let Some(target) = versions.iter().find(|v| v.version == ver) else {
            return self
                .channel
                .send(&format!("Version {ver} not found for \"{name}\"."))
                .await;
        };

        memory
            .sqlite()
            .activate_skill_version(name, target.id)
            .await?;

        write_skill_file(&self.skill_paths, name, &target.description, &target.body).await?;

        self.channel
            .send(&format!("Activated v{ver} for \"{name}\"."))
            .await
    }

    #[cfg(feature = "self-learning")]
    async fn handle_skill_approve(&mut self, name: Option<&str>) -> anyhow::Result<()> {
        let Some(name) = name else {
            return self.channel.send("Usage: /skill approve <name>").await;
        };
        let Some(memory) = &self.memory else {
            return self.channel.send("Memory not available.").await;
        };

        let versions = memory.sqlite().load_skill_versions(name).await?;
        let pending = versions
            .iter()
            .rfind(|v| v.source == "auto" && !v.is_active);

        let Some(target) = pending else {
            return self
                .channel
                .send(&format!("No pending auto version for \"{name}\"."))
                .await;
        };

        memory
            .sqlite()
            .activate_skill_version(name, target.id)
            .await?;

        write_skill_file(&self.skill_paths, name, &target.description, &target.body).await?;

        self.channel
            .send(&format!(
                "Approved and activated v{} for \"{name}\".",
                target.version
            ))
            .await
    }

    #[cfg(feature = "self-learning")]
    async fn handle_skill_reset(&mut self, name: Option<&str>) -> anyhow::Result<()> {
        let Some(name) = name else {
            return self.channel.send("Usage: /skill reset <name>").await;
        };
        let Some(memory) = &self.memory else {
            return self.channel.send("Memory not available.").await;
        };

        let versions = memory.sqlite().load_skill_versions(name).await?;
        let Some(v1) = versions.iter().find(|v| v.version == 1) else {
            return self
                .channel
                .send(&format!("Original version not found for \"{name}\"."))
                .await;
        };

        memory.sqlite().activate_skill_version(name, v1.id).await?;

        write_skill_file(&self.skill_paths, name, &v1.description, &v1.body).await?;

        self.channel
            .send(&format!("Reset \"{name}\" to original v1."))
            .await
    }
}

#[cfg(feature = "self-learning")]
pub(super) async fn write_skill_file(
    skill_paths: &[PathBuf],
    skill_name: &str,
    description: &str,
    body: &str,
) -> anyhow::Result<()> {
    if skill_name.contains('/') || skill_name.contains('\\') || skill_name.contains("..") {
        anyhow::bail!("invalid skill name: {skill_name}");
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
    anyhow::bail!("skill directory not found for {skill_name}")
}

/// Naive parser for `SQLite` datetime strings (e.g. "2024-01-15 10:30:00") to Unix seconds.
#[cfg(feature = "self-learning")]
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
#[cfg(feature = "self-learning")]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;

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
}
