// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Slash command helpers for `Agent<C>`.
//!
//! The `COMMANDS` constant has moved to `zeph-commands::commands`. This module now
//! contains only the `Agent<C>` helper methods used by the `AgentAccess` trait
//! implementations and the remaining un-migrated commands (`/skill`, `/skills`, `/feedback`).

use zeph_llm::provider::LlmProvider;

use super::Agent;
use super::error;

impl<C: crate::channel::Channel> Agent<C> {
    /// Handle built-in slash commands that short-circuit the main `run` loop.
    ///
    /// Returns `Some(true)` to break the loop (exit), `Some(false)` to continue to the next
    /// iteration, or `None` if the command was not recognized (caller should call
    /// `process_user_message`).
    ///
    /// Most commands are now handled through the session-registry or agent-registry. This
    /// method is kept for commands that could not be migrated due to non-Sync type constraints.
    #[allow(clippy::unused_self)]
    pub(super) fn handle_builtin_command(&self, _trimmed: &str) -> Option<bool> {
        None
    }

    /// Dispatch slash commands. Returns `Some(Ok(()))` when handled,
    /// `Some(Err(_))` on I/O error, `None` to fall through to LLM processing.
    ///
    /// Commands that remain here could not be migrated to the registry pattern because their
    /// implementations hold non-Send futures (references across `.await` points):
    /// - `/skill`, `/skills`, `/feedback` — non-Send DB references
    /// - `/compact` — `AnyProvider::embed/chat` creates `&AnyProvider` across await (HRTB)
    /// - `/mcp` — `RwLockWriteGuard`, `&[McpTool]`, and `McpToolRef<'_>` across await (HRTB)
    ///
    /// All other slash commands are dispatched through `session_registry` or `agent_registry`
    /// in `Agent::run`.
    pub(super) async fn dispatch_slash_command(
        &mut self,
        trimmed: &str,
    ) -> Option<Result<(), error::AgentError>> {
        macro_rules! handled {
            ($expr:expr) => {{
                if let Err(e) = $expr {
                    return Some(Err(e));
                }
                let _ = self.channel.flush_chunks().await;
                return Some(Ok(()));
            }};
        }

        let slash_urls = zeph_sanitizer::exfiltration::extract_flagged_urls(trimmed);
        if !slash_urls.is_empty() {
            self.security.user_provided_urls.write().extend(slash_urls);
        }

        if trimmed == "/compact" {
            let msg = if self.msg.messages.len() > self.context_manager.compaction_preserve_tail + 1
            {
                match self.compact_context().await {
                    Ok(
                        super::context::CompactionOutcome::Compacted
                        | super::context::CompactionOutcome::NoChange,
                    ) => "Context compacted successfully.".to_owned(),
                    Ok(super::context::CompactionOutcome::ProbeRejected) => {
                        "Compaction rejected: summary quality below threshold. \
                         Original context preserved."
                            .to_owned()
                    }
                    Err(e) => format!("Compaction failed: {e}"),
                }
            } else {
                "Nothing to compact.".to_owned()
            };
            handled!(self.channel.send(&msg).await.map_err(Into::into));
        }

        if trimmed == "/mcp" || trimmed.starts_with("/mcp ") {
            let args = trimmed.strip_prefix("/mcp").unwrap_or("").trim().to_owned();
            handled!(self.handle_mcp_command(&args).await);
        }

        if trimmed == "/skills" || trimmed.starts_with("/skills ") {
            let subcommand = trimmed.strip_prefix("/skills").unwrap_or("").trim();
            handled!(self.handle_skills_family(subcommand).await);
        }

        if trimmed == "/skill" || trimmed.starts_with("/skill ") {
            let rest = trimmed
                .strip_prefix("/skill")
                .unwrap_or("")
                .trim()
                .to_owned();
            handled!(self.handle_skill_command(&rest).await);
        }

        if trimmed == "/feedback" || trimmed.starts_with("/feedback ") {
            let rest = trimmed
                .strip_prefix("/feedback")
                .unwrap_or("")
                .trim()
                .to_owned();
            handled!(self.handle_feedback(&rest).await);
        }

        // @mention dispatch: not a `/` command, so not in the registry.
        if trimmed.starts_with('@') {
            return self.dispatch_agent_command(trimmed).await;
        }

        None
    }

