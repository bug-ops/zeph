// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Static registry of all slash commands available in the agent loop.
//!
//! Used by `/help` to enumerate and display commands grouped by category.

/// Broad grouping for displaying commands in `/help` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCategory {
    Session,
    Model,
    Info,
    Memory,
    Tools,
    Debug,
    Planning,
    Advanced,
}

impl SlashCategory {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "Session",
            Self::Model => "Model",
            Self::Info => "Info",
            Self::Memory => "Memory",
            Self::Tools => "Tools",
            Self::Debug => "Debug",
            Self::Planning => "Planning",
            Self::Advanced => "Advanced",
        }
    }
}

/// Metadata for a single slash command displayed by `/help`.
pub struct SlashCommandInfo {
    pub name: &'static str,
    /// Argument hint shown after the command name, e.g. `[path]` or `<name>`.
    pub args: &'static str,
    pub description: &'static str,
    pub category: SlashCategory,
    /// When `Some`, this entry was compiled in only for that feature.
    /// Shown in the help output as `[requires: <feature>]`.
    pub feature_gate: Option<&'static str>,
}

/// All slash commands recognised by the agent loop, in display order.
///
/// Feature-gated entries are wrapped in `#[cfg(feature = "...")]` so that
/// only commands compiled into the binary appear in `/help`.
pub const COMMANDS: &[SlashCommandInfo] = &[
    // --- Info ---
    SlashCommandInfo {
        name: "/help",
        args: "",
        description: "Show this help message",
        category: SlashCategory::Info,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/status",
        args: "",
        description: "Show current session status (provider, model, tokens, uptime)",
        category: SlashCategory::Info,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/skills",
        args: "",
        description: "List loaded skills (grouped by category when available)",
        category: SlashCategory::Info,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/skills confusability",
        args: "",
        description: "Show skill pairs with high embedding similarity (potential disambiguation failures)",
        category: SlashCategory::Info,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/guardrail",
        args: "",
        description: "Show guardrail status (provider, model, action, timeout, stats)",
        category: SlashCategory::Info,
        feature_gate: Some("guardrail"),
    },
    SlashCommandInfo {
        name: "/log",
        args: "",
        description: "Toggle verbose log output",
        category: SlashCategory::Info,
        feature_gate: None,
    },
    // --- Session ---
    SlashCommandInfo {
        name: "/exit",
        args: "",
        description: "Exit the agent (also: /quit)",
        category: SlashCategory::Session,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/new",
        args: "[--no-digest] [--keep-plan]",
        description: "Start a new conversation (reset context, preserve memory and MCP)",
        category: SlashCategory::Session,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/clear",
        args: "",
        description: "Clear conversation history",
        category: SlashCategory::Session,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/reset",
        args: "",
        description: "Reset conversation history (alias for /clear, replies with confirmation)",
        category: SlashCategory::Session,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/clear-queue",
        args: "",
        description: "Discard queued messages",
        category: SlashCategory::Session,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/compact",
        args: "",
        description: "Compact the context window",
        category: SlashCategory::Session,
        feature_gate: None,
    },
    // --- Model ---
    SlashCommandInfo {
        name: "/model",
        args: "[id|refresh]",
        description: "Show or switch the active model",
        category: SlashCategory::Model,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/provider",
        args: "[name|status]",
        description: "List configured providers or switch to one by name",
        category: SlashCategory::Model,
        feature_gate: None,
    },
    // --- Memory ---
    SlashCommandInfo {
        name: "/feedback",
        args: "<skill> <message>",
        description: "Submit feedback for a skill",
        category: SlashCategory::Memory,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/graph",
        args: "[subcommand]",
        description: "Query or manage the knowledge graph",
        category: SlashCategory::Memory,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/memory",
        args: "[tiers|promote <id>...]",
        description: "Show memory tier stats or manually promote messages to semantic tier",
        category: SlashCategory::Memory,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/guidelines",
        args: "",
        description: "Show current compression guidelines",
        category: SlashCategory::Memory,
        feature_gate: Some("compression-guidelines"),
    },
    // --- Tools ---
    SlashCommandInfo {
        name: "/skill",
        args: "<name>",
        description: "Load and display a skill body",
        category: SlashCategory::Tools,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/skill create",
        args: "<description>",
        description: "Generate a SKILL.md from natural language via LLM",
        category: SlashCategory::Tools,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/mcp",
        args: "[add|list|tools|remove]",
        description: "Manage MCP servers",
        category: SlashCategory::Tools,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/image",
        args: "<path>",
        description: "Attach an image to the next message",
        category: SlashCategory::Tools,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/agent",
        args: "[subcommand]",
        description: "Manage sub-agents",
        category: SlashCategory::Tools,
        feature_gate: None,
    },
    // --- Planning ---
    SlashCommandInfo {
        name: "/plan",
        args: "[goal|confirm|cancel|status|list|resume|retry]",
        description: "Create or manage execution plans",
        category: SlashCategory::Planning,
        feature_gate: None,
    },
    // --- Debug ---
    SlashCommandInfo {
        name: "/debug-dump",
        args: "[path]",
        description: "Enable or toggle debug dump output",
        category: SlashCategory::Debug,
        feature_gate: None,
    },
    SlashCommandInfo {
        name: "/dump-format",
        args: "<json|raw|trace>",
        description: "Switch debug dump format at runtime",
        category: SlashCategory::Debug,
        feature_gate: None,
    },
    // --- Advanced (feature-gated) ---
    #[cfg(feature = "scheduler")]
    SlashCommandInfo {
        name: "/scheduler",
        args: "[list]",
        description: "List scheduled tasks",
        category: SlashCategory::Tools,
        feature_gate: Some("scheduler"),
    },
    SlashCommandInfo {
        name: "/experiment",
        args: "[subcommand]",
        description: "Experimental features",
        category: SlashCategory::Advanced,
        feature_gate: Some("experiments"),
    },
    SlashCommandInfo {
        name: "/lsp",
        args: "",
        description: "Show LSP context status",
        category: SlashCategory::Advanced,
        feature_gate: Some("lsp-context"),
    },
    SlashCommandInfo {
        name: "/policy",
        args: "[status|check <tool> [args_json]]",
        description: "Inspect policy status or dry-run evaluation",
        category: SlashCategory::Tools,
        feature_gate: Some("policy-enforcer"),
    },
    SlashCommandInfo {
        name: "/focus",
        args: "",
        description: "Show Focus Agent status (active session, knowledge block size)",
        category: SlashCategory::Advanced,
        feature_gate: Some("context-compression"),
    },
    SlashCommandInfo {
        name: "/sidequest",
        args: "",
        description: "Show SideQuest eviction stats (passes run, tokens freed)",
        category: SlashCategory::Advanced,
        feature_gate: Some("context-compression"),
    },
];

