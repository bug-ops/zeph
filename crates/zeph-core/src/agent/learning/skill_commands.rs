// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt::Write as _;

use super::super::{Agent, Channel, LlmProvider};
use super::background::write_skill_file;

impl<C: Channel> Agent<C> {
    /// Return the `/skill [subcommand]` output as a `String` without sending via channel.
    ///
    /// Used by the `AgentAccess::handle_skill` implementation to satisfy the `Send` bound
    /// on the returned future.
    pub(crate) async fn handle_skill_command_as_string(
        &mut self,
        args: &str,
    ) -> Result<String, super::super::error::AgentError> {
        let parts: Vec<&str> = args.split_whitespace().collect();
        match parts.first().copied() {
            Some("stats") => self.handle_skill_stats_as_string().await,
            Some("versions") => self.handle_skill_versions_as_string(parts.get(1).copied()).await,
            Some("activate") => {
                self.handle_skill_activate_as_string(
                    parts.get(1).copied(),
                    parts.get(2).copied(),
                )
                .await
            }
            Some("approve") => {
                self.handle_skill_approve_as_string(parts.get(1).copied())
                    .await
            }
            Some("reset") => {
                self.handle_skill_reset_as_string(parts.get(1).copied())
                    .await
            }
            Some("trust") => self.handle_skill_trust_command_as_string(&parts[1..]).await,
            Some("block") => {
                self.handle_skill_block_as_string(parts.get(1).copied())
                    .await
            }
            Some("unblock") => {
                self.handle_skill_unblock_as_string(parts.get(1).copied())
                    .await
            }
            Some("install") => {
                self.handle_skill_install_as_string(parts.get(1).copied())
                    .await
            }
            Some("remove") => {
                self.handle_skill_remove_as_string(parts.get(1).copied())
                    .await
            }
            Some("create") => {
                let description = parts[1..].join(" ");
                self.handle_skill_create_as_string(&description).await
            }
            Some("scan") => Ok(self.handle_skill_scan_as_string()),
            Some("reject") => {
                let tail = if parts.len() > 2 { &parts[2..] } else { &[] };
                self.handle_skill_reject_as_string(parts.get(1).copied(), tail)
                    .await
            }
            _ => Ok(
                "Unknown /skill subcommand. Available: stats, versions, activate, approve, reset, trust, block, unblock, install, remove, reject, scan, create".to_owned()
            ),
        }
    }

    /// Handle `/skill create <description>` — generate a SKILL.md via LLM and auto-save it.
    ///
    /// Non-interactive: the skill is saved immediately with quarantined trust level.
    /// The generated preview is returned in the output so the user can review it.
    /// Use `/skill remove <name>` to discard an unwanted skill.
    #[allow(clippy::too_many_lines)] // long function; decomposition would require extracting state into additional structs — TODO(review): file a tracking issue for this decomposition
    async fn handle_skill_create_as_string(
        &mut self,
        description: &str,
    ) -> Result<String, super::super::error::AgentError> {
        if description.trim().is_empty() {
            return Ok(
                "Usage: /skill create <description>\n\nExample:\n  /skill create fetch weather data from wttr.in and display current conditions".to_owned()
            );
        }

        if description.chars().count() > 2048 {
            return Ok("Description too long (max 2048 characters).".to_owned());
        }

        let input_scan = zeph_skills::scanner::scan_skill_body(description);
        if input_scan.has_matches() {
            return Ok("Input blocked: injection patterns detected in description.".to_owned());
        }

        // Determine output directory: generation_output_dir > managed_dir > first skill_path.
        let output_dir = if let Some(ref dir) = self.skill_state.generation_output_dir {
            dir.clone()
        } else if let Some(ref dir) = self.skill_state.managed_dir {
            dir.clone()
        } else if let Some(first) = self.skill_state.skill_paths.first() {
            first.clone()
        } else {
            return Ok(
                "No skill output directory configured. Set skills.generation_output_dir or skills.paths.".to_owned()
            );
        };

        let mut output = String::new();

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
            let _ = write!(
                output,
                "Warning: {} is not listed in skills.paths. The generated skill may not be hot-reloaded automatically.\n\n",
                output_dir.display()
            );
        }

        let generation_provider =
            self.resolve_background_provider(&self.skill_state.generation_provider_name.clone());
        let generator = zeph_skills::SkillGenerator::new(generation_provider, output_dir.clone());
        let generator = if let Some(ref eval) = self.skill_state.skill_evaluator {
            generator.with_evaluator(
                std::sync::Arc::clone(eval),
                self.skill_state.eval_weights,
                self.skill_state.eval_threshold,
            )
        } else {
            generator
        };
        let request = zeph_skills::SkillGenerationRequest {
            description: description.to_owned(),
            category: None,
            allowed_tools: Vec::new(),
        };

