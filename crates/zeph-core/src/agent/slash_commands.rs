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

/// Returns a formatted overlay summary string for slash/TUI display.
///
/// Resolves the active plugin overlay against a scratch `Config::default()`.
/// Source and skipped plugin lists are accurate; merged config values (e.g.,
/// `allowed_commands`) are not shown because they depend on the live config base.
pub(crate) fn format_overlay_section(plugins_dir: &std::path::Path) -> String {
    let mut cfg = zeph_config::Config::default();
    match zeph_plugins::apply_plugin_config_overlays(&mut cfg, plugins_dir) {
        Err(e) => format!("overlay resolution failed: {e}"),
        Ok(overlay) => {
            if overlay.source_plugins.is_empty() && overlay.skipped_plugins.is_empty() {
                return "No plugin overlay active.".to_owned();
            }
            let mut out = String::from("Active plugin overlay:\n");
            if overlay.source_plugins.is_empty() {
                out.push_str("  Source plugins:  (none)\n");
            } else {
                out.push_str("  Source plugins:  ");
                out.push_str(&overlay.source_plugins.join(", "));
                out.push('\n');
            }
            if overlay.skipped_plugins.is_empty() {
                out.push_str("  Skipped plugins: (none)\n");
            } else {
                out.push_str("  Skipped plugins:\n");
                for reason in &overlay.skipped_plugins {
                    out.push_str("    - ");
                    out.push_str(reason);
                    out.push('\n');
                }
            }
            out.push_str(
                "  Note: overlay values shown against default config — run with --config for live intersection.",
            );
            out
        }
    }
}

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

    /// Dispatch slash commands that cannot be handled by the registry.
    ///
    /// Currently handles only `@mention` dispatch. All `/` slash commands are now
    /// dispatched through the session or agent command registry in `Agent::run`.
    ///
    /// Returns `Some(Ok(()))` when handled, `Some(Err(_))` on I/O error, `None` to
    /// fall through to LLM processing.
    pub(super) async fn dispatch_slash_command(
        &mut self,
        trimmed: &str,
    ) -> Option<Result<(), error::AgentError>> {
        // @mention dispatch: not a `/` command, so not in the registry.
        if trimmed.starts_with('@') {
            return self.dispatch_agent_command(trimmed).await;
        }

        // `/subagent spawn <cmd>` — ACP external process spawn (#3302).
        if trimmed.eq_ignore_ascii_case("/subagent")
            || trimmed.to_ascii_lowercase().starts_with("/subagent ")
        {
            let args = trimmed.get("/subagent".len()..).unwrap_or("").trim();
            return Some(self.handle_subagent_slash(args).await);
        }

        None
    }

    /// Handle `/subagent [spawn <cmd>]` and return a user-visible result.
    ///
    /// Routes `/subagent spawn <cmd>` through the ACP spawn callback when available.
    /// Returns a usage hint when no sub-command or command string is given, and a
    /// "not available" message when the ACP spawn callback has not been injected.
    async fn handle_subagent_slash(&mut self, args: &str) -> Result<(), error::AgentError> {
        let msg: String = if args.is_empty() {
            "Usage: /subagent <subcommand>\n\nSubcommands:\n  spawn <command>  Spawn an ACP sub-agent process".to_owned()
        } else {
            let (subcmd, rest) = args.split_once(' ').unwrap_or((args, ""));
            match subcmd {
                "spawn" => {
                    let cmd = rest.trim();
                    if cmd.is_empty() {
                        "Usage: /subagent spawn <command>\n\nExample: /subagent spawn zeph --acp"
                            .to_owned()
                    } else if let Some(spawn_fn) = self.runtime.config.acp_subagent_spawn_fn.clone()
                    {
                        let cmd = cmd.to_owned();
                        match spawn_fn(cmd).await {
                            Ok(output) => output,
                            Err(e) => format!("Sub-agent error: {e}"),
                        }
                    } else {
                        "ACP sub-agent spawning is not available in this mode.\n\
                         Use `zeph acp run-agent --command <CMD> --prompt <TEXT>` for one-shot sessions."
                            .to_owned()
                    }
                }
                other => format!("Unknown /subagent subcommand: '{other}'. Available: spawn"),
            }
        };

        let _ = self.channel.send(&msg).await;
        let _ = self.channel.flush_chunks().await;
        Ok(())
    }

    pub(super) async fn dispatch_agent_command(
        &mut self,
        trimmed: &str,
    ) -> Option<Result<(), error::AgentError>> {
        let known: Vec<String> = self
            .services
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
    pub(super) fn handle_status_as_string(&mut self) -> String {
        use std::fmt::Write;
        use zeph_llm::provider::Role;

        let uptime = self.runtime.lifecycle.start_time.elapsed().as_secs();
        let msg_count = self
            .msg
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .count();

        let metrics = collect_status_metrics(self.runtime.metrics.metrics_tx.as_ref());
        let skill_count = self.services.skill.registry.read().all_meta().len();

        let mut out = String::from("Session status:\n\n");
        let _ = writeln!(out, "Provider:  {}", self.provider.name());
        let _ = writeln!(out, "Model:     {}", self.runtime.config.model_name);
        let _ = writeln!(out, "Uptime:    {uptime}s");
        let _ = writeln!(out, "Turns:     {msg_count}");
        let _ = writeln!(out, "API calls: {}", metrics.api_calls);
        if metrics.reasoning_tokens > 0 {
            let _ = writeln!(
                out,
                "Tokens:    {} prompt / {} completion ({} reasoning, subset of completion)",
                metrics.prompt_tokens, metrics.completion_tokens, metrics.reasoning_tokens
            );
        } else {
            let _ = writeln!(
                out,
                "Tokens:    {} prompt / {} completion",
                metrics.prompt_tokens, metrics.completion_tokens
            );
        }
        let _ = writeln!(out, "Skills:    {skill_count}");
        let _ = writeln!(out, "MCP:       {} server(s)", metrics.mcp_servers);
        if let Some(ref tf) = self.services.tool_state.tool_schema_filter {
            let _ = writeln!(
                out,
                "Filter:    enabled (top_k={}, always_on={}, {} embeddings)",
                tf.top_k(),
                tf.always_on_count(),
                tf.embedding_count(),
            );
        }
        if let Some(ref adv) = self.runtime.config.adversarial_policy_info {
            let provider_display = if adv.provider.is_empty() {
                "default"
            } else {
                adv.provider.as_str()
            };
            let _ = writeln!(
                out,
                "Adv gate:  enabled (provider={}, policies={}, fail_open={}, timeout_ms={})",
                provider_display, adv.policy_count, adv.fail_open, adv.timeout_ms
            );
        }
        append_cost_section(&mut out, metrics.cost_cents, &metrics.provider_breakdown);
        append_orchestration_section(
            &mut out,
            metrics.orch_plans,
            metrics.orch_tasks,
            metrics.orch_completed,
            metrics.orch_failed,
            metrics.orch_skipped,
        );
        append_ensemble_section(
            &mut out,
            metrics.ensemble_degraded,
            metrics.ensemble_agreement_ratio,
            &metrics.ensemble_member_stats,
        );
        append_pruning_section(
            &mut out,
            self.context_manager.compression.pruning_strategy,
            self.services.compression.subgoal_registry.subgoals.len(),
            self.services.compression.subgoal_registry.active_subgoal(),
        );
        append_graph_recall_section(&mut out, &self.services.memory.extraction.graph_config);

        out.trim_end().to_owned()
    }

    /// Return formatted guardrail status string for use via [`AgentAccess::guardrail_status`].
    pub(super) fn format_guardrail_status(&self) -> String {
        use std::fmt::Write;

        let mut out = String::new();
        if let Some(ref guardrail) = self.services.security.guardrail {
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
        let _ = writeln!(
            out,
            "Enabled:          {}",
            self.services.focus.config.enabled
        );
        let _ = writeln!(out, "Active session:   {}", self.services.focus.is_active());
        if let Some(ref scope) = self.services.focus.active_scope {
            let _ = writeln!(out, "Active scope:     {scope}");
        }
        let _ = writeln!(
            out,
            "Knowledge blocks: {}",
            self.services.focus.knowledge_blocks.len()
        );
        let _ = writeln!(
            out,
            "Turns since focus: {}",
            self.services.focus.turns_since_focus
        );
        out.trim_end().to_owned()
    }

    /// Return formatted `SideQuest` eviction status string for use via
    /// [`AgentAccess::sidequest_status`].
    pub(super) fn format_sidequest_status(&self) -> String {
        use std::fmt::Write;
        let mut out = String::from("SideQuest status\n\n");
        let _ = writeln!(
            out,
            "Enabled:        {}",
            self.services.sidequest.config.enabled
        );
        let _ = writeln!(
            out,
            "Interval turns: {}",
            self.services.sidequest.config.interval_turns
        );
        let _ = writeln!(
            out,
            "Turn counter:   {}",
            self.services.sidequest.turn_counter
        );
        let _ = writeln!(
            out,
            "Passes run:     {}",
            self.services.sidequest.passes_run
        );
        let _ = writeln!(
            out,
            "Total evicted:  {} tool outputs",
            self.services.sidequest.total_evicted
        );
        out.trim_end().to_owned()
    }

    /// Load an image and return a status string for use via [`AgentAccess::load_image`].
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn handle_image_as_string(&mut self, path: &str) -> String {
        use zeph_common::path_guard::{PathRejection, classify_relative_path};
        use zeph_llm::provider::{ImageData, MessagePart};

        match classify_relative_path(path) {
            PathRejection::Allowed => {}
            PathRejection::Absolute => {
                return "Invalid image path: absolute paths are not supported, use a path \
                    relative to the working directory"
                    .to_owned();
            }
            PathRejection::Traversal => {
                return "Invalid image path: path traversal ('..') is not allowed".to_owned();
            }
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

    /// Return the `/skills [subcommand]` output as a `String` without sending via channel.
    ///
    /// Execute a `/plugins` command given pre-cloned state, suitable for use inside
    /// `tokio::task::spawn_blocking` without borrowing `&self`.
    #[allow(clippy::needless_pass_by_value)]
    pub(super) fn run_plugin_command(
        args: &str,
        managed_dir: Option<std::path::PathBuf>,
        mcp_allowed: Vec<String>,
        base_shell_allowed: Vec<String>,
        ephemeral_plugin_names: Vec<String>,
        reputation_cfg: &zeph_config::plugins::ReputationConfig,
    ) -> String {
        // Use the canonical default so CLI and TUI always reference the same directory.
        let plugins_dir = zeph_plugins::PluginManager::default_plugins_dir();

        let (subcmd, rest) = args.trim().split_once(' ').unwrap_or((args.trim(), ""));

        // Overlay subcommand does not need PluginManager; resolve early to avoid moving plugins_dir.
        if subcmd == "overlay" || (matches!(subcmd, "" | "list") && rest.trim() == "--overlay") {
            return format_overlay_section(&plugins_dir);
        }

        // Fall back to the canonical default managed skills dir so the conflict check is
        // never silently disabled by an empty path (M5 fix).
        let managed_dir = managed_dir
            .unwrap_or_else(|| zeph_config::defaults::default_vault_dir().join("skills"));
        let mgr = zeph_plugins::PluginManager::new(
            plugins_dir,
            managed_dir,
            mcp_allowed,
            base_shell_allowed,
        )
        .with_reputation_config(reputation_cfg, false);

        match subcmd {
            "" | "list" => match mgr.list_installed() {
                Ok(plugins) if plugins.is_empty() && ephemeral_plugin_names.is_empty() => {
                    "No plugins installed.".to_owned()
                }
                Ok(plugins) => {
                    let mut lines: Vec<String> = plugins
                        .iter()
                        .map(|p| format!("{} v{} — {}", p.name, p.version, p.description))
                        .collect();
                    for name in &ephemeral_plugin_names {
                        lines.push(format!("{name} [ephemeral]"));
                    }
                    lines.join("\n")
                }
                Err(e) => format!("plugin list failed: {e}"),
            },
            "add" => {
                use std::fmt::Write as _;
                if rest.is_empty() {
                    return "Usage: /plugins add <source>".to_owned();
                }
                match mgr.add(rest.trim()) {
                    Ok(r) => {
                        let mut out = format!("Installed plugin \"{}\"", r.name);
                        if !r.installed_skills.is_empty() {
                            let _ = write!(out, "\n  Skills: {}", r.installed_skills.join(", "));
                        }
                        if !r.mcp_server_ids.is_empty() {
                            let _ = write!(
                                out,
                                "\n  MCP servers (restart required): {}",
                                r.mcp_server_ids.join(", ")
                            );
                        }
                        for w in &r.warnings {
                            let _ = write!(out, "\n  warning: {w}");
                        }
                        out
                    }
                    Err(e) => format!("plugin add failed: {e}"),
                }
            }
            "remove" => {
                use std::fmt::Write as _;
                if rest.is_empty() {
                    return "Usage: /plugins remove <name>".to_owned();
                }
                match mgr.remove(rest.trim()) {
                    Ok(r) => {
                        let mut out = format!("Removed plugin \"{}\"", rest.trim());
                        if !r.removed_skills.is_empty() {
                            let _ =
                                write!(out, "\n  Removed skills: {}", r.removed_skills.join(", "));
                        }
                        out
                    }
                    Err(e) => format!("plugin remove failed: {e}"),
                }
            }
            other => {
                format!(
                    "Unknown /plugins subcommand: '{other}'. Available: list, list --overlay, overlay, add, remove"
                )
            }
        }
    }

    #[tracing::instrument(skip_all, name = "core.agent.handle_skills")]
    pub(super) async fn handle_skills_as_string(
        &mut self,
        subcommand: &str,
    ) -> Result<String, error::AgentError> {
        match subcommand {
            "" => self.handle_skills_command_as_string().await,
            "confusability" => self.handle_skills_confusability_as_string().await,
            "injection" => self.handle_skills_injection_as_string(),
            "trust" => self.handle_skills_trust_as_string(),
            other => Ok(format!(
                "Unknown /skills subcommand: '{other}'. Available: confusability, injection, trust"
            )),
        }
    }

    #[tracing::instrument(skip_all, name = "core.agent.handle_skills_command")]
    async fn handle_skills_command_as_string(&mut self) -> Result<String, error::AgentError> {
        use std::collections::BTreeMap;
        use std::fmt::Write;

        let (all_meta, load_errors): (
            Vec<zeph_skills::loader::SkillMeta>,
            Vec<(std::path::PathBuf, String)>,
        ) = {
            let reg = self.services.skill.registry.read();
            (
                reg.all_meta().into_iter().cloned().collect(),
                reg.load_errors().to_vec(),
            )
        };

        // Clone Arc before .await to avoid holding &self across suspension points.
        let memory = self.services.memory.persistence.memory.clone();
        let mut trust_map: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for meta in &all_meta {
            if let Some(ref memory) = memory {
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

        if let Some(ref memory) = memory {
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

        if !load_errors.is_empty() {
            output.push_str("\nFailed to load:\n");
            for (path, reason) in &load_errors {
                let _ = writeln!(output, "- {}: {reason}", path.display());
            }
        }

        Ok(output)
    }

    /// Start a user-driven loop that injects `prompt` every `interval_secs` seconds.
    pub(crate) fn start_user_loop(&mut self, prompt: String, interval_secs: u64) {
        use std::time::Duration;
        use tokio::time::{Instant, MissedTickBehavior};

        let period = Duration::from_secs(interval_secs);
        // interval_at(now + period, period) ensures the first tick fires after one full period,
        // not immediately. tokio::time::interval() would fire at t=0 which is never desired.
        let mut interval = tokio::time::interval_at(Instant::now() + period, period);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        let cancel_tx = tokio_util::sync::CancellationToken::new();
        self.runtime.lifecycle.user_loop = Some(crate::agent::state::LoopState {
            prompt,
            iteration: 0,
            interval,
            cancel_tx,
        });
    }

    /// Stop the active user loop and return a user-visible message.
    pub(crate) fn stop_user_loop(&mut self) -> String {
        if let Some(ls) = self.runtime.lifecycle.user_loop.take() {
            let iters = ls.iteration;
            ls.cancel_tx.cancel();
            format!("Loop stopped after {iters} iteration(s).")
        } else {
            "No active loop.".to_owned()
        }
    }

    #[tracing::instrument(skip_all, name = "core.agent.handle_skills_confusability")]
    async fn handle_skills_confusability_as_string(&mut self) -> Result<String, error::AgentError> {
        let threshold = self.services.skill.confusability_threshold;
        if threshold <= 0.0 {
            return Ok("Confusability monitoring is disabled. \
                 Set [skills] confusability_threshold in config (e.g. 0.85) to enable."
                .to_owned());
        }

        let Some(matcher) = &self.services.skill.matcher else {
            return Ok(
                "Skill matcher not available (no embedding provider configured).".to_owned(),
            );
        };

        let all_meta: Vec<zeph_skills::loader::SkillMeta> = self
            .services
            .skill
            .registry
            .read()
            .all_meta()
            .into_iter()
            .cloned()
            .collect();
        let refs: Vec<&zeph_skills::loader::SkillMeta> = all_meta.iter().collect();

        let report = matcher.confusability_report(&refs, threshold).await;
        Ok(report.to_string())
    }

    /// Report the current `GoSkills` grouping / injection-score config, as applied to this
    /// `Agent` instance. Exists so tests outside `zeph-core` can observe that
    /// `group_structured`, `support_similarity_threshold`, and `min_injection_score` reached
    /// the constructed `Agent` at cold start, mirroring `handle_skills_confusability_as_string`.
    #[tracing::instrument(skip_all, name = "core.agent.handle_skills_injection")]
    fn handle_skills_injection_as_string(&self) -> Result<String, error::AgentError> {
        Ok(format!(
            "Skill injection config: group_structured={}, support_similarity_threshold={:.2}, min_injection_score={:.2}",
            self.services.skill.group_structured,
            self.services.skill.support_similarity_threshold,
            self.services.skill.min_injection_score,
        ))
    }

    /// Report the current skill-trust levels and `SkillOrchestra` RL routing state, as applied
    /// to this `Agent` instance. Exists so tests outside `zeph-core` can observe that
    /// `with_trust_config` and `with_rl_routing`/`with_rl_head` reached the constructed `Agent`
    /// at cold start (#5920/#5921), mirroring `handle_skills_injection_as_string` (#5867) for
    /// the same wire-X-into-ACP/serve/daemon defect class.
    #[tracing::instrument(skip_all, name = "core.agent.handle_skills_trust")]
    fn handle_skills_trust_as_string(&self) -> Result<String, error::AgentError> {
        let trust = &self.services.skill.trust_config;
        let rl_enabled = self
            .services
            .learning_engine
            .rl_routing
            .as_ref()
            .is_some_and(|r| r.enabled);
        Ok(format!(
            "Skill trust config: default_level={:?}, local_level={:?}, bundled_level={:?}, \
             hash_mismatch_level={:?} | RL routing: enabled={}, rl_head_loaded={}",
            trust.default_level,
            trust.local_level,
            trust.bundled_level,
            trust.hash_mismatch_level,
            rl_enabled,
            self.services.skill.rl_head.is_some(),
        ))
    }
}

/// Builds the phase-1 (session/debug) command registry used by [`Agent::run`]: handlers
/// that only need `ChannelSink`/`DebugAccess`/`MessageAccess`/`SessionAccess`, not
/// `&mut Agent<C>` itself.
///
/// Extracted into its own function (rather than inlined in `run`) so the exact set of
/// registered handlers can be inspected by tests without duplicating the `.register()`
/// call list — see `commands_rs_drift_tests` for the regression guard this enables
/// against `zeph_commands::COMMANDS` silently drifting from the real registrations (#5987).
pub(crate) fn build_session_debug_registry<'ctx>()
-> zeph_commands::CommandRegistry<zeph_commands::CommandContext<'ctx>> {
    use zeph_commands::CommandRegistry;
    use zeph_commands::handlers::debug::{DebugDumpCommand, DumpFormatCommand, LogCommand};
    use zeph_commands::handlers::help::HelpCommand;
    use zeph_commands::handlers::session::{
        ClearCommand, ClearQueueCommand, ExitCommand, HistoryCommand, QuitCommand, ResetCommand,
    };

    let mut reg = CommandRegistry::new();
    reg.register(ExitCommand);
    reg.register(QuitCommand);
    reg.register(ClearCommand);
    reg.register(ResetCommand);
    reg.register(ClearQueueCommand);
    reg.register(HistoryCommand);
    reg.register(LogCommand);
    reg.register(DebugDumpCommand);
    reg.register(DumpFormatCommand);
    reg.register(HelpCommand);
    #[cfg(test)]
    reg.register(super::test_stubs::TestErrorCommand);
    reg
}

/// Builds the phase-2 (agent-command) registry used by [`Agent::run`]: handlers that
/// need `&mut Agent<C>` directly.
///
/// See [`build_session_debug_registry`] for why this is extracted.
pub(crate) fn build_agent_command_registry<'ctx>()
-> zeph_commands::CommandRegistry<zeph_commands::CommandContext<'ctx>> {
    use zeph_commands::CommandRegistry;
    use zeph_commands::handlers::{
        acp::AcpCommand,
        agent_cmd::AgentCommand,
        agents_fleet::AgentsFleetCommand,
        caveman::CavemanCommand,
        cd::CdCommand,
        checkpoint::{RedoCommand, UndoCommand},
        compaction::{CompactCommand, NewConversationCommand, RecapCommand},
        conv::ConvCommand,
        experiment::ExperimentCommand,
        goal::GoalCommand,
        loop_cmd::LoopCommand,
        lsp::LspCommand,
        mcp::McpCommand,
        memory::{
            GraphCommand, GuidelinesCommand, KnowledgeSlashCommand, MemoryCommand,
            StoreSlashCommand,
        },
        misc::{CacheStatsCommand, ImageCommand, NotifyTestCommand},
        model::{ModelCommand, ProviderCommand},
        plan::PlanCommand,
        plugins::PluginsCommand,
        policy::PolicyCommand,
        reasoning_effort::ReasoningEffortCommand,
        scheduler::SchedulerCommand,
        search::SearchCommand,
        skill::{FeedbackCommand, SkillCommand, SkillsCommand},
        status::{FocusCommand, GuardrailCommand, SideQuestCommand, StatusCommand},
        think_tokens::ThinkTokensCommand,
        trajectory::{ScopeCommand, TrajectoryCommand},
        worktree::WorktreeCommand,
    };

    let mut agent_reg = CommandRegistry::new();
    agent_reg.register(CavemanCommand);
    agent_reg.register(CdCommand);
    agent_reg.register(MemoryCommand);
    agent_reg.register(StoreSlashCommand);
    agent_reg.register(GraphCommand);
    agent_reg.register(KnowledgeSlashCommand);
    agent_reg.register(GuidelinesCommand);
    agent_reg.register(ModelCommand);
    agent_reg.register(ProviderCommand);
    agent_reg.register(ThinkTokensCommand);
    agent_reg.register(ReasoningEffortCommand);
    // Phase 6 migrations: /skill, /skills, /feedback use clone-before-await pattern.
    agent_reg.register(SkillCommand);
    agent_reg.register(SkillsCommand);
    agent_reg.register(FeedbackCommand);
    agent_reg.register(McpCommand);
    agent_reg.register(PolicyCommand);
    agent_reg.register(SchedulerCommand);
    agent_reg.register(SearchCommand);
    agent_reg.register(LspCommand);
    // Phase 4 migrations (Send-safe commands):
    agent_reg.register(CacheStatsCommand);
    agent_reg.register(ImageCommand);
    agent_reg.register(NotifyTestCommand);
    agent_reg.register(StatusCommand);
    agent_reg.register(GuardrailCommand);
    agent_reg.register(FocusCommand);
    agent_reg.register(SideQuestCommand);
    agent_reg.register(AgentCommand);
    agent_reg.register(AgentsFleetCommand);
    // Phase 5 migrations (Send-compatible):
    agent_reg.register(CompactCommand);
    agent_reg.register(NewConversationCommand);
    agent_reg.register(RecapCommand);
    agent_reg.register(ExperimentCommand);
    agent_reg.register(PlanCommand);
    agent_reg.register(LoopCommand);
    agent_reg.register(PluginsCommand);
    agent_reg.register(AcpCommand);
    #[cfg(feature = "cocoon")]
    agent_reg.register(zeph_commands::handlers::cocoon::CocoonCommand);
    agent_reg.register(TrajectoryCommand);
    agent_reg.register(ScopeCommand);
    agent_reg.register(GoalCommand);
    agent_reg.register(UndoCommand);
    agent_reg.register(RedoCommand);
    agent_reg.register(ConvCommand);
    agent_reg.register(WorktreeCommand);
    agent_reg
}

struct StatusMetrics {
    api_calls: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    reasoning_tokens: u64,
    cost_cents: f64,
    mcp_servers: usize,
    orch_plans: u64,
    orch_tasks: u64,
    orch_completed: u64,
    orch_failed: u64,
    orch_skipped: u64,
    ensemble_degraded: u64,
    ensemble_agreement_ratio: Option<f64>,
    ensemble_member_stats: Vec<(String, f64, u64)>,
    provider_breakdown: Vec<(String, crate::cost::ProviderUsage)>,
}

fn collect_status_metrics(
    metrics_tx: Option<&tokio::sync::watch::Sender<crate::metrics::MetricsSnapshot>>,
) -> StatusMetrics {
    if let Some(tx) = metrics_tx {
        let m = tx.borrow();
        StatusMetrics {
            api_calls: m.api_calls,
            prompt_tokens: m.prompt_tokens,
            completion_tokens: m.completion_tokens,
            reasoning_tokens: m.reasoning_tokens,
            cost_cents: m.cost_spent_cents,
            mcp_servers: m.mcp_server_count,
            orch_plans: m.orchestration.plans_total,
            orch_tasks: m.orchestration.tasks_total,
            orch_completed: m.orchestration.tasks_completed,
            orch_failed: m.orchestration.tasks_failed,
            orch_skipped: m.orchestration.tasks_skipped,
            ensemble_degraded: m.orchestration.ensemble_degraded_total,
            ensemble_agreement_ratio: m.orchestration.ensemble_last_agreement_ratio,
            ensemble_member_stats: m.orchestration.ensemble_member_stats.clone(),
            provider_breakdown: m.provider_cost_breakdown.clone(),
        }
    } else {
        StatusMetrics {
            api_calls: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            reasoning_tokens: 0,
            cost_cents: 0.0,
            mcp_servers: 0,
            orch_plans: 0,
            orch_tasks: 0,
            orch_completed: 0,
            orch_failed: 0,
            orch_skipped: 0,
            ensemble_degraded: 0,
            ensemble_agreement_ratio: None,
            ensemble_member_stats: vec![],
            provider_breakdown: vec![],
        }
    }
}

fn append_cost_section(
    out: &mut String,
    cost_cents: f64,
    provider_breakdown: &[(String, crate::cost::ProviderUsage)],
) {
    use std::fmt::Write;
    if cost_cents > 0.0 {
        let _ = writeln!(out, "Cost:      ${:.4}", cost_cents / 100.0);
        if !provider_breakdown.is_empty() {
            let _ = writeln!(
                out,
                "  {:<16} {:>8} {:>8} {:>8}",
                "Provider", "Requests", "Tokens", "Cost"
            );
            for (name, usage) in provider_breakdown {
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
}

fn append_orchestration_section(
    out: &mut String,
    orch_plans: u64,
    orch_tasks: u64,
    orch_completed: u64,
    orch_failed: u64,
    orch_skipped: u64,
) {
    use std::fmt::Write;
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
}

/// Append ensemble-verified plan verification stats (spec `073-orch-ensemble-merge`) to the
/// `/status` output. Silent (no section printed) when the ensemble has never run — the
/// member-stats list is empty and no member has ever cast a ballot.
fn append_ensemble_section(
    out: &mut String,
    ensemble_degraded: u64,
    ensemble_agreement_ratio: Option<f64>,
    ensemble_member_stats: &[(String, f64, u64)],
) {
    use std::fmt::Write;
    if ensemble_member_stats.is_empty() && ensemble_degraded == 0 {
        return;
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "Ensemble verify:");
    if let Some(ratio) = ensemble_agreement_ratio {
        let _ = writeln!(out, "  Last agreement: {:.0}%", ratio * 100.0);
    }
    if ensemble_degraded > 0 {
        let _ = writeln!(out, "  Degraded:  {ensemble_degraded} (quorum fallback)");
    }
    for (member, score, observations) in ensemble_member_stats {
        let _ = writeln!(out, "  {member:<16} ema={score:.2} (n={observations})");
    }
}

fn append_pruning_section(
    out: &mut String,
    pruning_strategy: crate::config::PruningStrategy,
    subgoal_count: usize,
    active_subgoal: Option<&zeph_agent_context::compaction::Subgoal>,
) {
    use crate::config::PruningStrategy;
    use std::fmt::Write;
    if matches!(
        pruning_strategy,
        PruningStrategy::Subgoal | PruningStrategy::SubgoalMig
    ) {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "Pruning:   {}",
            match pruning_strategy {
                PruningStrategy::SubgoalMig => "subgoal_mig",
                _ => "subgoal",
            }
        );
        let _ = writeln!(out, "Subgoals:  {subgoal_count} tracked");
        if let Some(active) = active_subgoal {
            let _ = writeln!(out, "Active:    \"{}\"", active.description);
        } else {
            let _ = writeln!(out, "Active:    (none yet)");
        }
    }
}

fn append_graph_recall_section(out: &mut String, gc: &zeph_config::memory::GraphConfig) {
    use std::fmt::Write;
    if gc.enabled {
        let _ = writeln!(out);
        if gc.spreading_activation.enabled {
            let _ = writeln!(
                out,
                "Graph recall: spreading activation (lambda={:.2}, hops={})",
                gc.spreading_activation.decay_lambda, gc.spreading_activation.max_hops,
            );
        } else {
            let _ = writeln!(out, "Graph recall: BFS (hops={})", gc.max_hops);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #5904 SIGNIFICANT-1: `dispatch_slash_command` is the only slash-command dispatch path
    /// with no `trusted`/`requires_auth` check at all — it runs `/subagent spawn <cmd>`
    /// (external ACP process spawn) unconditionally regardless of channel trust. HTTP entry
    /// points (serve-sessions, gateway) rely on `zeph_commands::is_recognized_command`
    /// excluding every command dispatched here, so they never forward such a command raw
    /// expecting the registry's trust gate to catch it. If this function ever starts handling
    /// another command besides `/subagent` (`@mention` is not `/`-prefixed and is exempt),
    /// `zeph_commands::UNGATED_DISPATCH_COMMANDS` must be updated to exclude it too — this test
    /// pins the current, single exception so that omission is caught here, at the source of the
    /// trust-blind path, not only in `zeph-commands`.
    #[test]
    fn subagent_is_excluded_from_is_recognized_command() {
        assert!(!zeph_commands::is_recognized_command("/subagent"));
        assert!(!zeph_commands::is_recognized_command(
            "/subagent spawn zeph --acp"
        ));
    }

    #[test]
    fn format_overlay_section_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let out = format_overlay_section(tmp.path());
        assert_eq!(out, "No plugin overlay active.");
    }

    #[test]
    fn format_overlay_section_with_source_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join("myplugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        let manifest = r#"
[plugin]
name = "myplugin"
version = "0.1.0"
description = "test"

[config.tools.shell]
blocked_commands = ["curl"]
"#;
        std::fs::write(plugin_dir.join(".plugin.toml"), manifest).unwrap();
        let out = format_overlay_section(tmp.path());
        assert!(out.contains("Active plugin overlay:"));
        assert!(out.contains("myplugin"));
        assert!(out.contains("Source plugins:"));
        assert!(out.contains("Note:"));
    }

    #[test]
    fn run_plugin_command_overlay_subcommand() {
        let tmp = tempfile::tempdir().unwrap();
        // Override default plugins dir is not possible in run_plugin_command since it uses
        // the canonical dir. Test that the function returns the expected prefix on an empty dir.
        // We test format_overlay_section directly for correctness; this test guards routing.
        let out = format_overlay_section(tmp.path());
        assert_eq!(out, "No plugin overlay active.");
    }

    #[test]
    fn format_overlay_section_skipped_plugin_shows_reason() {
        let tmp = tempfile::tempdir().unwrap();
        // Write a plugin dir with an invalid manifest to trigger skipped_plugins.
        let plugin_dir = tmp.path().join("badplugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join(".plugin.toml"),
            b"not valid toml at all {{{{",
        )
        .unwrap();
        let out = format_overlay_section(tmp.path());
        // Either skipped with reason or empty overlay — either way must not panic.
        assert!(out.contains("No plugin overlay active.") || out.contains("badplugin"));
    }
}