use zeph_llm::provider::LlmProvider;

use super::Agent;
use super::error;
use super::message_queue::{MAX_IMAGE_BYTES, detect_image_mime};

impl<C: crate::channel::Channel> Agent<C> {
    /// Handle built-in slash commands that short-circuit the main `run` loop.
    ///
    /// Returns `Some(true)` to break the loop (exit), `Some(false)` to continue to the next
    /// iteration, or `None` if the command was not recognized (caller should call
    /// `process_user_message`).
    #[allow(clippy::too_many_lines)]
    pub(super) async fn handle_builtin_command(
        &mut self,
        trimmed: &str,
    ) -> Result<Option<bool>, error::AgentError> {
        if trimmed == "/clear-queue" {
            let n = self.clear_queue();
            self.notify_queue_count().await;
            self.channel
                .send(&format!("Cleared {n} queued messages."))
                .await?;
            let _ = self.channel.flush_chunks().await;
            return Ok(Some(false));
        }

        if trimmed == "/compact" {
            if self.msg.messages.len() > self.context_manager.compaction_preserve_tail + 1 {
                match self.compact_context().await {
                    Ok(
                        super::context::CompactionOutcome::Compacted
                        | super::context::CompactionOutcome::NoChange,
                    ) => {
                        let _ = self.channel.send("Context compacted successfully.").await;
                    }
                    Ok(super::context::CompactionOutcome::ProbeRejected) => {
                        let _ = self
                            .channel
                            .send(
                                "Compaction rejected: summary quality below threshold. \
                                 Original context preserved.",
                            )
                            .await;
                    }
                    Err(e) => {
                        let _ = self.channel.send(&format!("Compaction failed: {e}")).await;
                    }
                }
            } else {
                let _ = self.channel.send("Nothing to compact.").await;
            }
            let _ = self.channel.flush_chunks().await;
            return Ok(Some(false));
        }

        if trimmed == "/new" || trimmed.starts_with("/new ") {
            let args = trimmed.strip_prefix("/new").unwrap_or("").trim();
            let keep_plan = args.split_whitespace().any(|a| a == "--keep-plan");
            let no_digest = args.split_whitespace().any(|a| a == "--no-digest");
            match self.reset_conversation(keep_plan, no_digest).await {
                Ok((old_id, new_id)) => {
                    let old = old_id.map_or_else(|| "none".to_string(), |id| id.0.to_string());
                    let new = new_id.map_or_else(|| "none".to_string(), |id| id.0.to_string());
                    let keep_note = if keep_plan { " (plan preserved)" } else { "" };
                    self.channel
                        .send(&format!(
                            "New conversation started. Previous: {old} → Current: {new}{keep_note}"
                        ))
                        .await?;
                }
                Err(e) => {
                    self.channel
                        .send(&format!("Failed to start new conversation: {e}"))
                        .await?;
                }
            }
            let _ = self.channel.flush_chunks().await;
            return Ok(Some(false));
        }

        if trimmed == "/clear" {
            self.clear_history();
            self.tool_orchestrator.clear_cache();
            if let Ok(mut urls) = self.security.user_provided_urls.write() {
                urls.clear();
            }
            let _ = self.channel.flush_chunks().await;
            return Ok(Some(false));
        }

        if trimmed == "/reset" {
            self.clear_history();
            self.tool_orchestrator.clear_cache();
            if let Ok(mut urls) = self.security.user_provided_urls.write() {
                urls.clear();
            }
            self.channel.send("Conversation history reset.").await?;
            let _ = self.channel.flush_chunks().await;
            return Ok(Some(false));
        }

        if trimmed == "/cache-stats" {
            let stats = self.tool_orchestrator.cache_stats();
            self.channel.send(&stats).await?;
            let _ = self.channel.flush_chunks().await;
            return Ok(Some(false));
        }

        if trimmed == "/model" || trimmed.starts_with("/model ") {
            self.handle_model_command(trimmed).await;
            let _ = self.channel.flush_chunks().await;
            return Ok(Some(false));
        }

        if trimmed == "/provider" || trimmed.starts_with("/provider ") {
            self.handle_provider_command(trimmed).await;
            let _ = self.channel.flush_chunks().await;
            return Ok(Some(false));
        }

        if trimmed == "/debug-dump" || trimmed.starts_with("/debug-dump ") {
            self.handle_debug_dump_command(trimmed).await;
            let _ = self.channel.flush_chunks().await;
            return Ok(Some(false));
        }

        if trimmed.starts_with("/dump-format") {
            self.handle_dump_format_command(trimmed).await;
            let _ = self.channel.flush_chunks().await;
            return Ok(Some(false));
        }

        if trimmed == "/exit" || trimmed == "/quit" {
            if self.channel.supports_exit() {
                return Ok(Some(true));
            }
            let _ = self
                .channel
                .send("/exit is not supported in this channel.")
                .await;
            return Ok(Some(false));
        }

        Ok(None)
    }