        let mut generated = match generator.generate(request).await {
            Ok(g) => g,
            Err(e) => return Ok(format!("Skill generation failed: {e}")),
        };

        // Dedup check: compare against existing registry embeddings.
        if let Some(ref matcher) = self.skill_state.matcher {
            let skill_text = format!("{} {}", generated.meta.description, generated.content);
            let all_meta_owned: Vec<zeph_skills::loader::SkillMeta> = {
                let registry = self.skill_state.registry.read();
                registry.all_meta().into_iter().cloned().collect()
            };
            let all_meta_refs: Vec<&zeph_skills::loader::SkillMeta> =
                all_meta_owned.iter().collect();
            let embed_provider = self.embedding_provider.clone();
            let embed_fn = move |text: &str| -> zeph_skills::matcher::EmbedFuture {
                let owned = text.to_owned();
                let p = embed_provider.clone();
                Box::pin(async move { p.embed(&owned).await })
            };
            let matches = match matcher
                .match_skills(&all_meta_refs, &skill_text, 1, false, embed_fn)
                .await
            {
                zeph_skills::MatchResult::Scored(v) => v,
                zeph_skills::MatchResult::InfraError => Vec::new(),
            };
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

        if generated.has_injection_patterns {
            output.push_str("Injection patterns detected in generated skill. Skipping save.\n\n");
            let _ = write!(
                output,
                "Generated skill **{}** (NOT saved):\n\n```\n{}\n```",
                generated.name, generated.content
            );
            if !generated.warnings.is_empty() {
                output.push_str("\n\n**Warnings:**");
                for w in &generated.warnings {
                    output.push('\n');
                    output.push_str("- ");
                    output.push_str(w);
                }
            }
            return Ok(output);
        }

        // Auto-save with quarantined trust (non-interactive path).
        match generator.approve_and_save(&generated).await {
            Ok(path) => {
                // Register quarantined trust so hot-reload does not grant implicit trust.
                if let Some(ref memory) = self.memory_state.persistence.memory {
                    let _ = memory
                        .sqlite()
                        .set_skill_trust_level(&generated.name, "quarantined")
                        .await;
                }
                let _ = write!(
                    output,
                    "Generated skill **{}**:\n\n```\n{}\n```",
                    generated.name, generated.content
                );
                if !generated.warnings.is_empty() {
                    output.push_str("\n\n**Warnings:**");
                    for w in &generated.warnings {
                        output.push('\n');
                        output.push_str("- ");
                        output.push_str(w);
                    }
                }
                let _ = write!(
                    output,
                    "\n\nAuto-saved to {} with quarantined trust. Use `/skill remove {}` to discard.",
                    path.display(),
                    generated.name,
                );
            }
            Err(e) => {
                let _ = write!(output, "Failed to save skill: {e}");
            }
        }

        Ok(output)
    }