    pub(super) async fn dispatch_agent_command(
        &mut self,
        trimmed: &str,
    ) -> Option<Result<(), error::AgentError>> {
        let known: Vec<String> = self
            .orchestration
            .subagent_manager
            .as_ref()
            .map(|m| m.definitions().iter().map(|d| d.name.clone()).collect())
            .unwrap_or_default();
        match zeph_subagent::AgentCommand::parse(trimmed, &known) {
            Ok(cmd) => {
                if let Some(msg) = self.handle_agent_command(cmd).await
                    && let Err(e) = self.channel.send(&msg).await
                {
                    return Some(Err(e.into()));
                }
                let _ = self.channel.flush_chunks().await;
                Some(Ok(()))
            }
            Err(e) if trimmed.starts_with('@') => {
                tracing::debug!("@mention not matched as agent: {e}");
                None
            }
            Err(e) => {
                if let Err(send_err) = self.channel.send(&e.to_string()).await {
                    return Some(Err(send_err.into()));
                }
                let _ = self.channel.flush_chunks().await;
                Some(Ok(()))
            }
        }
    }

    /// Return formatted session status string for use via [`AgentAccess::session_status`].
    #[allow(clippy::too_many_lines)]
    pub(super) fn handle_status_as_string(&mut self) -> String {
        use std::fmt::Write;
        use zeph_llm::provider::Role;

        let uptime = self.lifecycle.start_time.elapsed().as_secs();
        let msg_count = self
            .msg
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .count();

        let (
            api_calls,
            prompt_tokens,
            completion_tokens,
            cost_cents,
            mcp_servers,
            orch_plans,
            orch_tasks,
            orch_completed,
            orch_failed,
            orch_skipped,
            provider_breakdown,
        ) = if let Some(ref tx) = self.metrics.metrics_tx {
            let m = tx.borrow();
            (
                m.api_calls,
                m.prompt_tokens,
                m.completion_tokens,
                m.cost_spent_cents,
                m.mcp_server_count,
                m.orchestration.plans_total,
                m.orchestration.tasks_total,
                m.orchestration.tasks_completed,
                m.orchestration.tasks_failed,
                m.orchestration.tasks_skipped,
                m.provider_cost_breakdown.clone(),
            )
        } else {
            (0, 0, 0, 0.0, 0, 0, 0, 0, 0, 0, vec![])
        };

        let skill_count = self.skill_state.registry.read().all_meta().len();

        let mut out = String::from("Session status:\n\n");
        let _ = writeln!(out, "Provider:  {}", self.provider.name());
        let _ = writeln!(out, "Model:     {}", self.runtime.model_name);
        let _ = writeln!(out, "Uptime:    {uptime}s");
        let _ = writeln!(out, "Turns:     {msg_count}");
        let _ = writeln!(out, "API calls: {api_calls}");
        let _ = writeln!(
            out,
            "Tokens:    {prompt_tokens} prompt / {completion_tokens} completion"
        );
        let _ = writeln!(out, "Skills:    {skill_count}");
        let _ = writeln!(out, "MCP:       {mcp_servers} server(s)");
        if let Some(ref tf) = self.tool_state.tool_schema_filter {
            let _ = writeln!(
                out,
                "Filter:    enabled (top_k={}, always_on={}, {} embeddings)",
                tf.top_k(),
                tf.always_on_count(),
                tf.embedding_count(),
            );
        }
        if let Some(ref adv) = self.runtime.adversarial_policy_info {
            let provider_display = if adv.provider.is_empty() {
                "default"
            } else {
                adv.provider.as_str()
            };
            let _ = writeln!(
                out,
                "Adv gate:  enabled (provider={}, policies={}, fail_open={})",
                provider_display, adv.policy_count, adv.fail_open
            );
        }
        if cost_cents > 0.0 {
            let _ = writeln!(out, "Cost:      ${:.4}", cost_cents / 100.0);
            if !provider_breakdown.is_empty() {
                let _ = writeln!(
                    out,
                    "  {:<16} {:>8} {:>8} {:>8}",
                    "Provider", "Requests", "Tokens", "Cost"
                );
                for (name, usage) in &provider_breakdown {
                    let total_tokens = usage.input_tokens + usage.output_tokens;
                    let _ = writeln!(
                        out,
                        "  {:<16} {:>8} {:>8} {:>8}",
                        name,
                        usage.request_count,
                        total_tokens,
                        format!("${:.4}", usage.cost_cents / 100.0),
                    );
                }
            }
        }
        if orch_plans > 0 {
            let _ = writeln!(out);
            let _ = writeln!(out, "Orchestration:");
            let _ = writeln!(out, "  Plans:     {orch_plans}");
            let _ = writeln!(out, "  Tasks:     {orch_completed}/{orch_tasks} completed");
            if orch_failed > 0 {
                let _ = writeln!(out, "  Failed:    {orch_failed}");
            }
            if orch_skipped > 0 {
                let _ = writeln!(out, "  Skipped:   {orch_skipped}");
            }
        }

        {
            use crate::config::PruningStrategy;
            if matches!(
                self.context_manager.compression.pruning_strategy,
                PruningStrategy::Subgoal | PruningStrategy::SubgoalMig
            ) {
                let _ = writeln!(out);
                let _ = writeln!(
                    out,
                    "Pruning:   {}",
                    match self.context_manager.compression.pruning_strategy {
                        PruningStrategy::SubgoalMig => "subgoal_mig",
                        _ => "subgoal",
                    }
                );
                let subgoal_count = self.compression.subgoal_registry.subgoals.len();
                let _ = writeln!(out, "Subgoals:  {subgoal_count} tracked");
                if let Some(active) = self.compression.subgoal_registry.active_subgoal() {
                    let _ = writeln!(out, "Active:    \"{}\"", active.description);
                } else {
                    let _ = writeln!(out, "Active:    (none yet)");
                }
            }
        }

        let gc = &self.memory_state.extraction.graph_config;
        if gc.enabled {
            let _ = writeln!(out);
            if gc.spreading_activation.enabled {
                let _ = writeln!(
                    out,
                    "Graph recall: spreading activation (lambda={:.2}, hops={})",
                    gc.spreading_activation.decay_lambda, gc.spreading_activation.max_hops,
                );
            } else {
                let _ = writeln!(out, "Graph recall: BFS (hops={})", gc.max_hops,);
            }
        }

        out.trim_end().to_owned()
    }