    /// Dispatch slash commands. Returns `Some(Ok(()))` when handled,
    /// `Some(Err(_))` on I/O error, `None` to fall through to LLM processing.
    #[allow(clippy::too_many_lines)]
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
        if !slash_urls.is_empty()
            && let Ok(mut set) = self.security.user_provided_urls.write()
        {
            set.extend(slash_urls);
        }

        if trimmed == "/help" {
            handled!(self.handle_help_command().await);
        }

        if trimmed == "/status" {
            handled!(self.handle_status_command().await);
        }
        if trimmed == "/guardrail" {
            handled!(self.handle_guardrail_command().await);
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

        if trimmed == "/mcp" || trimmed.starts_with("/mcp ") {
            let args = trimmed.strip_prefix("/mcp").unwrap_or("").trim().to_owned();
            handled!(self.handle_mcp_command(&args).await);
        }

        if trimmed == "/image" || trimmed.starts_with("/image ") {
            let path = trimmed
                .strip_prefix("/image")
                .unwrap_or("")
                .trim()
                .to_owned();
            if path.is_empty() {
                handled!(
                    self.channel
                        .send("Usage: /image <path>")
                        .await
                        .map_err(Into::into)
                );
            }
            handled!(self.handle_image_command(&path).await);
        }

        if trimmed == "/plan" || trimmed.starts_with("/plan ") {
            return Some(self.dispatch_plan_command(trimmed).await);
        }

        if trimmed == "/graph" || trimmed.starts_with("/graph ") {
            handled!(self.handle_graph_command(trimmed).await);
        }

        if trimmed == "/memory" || trimmed.starts_with("/memory ") {
            handled!(self.handle_memory_command(trimmed).await);
        }
        if trimmed == "/guidelines" {
            handled!(self.handle_guidelines_command().await);
        }

        #[cfg(feature = "scheduler")]
        if trimmed == "/scheduler" || trimmed.starts_with("/scheduler ") {
            handled!(self.handle_scheduler_command(trimmed).await);
        }
        if trimmed == "/experiment" || trimmed.starts_with("/experiment ") {
            handled!(self.handle_experiment_command(trimmed).await);
        }
        if trimmed == "/lsp" {
            handled!(self.handle_lsp_status_command().await);
        }
        if trimmed == "/policy" || trimmed.starts_with("/policy ") {
            let args = trimmed
                .strip_prefix("/policy")
                .unwrap_or("")
                .trim()
                .to_owned();
            handled!(self.handle_policy_command(&args).await);
        }

        if trimmed == "/log" {
            handled!(self.handle_log_command().await);
        }

        if trimmed.starts_with("/agent") || trimmed.starts_with('@') {
            return self.dispatch_agent_command(trimmed).await;
        }
        if trimmed == "/focus" {
            handled!(self.handle_focus_status_command().await);
        }
        if trimmed == "/sidequest" {
            handled!(self.handle_sidequest_status_command().await);
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
        match crate::subagent::AgentCommand::parse(trimmed, &known) {
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

    pub(super) async fn handle_image_command(
        &mut self,
        path: &str,
    ) -> Result<(), error::AgentError> {
        use std::path::Component;
        use zeph_llm::provider::{ImageData, MessagePart};

        let has_parent_dir = std::path::Path::new(path)
            .components()
            .any(|c| c == Component::ParentDir);
        if has_parent_dir {
            self.channel
                .send("Invalid image path: path traversal not allowed")
                .await?;
            let _ = self.channel.flush_chunks().await;
            return Ok(());
        }

        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) => {
                self.channel
                    .send(&format!("Cannot read image {path}: {e}"))
                    .await?;
                let _ = self.channel.flush_chunks().await;
                return Ok(());
            }
        };
        if data.len() > MAX_IMAGE_BYTES {
            self.channel
                .send(&format!(
                    "Image {path} exceeds size limit ({} MB), skipping",
                    MAX_IMAGE_BYTES / 1024 / 1024
                ))
                .await?;
            let _ = self.channel.flush_chunks().await;
            return Ok(());
        }
        let mime_type = detect_image_mime(Some(path)).to_string();
        self.msg
            .pending_image_parts
            .push(MessagePart::Image(Box::new(ImageData { data, mime_type })));
        self.channel
            .send(&format!("Image loaded: {path}. Send your message."))
            .await?;
        let _ = self.channel.flush_chunks().await;
        Ok(())
    }

    pub(super) async fn handle_help_command(&mut self) -> Result<(), error::AgentError> {
        use std::fmt::Write;

        let mut out = String::from("Slash commands:\n\n");

        let categories = [
            SlashCategory::Info,
            SlashCategory::Session,
            SlashCategory::Model,
            SlashCategory::Memory,
            SlashCategory::Tools,
            SlashCategory::Planning,
            SlashCategory::Debug,
            SlashCategory::Advanced,
        ];

        for cat in &categories {
            let entries: Vec<_> = COMMANDS.iter().filter(|c| &c.category == cat).collect();
            if entries.is_empty() {
                continue;
            }
            let _ = writeln!(out, "{}:", cat.as_str());
            for cmd in entries {
                if cmd.args.is_empty() {
                    let _ = write!(out, "  {}", cmd.name);
                } else {
                    let _ = write!(out, "  {} {}", cmd.name, cmd.args);
                }
                let _ = write!(out, "  — {}", cmd.description);
                if let Some(feat) = cmd.feature_gate {
                    let _ = write!(out, " [requires: {feat}]");
                }
                let _ = writeln!(out);
            }
            let _ = writeln!(out);
        }

        self.channel.send(out.trim_end()).await?;
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn handle_status_command(&mut self) -> Result<(), error::AgentError> {
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

        let skill_count = self
            .skill_state
            .registry
            .read()
            .map(|r| r.all_meta().len())
            .unwrap_or(0);

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
        if let Some(ref tf) = self.tool_schema_filter {
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

        let gc = &self.memory_state.graph_config;
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

        self.channel.send(out.trim_end()).await?;
        Ok(())
    }

    pub(super) async fn handle_guardrail_command(&mut self) -> Result<(), error::AgentError> {
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

        self.channel.send(out.trim_end()).await?;
        Ok(())
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
            .expect("registry read lock")
            .all_meta()
            .into_iter()
            .cloned()
            .collect();

        let mut trust_map: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for meta in &all_meta {
            if let Some(memory) = &self.memory_state.memory {
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

        if let Some(memory) = &self.memory_state.memory {
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
            .expect("registry read lock")
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