    async fn handle_skill_reject_as_string(
        &mut self,
        name: Option<&str>,
        reason_parts: &[&str],
    ) -> Result<String, super::super::error::AgentError> {
        let Some(name) = name else {
            return Ok("Usage: /skill reject <name> <reason>".to_owned());
        };
        // SEC-PH1-001: validate skill exists in registry before writing to DB
        if self.skill_state.registry.read().skill(name).is_err() {
            return Ok(format!("Unknown skill: \"{name}\"."));
        }
        let reason = reason_parts.join(" ");
        if reason.is_empty() {
            return Ok("Usage: /skill reject <name> <reason>".to_owned());
        }
        // SEC-PH1-002: cap reason length to prevent oversized LLM prompts
        let reason = if reason.len() > 500 {
            reason[..500].to_string()
        } else {
            reason
        };
        // Clone Arc before .await to avoid holding &self across suspension points.
        let memory = self.memory_state.persistence.memory.clone();
        let Some(memory) = memory else {
            return Ok("Memory not available.".to_owned());
        };
        let conversation_id = self.memory_state.persistence.conversation_id;
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
                conversation_id,
                "user_rejection",
                Some(&reason),
                Some("user_rejection"), // REV-002: structured outcome_detail
            )
            .await?;
        // Note: generate_improved_skill is intentionally not called here to keep this future
        // Send-compatible. The original handle_skill_command (non-_as_string) still triggers
        // learning for the reject subcommand when dispatched via dispatch_slash_command or tests.
        Ok(format!("Rejection recorded for \"{name}\"."))
    }

    async fn handle_skill_stats_as_string(
        &mut self,
    ) -> Result<String, super::super::error::AgentError> {
        use std::fmt::Write;

        let memory = self.memory_state.persistence.memory.clone();
        let Some(memory) = memory else {
            return Ok("Memory not available.".to_owned());
        };

        let stats = memory.sqlite().load_skill_outcome_stats().await?;
        if stats.is_empty() {
            return Ok("No skill outcome data yet.".to_owned());
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

        Ok(output)
    }

    async fn handle_skill_versions_as_string(
        &mut self,
        name: Option<&str>,
    ) -> Result<String, super::super::error::AgentError> {
        use std::fmt::Write;

        let Some(name) = name else {
            return Ok("Usage: /skill versions <name>".to_owned());
        };
        let memory = self.memory_state.persistence.memory.clone();
        let Some(memory) = memory else {
            return Ok("Memory not available.".to_owned());
        };

        let versions = memory.sqlite().load_skill_versions(name).await?;
        if versions.is_empty() {
            return Ok(format!("No versions found for \"{name}\"."));
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

        Ok(output)
    }

    async fn handle_skill_activate_as_string(
        &mut self,
        name: Option<&str>,
        version_str: Option<&str>,
    ) -> Result<String, super::super::error::AgentError> {
        let (Some(name), Some(ver_str)) = (name, version_str) else {
            return Ok("Usage: /skill activate <name> <version>".to_owned());
        };
        let Ok(ver) = ver_str.parse::<i64>() else {
            return Ok("Invalid version number.".to_owned());
        };
        // Clone Arc before .await to avoid holding &self across suspension points.
        let memory = self.memory_state.persistence.memory.clone();
        let Some(memory) = memory else {
            return Ok("Memory not available.".to_owned());
        };

        let versions = memory.sqlite().load_skill_versions(name).await?;
        // Clone target fields to avoid holding &SkillVersionRow across .await.
        let target_opt = versions
            .iter()
            .find(|v| v.version == ver)
            .map(|v| (v.id, v.description.clone(), v.body.clone()));
        let Some((target_id, target_desc, target_body)) = target_opt else {
            return Ok(format!("Version {ver} not found for \"{name}\"."));
        };

        memory
            .sqlite()
            .activate_skill_version(name, target_id)
            .await?;

        write_skill_file(
            &self.skill_state.skill_paths,
            name,
            &target_desc,
            &target_body,
        )
        .await?;

        Ok(format!("Activated v{ver} for \"{name}\"."))
    }

    async fn handle_skill_approve_as_string(
        &mut self,
        name: Option<&str>,
    ) -> Result<String, super::super::error::AgentError> {
        let Some(name) = name else {
            return Ok("Usage: /skill approve <name>".to_owned());
        };
        // Clone Arc before .await to avoid holding &self across suspension points.
        let memory = self.memory_state.persistence.memory.clone();
        let Some(memory) = memory else {
            return Ok("Memory not available.".to_owned());
        };

        let versions = memory.sqlite().load_skill_versions(name).await?;
        // Clone target fields to avoid holding &SkillVersionRow across .await.
        let pending_opt = versions
            .iter()
            .rfind(|v| v.source == "auto" && !v.is_active)
            .map(|v| (v.id, v.version, v.description.clone(), v.body.clone()));

        let Some((target_id, target_ver, target_desc, target_body)) = pending_opt else {
            return Ok(format!("No pending auto version for \"{name}\"."));
        };

        memory
            .sqlite()
            .activate_skill_version(name, target_id)
            .await?;

        write_skill_file(
            &self.skill_state.skill_paths,
            name,
            &target_desc,
            &target_body,
        )
        .await?;

        Ok(format!(
            "Approved and activated v{target_ver} for \"{name}\"."
        ))
    }

    async fn handle_skill_reset_as_string(
        &mut self,
        name: Option<&str>,
    ) -> Result<String, super::super::error::AgentError> {
        let Some(name) = name else {
            return Ok("Usage: /skill reset <name>".to_owned());
        };
        // Clone Arc before .await to avoid holding &self across suspension points.
        let memory = self.memory_state.persistence.memory.clone();
        let Some(memory) = memory else {
            return Ok("Memory not available.".to_owned());
        };

        let versions = memory.sqlite().load_skill_versions(name).await?;
        // Clone v1 fields to avoid holding &SkillVersionRow across .await.
        let v1_opt = versions
            .iter()
            .find(|v| v.version == 1)
            .map(|v| (v.id, v.description.clone(), v.body.clone()));
        let Some((v1_id, v1_desc, v1_body)) = v1_opt else {
            return Ok(format!("Original version not found for \"{name}\"."));
        };

        memory.sqlite().activate_skill_version(name, v1_id).await?;

        write_skill_file(&self.skill_state.skill_paths, name, &v1_desc, &v1_body).await?;

        Ok(format!("Reset \"{name}\" to original v1."))
    }
}