    /// Return formatted guardrail status string for use via [`AgentAccess::guardrail_status`].
    pub(super) fn format_guardrail_status(&self) -> String {
        use std::fmt::Write;

        let mut out = String::new();
        if let Some(ref guardrail) = self.security.guardrail {
            let stats = guardrail.stats();
            let _ = writeln!(out, "Guardrail: enabled");
            let _ = writeln!(out, "Action:    {:?}", guardrail.action());
            let _ = writeln!(out, "Fail strategy: {:?}", guardrail.fail_strategy());
            let _ = writeln!(out, "Timeout:   {}ms", guardrail.timeout_ms());
            let _ = writeln!(
                out,
                "Tool scan: {}",
                if guardrail.scan_tool_output() {
                    "enabled"
                } else {
                    "disabled"
                }
            );
            let _ = writeln!(out, "\nStats:");
            let _ = writeln!(out, "  Total checks:  {}", stats.total_checks);
            let _ = writeln!(out, "  Flagged:       {}", stats.flagged_count);
            let _ = writeln!(out, "  Errors:        {}", stats.error_count);
            let _ = writeln!(out, "  Avg latency:   {}ms", stats.avg_latency_ms());
        } else {
            out.push_str("Guardrail: disabled\n");
            out.push_str(
                "Enable with: --guardrail flag or [security.guardrail] enabled = true in config",
            );
        }
        out.trim_end().to_owned()
    }

    /// Return formatted Focus Agent status string for use via [`AgentAccess::focus_status`].
    pub(super) fn format_focus_status(&self) -> String {
        use std::fmt::Write;
        let mut out = String::from("Focus Agent status\n\n");
        let _ = writeln!(out, "Enabled:          {}", self.focus.config.enabled);
        let _ = writeln!(out, "Active session:   {}", self.focus.is_active());
        if let Some(ref scope) = self.focus.active_scope {
            let _ = writeln!(out, "Active scope:     {scope}");
        }
        let _ = writeln!(
            out,
            "Knowledge blocks: {}",
            self.focus.knowledge_blocks.len()
        );
        let _ = writeln!(out, "Turns since focus: {}", self.focus.turns_since_focus);
        out.trim_end().to_owned()
    }

    /// Return formatted `SideQuest` eviction status string for use via
    /// [`AgentAccess::sidequest_status`].
    pub(super) fn format_sidequest_status(&self) -> String {
        use std::fmt::Write;
        let mut out = String::from("SideQuest status\n\n");
        let _ = writeln!(out, "Enabled:        {}", self.sidequest.config.enabled);
        let _ = writeln!(
            out,
            "Interval turns: {}",
            self.sidequest.config.interval_turns
        );
        let _ = writeln!(out, "Turn counter:   {}", self.sidequest.turn_counter);
        let _ = writeln!(out, "Passes run:     {}", self.sidequest.passes_run);
        let _ = writeln!(
            out,
            "Total evicted:  {} tool outputs",
            self.sidequest.total_evicted
        );
        out.trim_end().to_owned()
    }

