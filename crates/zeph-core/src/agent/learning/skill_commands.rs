// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::super::{Agent, Channel, LlmProvider};
use super::background::write_skill_file;

impl<C: Channel> Agent<C> {
    pub(crate) async fn handle_skill_command(
        &mut self,
        args: &str,
    ) -> Result<(), super::super::error::AgentError> {
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
            Some("create") => {
                let description = parts[1..].join(" ");
                self.handle_skill_create(&description).await
            }
            Some("scan") => self.handle_skill_scan().await,
            Some("reject") => {
                let tail = if parts.len() > 2 { &parts[2..] } else { &[] };
                self.handle_skill_reject(parts.get(1).copied(), tail).await
            }
            _ => {
                self.channel
                    .send("Unknown /skill subcommand. Available: stats, versions, activate, approve, reset, trust, block, unblock, install, remove, reject, scan, create")
                    .await?;
                Ok(())
            }
        }
    }

    /// Handle `/skill create <description>` — generate a SKILL.md via LLM and save it.
    #[allow(clippy::too_many_lines)]
    async fn handle_skill_create(
        &mut self,
        description: &str,
    ) -> Result<(), super::super::error::AgentError> {
        if description.trim().is_empty() {
            self.channel
                .send("Usage: /skill create <description>\n\nExample:\n  /skill create fetch weather data from wttr.in and display current conditions")
                .await?;
            return Ok(());
        }

        if description.chars().count() > 2048 {
            self.channel
                .send("Description too long (max 2048 characters).")
                .await?;
            return Ok(());
        }

        let input_scan = zeph_skills::scanner::scan_skill_body(description);
        if input_scan.has_matches() {
            self.channel
                .send("Input blocked: injection patterns detected in description.")
                .await?;
            return Ok(());
        }

        // Determine output directory: generation_output_dir > managed_dir > first skill_path.
        let output_dir = if let Some(ref dir) = self.skill_state.generation_output_dir {
            dir.clone()
        } else if let Some(ref dir) = self.skill_state.managed_dir {
            dir.clone()
        } else if let Some(first) = self.skill_state.skill_paths.first() {
            first.clone()
        } else {
            self.channel
                .send("No skill output directory configured. Set skills.generation_output_dir or skills.paths.")
                .await?;
            return Ok(());
        };
        // Warn if output_dir is not in watched skill_paths (hot-reload may miss the new skill).
        let is_watched = self
            .skill_state
            .skill_paths
            .iter()
            .any(|p| output_dir.starts_with(p) || p == &output_dir);
        if !is_watched {
            tracing::warn!(
                output_dir = %output_dir.display(),
                "generation_output_dir is not in skills.paths — hot-reload may not pick up the new skill"
            );
            self.channel
                .send(&format!(
                    "Warning: {} is not listed in skills.paths. The generated skill may not be hot-reloaded automatically.",
                    output_dir.display()
                ))
                .await?;
        }

        let generation_provider =
            self.resolve_background_provider(&self.skill_state.generation_provider_name.clone());
        let generator = zeph_skills::SkillGenerator::new(generation_provider, output_dir.clone());
        self.channel
            .send(&format!("Generating skill from: \"{description}\"…"))
            .await?;
        let request = zeph_skills::SkillGenerationRequest {
            description: description.to_owned(),
            category: None,
            allowed_tools: Vec::new(),
        };

        let mut generated = match generator.generate(request).await {
            Ok(g) => g,
            Err(e) => {
                self.channel
                    .send(&format!("Skill generation failed: {e}"))
                    .await?;
                return Ok(());
            }
        };

        // Dedup check: compare against existing registry embeddings.
        if let Some(ref matcher) = self.skill_state.matcher {
            let skill_text = format!("{} {}", generated.meta.description, generated.content);
            let all_meta_owned: Vec<zeph_skills::loader::SkillMeta> = {
                let registry = self.skill_state.registry.read().unwrap();
                registry.all_meta().into_iter().cloned().collect()
            };
            let all_meta_refs: Vec<&zeph_skills::loader::SkillMeta> =
                all_meta_owned.iter().collect();
            let embed_provider = self.embedding_provider.clone();
            let embed_fn = |text: &str| -> zeph_skills::matcher::EmbedFuture {
                let owned = text.to_owned();
                let p = embed_provider.clone();
                Box::pin(async move { p.embed(&owned).await })
            };
            let matches = matcher
                .match_skills(&all_meta_refs, &skill_text, 1, false, embed_fn)
                .await;
            if let Some(best) = matches.first()
                && best.score > 0.85
                && let Some(meta) = all_meta_refs.get(best.index)
            {
                generated.warnings.push(format!(
                    "Similar skill exists: **{}** (similarity: {:.2}). Consider using the existing skill instead.",
                    meta.name, best.score
                ));
            }
        }

        // Show preview.
        let mut preview = format!(
            "Generated skill **{}**:\n\n```\n{}\n```",
            generated.name, generated.content
        );
        if !generated.warnings.is_empty() {
            preview.push_str("\n\n**Warnings:**");
            for w in &generated.warnings {
                preview.push('\n');
                preview.push_str("- ");
                preview.push_str(w);
            }
        }
        let confirm_text = if generated.has_injection_patterns {
            "\n\nInjection patterns detected. Type **yes force** to save anyway, anything else to discard."
        } else {
            "\n\nType **yes** to save, anything else to discard."
        };
        preview.push_str(confirm_text);
        self.channel.send(&preview).await?;

        // Wait for confirmation.
        let reply = self.channel.recv().await;
        let confirmed = matches!(reply, Ok(Some(ref msg)) if {
            let trimmed = msg.text.trim();
            if generated.has_injection_patterns {
                trimmed.eq_ignore_ascii_case("yes force")
            } else {
                trimmed.eq_ignore_ascii_case("yes")
            }
        });

        if !confirmed {
            self.channel.send("Skill discarded.").await?;
            return Ok(());
        }

        // Save to disk.
        match generator.approve_and_save(&generated).await {
            Ok(path) => {
                // Register quarantined trust so hot-reload does not grant implicit trust.
                if let Some(ref memory) = self.memory_state.memory {
                    let _ = memory
                        .sqlite()
                        .set_skill_trust_level(&generated.name, "quarantined")
                        .await;
                }
                self.channel
                    .send(&format!(
                        "Skill **{}** saved to {}. It will be loaded automatically by hot-reload.",
                        generated.name,
                        path.display()
                    ))
                    .await?;
            }
            Err(e) => {
                self.channel
                    .send(&format!("Failed to save skill: {e}"))
                    .await?;
            }
        }

        Ok(())
    }

    async fn handle_skill_reject(
        &mut self,
        name: Option<&str>,
        reason_parts: &[&str],
    ) -> Result<(), super::super::error::AgentError> {
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

    async fn handle_skill_stats(&mut self) -> Result<(), super::super::error::AgentError> {
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
    ) -> Result<(), super::super::error::AgentError> {
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
    ) -> Result<(), super::super::error::AgentError> {
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
    ) -> Result<(), super::super::error::AgentError> {
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
    ) -> Result<(), super::super::error::AgentError> {
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