    /// Load an image and return a status string for use via [`AgentAccess::load_image`].
    pub(super) fn handle_image_as_string(&mut self, path: &str) -> String {
        use std::path::Component;
        use zeph_llm::provider::{ImageData, MessagePart};

        let p = std::path::Path::new(path);
        if p.is_absolute() || p.components().any(|c| c == Component::ParentDir) {
            return "Invalid image path: path traversal not allowed".to_owned();
        }

        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) => return format!("Cannot read image {path}: {e}"),
        };
        if data.len() > super::message_queue::MAX_IMAGE_BYTES {
            return format!(
                "Image {path} exceeds size limit ({} MB), skipping",
                super::message_queue::MAX_IMAGE_BYTES / 1024 / 1024
            );
        }
        let mime_type = super::message_queue::detect_image_mime(Some(path)).to_string();
        self.msg
            .pending_image_parts
            .push(MessagePart::Image(Box::new(ImageData { data, mime_type })));
        format!("Image loaded: {path}. Send your message.")
    }

    pub(super) async fn handle_skills_family(
        &mut self,
        subcommand: &str,
    ) -> Result<(), error::AgentError> {
        match subcommand {
            "" => self.handle_skills_command().await,
            "confusability" => self.handle_skills_confusability_command().await,
            other => {
                self.channel
                    .send(&format!(
                        "Unknown /skills subcommand: '{other}'. Available: confusability"
                    ))
                    .await?;
                Ok(())
            }
        }
    }

    pub(super) async fn handle_skills_command(&mut self) -> Result<(), error::AgentError> {
        use std::collections::BTreeMap;
        use std::fmt::Write;

        let all_meta: Vec<zeph_skills::loader::SkillMeta> = self
            .skill_state
            .registry
            .read()
            .all_meta()
            .into_iter()
            .cloned()
            .collect();

        let mut trust_map: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for meta in &all_meta {
            if let Some(memory) = &self.memory_state.persistence.memory {
                let info = memory
                    .sqlite()
                    .load_skill_trust(&meta.name)
                    .await
                    .ok()
                    .flatten()
                    .map_or_else(String::new, |r| format!(" [{}]", r.trust_level));
                trust_map.insert(meta.name.clone(), info);
            }
        }

        let mut output = String::from("Available skills:\n\n");

        let has_categories = all_meta.iter().any(|m| m.category.is_some());
        if has_categories {
            let mut by_category: BTreeMap<&str, Vec<&zeph_skills::loader::SkillMeta>> =
                BTreeMap::new();
            for meta in &all_meta {
                let cat = meta.category.as_deref().unwrap_or("other");
                by_category.entry(cat).or_default().push(meta);
            }
            for (cat, skills) in &by_category {
                let _ = writeln!(output, "[{cat}]");
                for meta in skills {
                    let trust_info = trust_map.get(&meta.name).map_or("", String::as_str);
                    let _ = writeln!(output, "- {} — {}{trust_info}", meta.name, meta.description);
                }
                output.push('\n');
            }
        } else {
            for meta in &all_meta {
                let trust_info = trust_map.get(&meta.name).map_or("", String::as_str);
                let _ = writeln!(output, "- {} — {}{trust_info}", meta.name, meta.description);
            }
        }

        if let Some(memory) = &self.memory_state.persistence.memory {
            match memory.sqlite().load_skill_usage().await {
                Ok(usage) if !usage.is_empty() => {
                    output.push_str("\nUsage statistics:\n\n");
                    for row in &usage {
                        let _ = writeln!(
                            output,
                            "- {}: {} invocations (last: {})",
                            row.skill_name, row.invocation_count, row.last_used_at,
                        );
                    }
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("failed to load skill usage: {e:#}"),
            }
        }

        self.channel.send(&output).await?;
        Ok(())
    }

    pub(super) async fn handle_skills_confusability_command(
        &mut self,
    ) -> Result<(), error::AgentError> {
        let threshold = self.skill_state.confusability_threshold;
        if threshold <= 0.0 {
            self.channel
                .send(
                    "Confusability monitoring is disabled. \
                     Set [skills] confusability_threshold in config (e.g. 0.85) to enable.",
                )
                .await?;
            return Ok(());
        }

        let Some(matcher) = &self.skill_state.matcher else {
            self.channel
                .send("Skill matcher not available (no embedding provider configured).")
                .await?;
            return Ok(());
        };

        let all_meta: Vec<zeph_skills::loader::SkillMeta> = self
            .skill_state
            .registry
            .read()
            .all_meta()
            .into_iter()
            .cloned()
            .collect();
        let refs: Vec<&zeph_skills::loader::SkillMeta> = all_meta.iter().collect();

        let report = matcher.confusability_report(&refs, threshold).await;
        self.channel.send(&report.to_string()).await?;
        Ok(())
    }
}
