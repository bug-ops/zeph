// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Sub-agent command handlers and spawn-context assembly.
//!
//! Extracted from `agent/mod.rs` (#4923). Handles `/agent` command dispatch (list,
//! status, approve/deny, spawn, cancel, resume), background polling of running
//! sub-agents, and construction of the bounded parent-message context handed to a
//! freshly spawned sub-agent.

use std::sync::Arc;

use zeph_tools::registry::ToolDef;

use super::{Agent, error};
use crate::channel::Channel;

/// Number of trailing forwarded-transcript lines surfaced per subagent in
/// [`crate::metrics::SubAgentMetrics::live_transcript`] (issue #6359, FR-005).
const LIVE_TRANSCRIPT_TAIL_LINES: usize = 20;

impl<C: Channel> Agent<C> {
    /// Resolve a sub-agent's requested vault-secret key against the custom secrets already
    /// resolved from the vault at startup (`ZEPH_SECRET_<NAME>` keys — the same pre-resolved
    /// map used for skill `requires_secrets` injection, see `tool_execution::inject_active_skill_env`).
    ///
    /// Matching is case-insensitive with `-` normalized to `_`, mirroring the vault-key
    /// naming convention (`ZEPH_SECRET_MY-KEY` and `ZEPH_SECRET_MY_KEY` both resolve to
    /// `my_key`). Returns `None` when `key` was never resolved from the vault at startup.
    pub(crate) fn resolve_subagent_secret(&self, key: &str) -> Option<crate::vault::Secret> {
        let normalized = key.to_lowercase().replace('-', "_");
        self.services
            .skill
            .available_custom_secrets
            .get(&normalized)
            .map(|s| crate::vault::Secret::new(s.expose().to_owned()))
    }

    /// Poll all active sub-agents for completed/failed/canceled results.
    ///
    /// Non-blocking: returns immediately with a list of `(task_id, result)` pairs
    /// for agents that have finished. Each completed agent is removed from the manager.
    #[tracing::instrument(name = "core.agent.poll_subagents", skip_all, level = "debug")]
    pub async fn poll_subagents(&mut self) -> Vec<(String, String)> {
        let Some(mgr) = &mut self.services.orchestration.subagent_manager else {
            return vec![];
        };

        let finished: Vec<String> = mgr
            .statuses()
            .into_iter()
            .filter_map(|(id, status)| {
                if matches!(
                    status.state,
                    zeph_subagent::SubAgentState::Completed
                        | zeph_subagent::SubAgentState::Failed
                        | zeph_subagent::SubAgentState::Canceled
                ) {
                    Some(id)
                } else {
                    None
                }
            })
            .collect();

        let mut results = vec![];
        for task_id in finished {
            match mgr.collect(&task_id).await {
                Ok(result) => results.push((task_id, result)),
                Err(e) => {
                    tracing::warn!(task_id, error = %e, "failed to collect sub-agent result");
                }
            }
        }
        results
    }
    /// Run the chat loop, receiving messages via the channel until EOF or shutdown.
    ///
    /// # Errors
    ///
    /// Returns an error if channel I/O or LLM communication fails.
    /// Refresh sub-agent metrics snapshot for the TUI metrics panel.
    pub(super) fn refresh_subagent_metrics(&mut self) {
        let Some(ref mgr) = self.services.orchestration.subagent_manager else {
            return;
        };
        let sub_agent_metrics: Vec<crate::metrics::SubAgentMetrics> = mgr
            .statuses()
            .into_iter()
            .map(|(id, s)| {
                let def = mgr.agents_def(&id);
                crate::metrics::SubAgentMetrics {
                    name: def.map_or_else(|| id[..8.min(id.len())].to_owned(), |d| d.name.clone()),
                    id: id.clone(),
                    state: format!("{:?}", s.state).to_lowercase(),
                    turns_used: s.turns_used,
                    max_turns: def.map_or(20, |d| d.permissions.max_turns),
                    background: def.is_some_and(|d| d.permissions.background),
                    elapsed_secs: s.started_at.elapsed().as_secs(),
                    permission_mode: def.map_or_else(String::new, |d| {
                        use zeph_subagent::def::PermissionMode;
                        match d.permissions.permission_mode {
                            PermissionMode::AcceptEdits => "accept_edits".into(),
                            PermissionMode::DontAsk => "dont_ask".into(),
                            PermissionMode::BypassPermissions => "bypass_permissions".into(),
                            PermissionMode::Plan => "plan".into(),
                            _ => String::new(),
                        }
                    }),
                    transcript_dir: mgr
                        .agent_transcript_dir(&id)
                        .map(|p| p.to_string_lossy().into_owned()),
                    live_transcript: mgr.forwarded_tail(&id, LIVE_TRANSCRIPT_TAIL_LINES),
                }
            })
            .collect();
        self.update_metrics(|m| m.sub_agents = sub_agent_metrics);
    }
    /// Non-blocking poll: notify the user when background sub-agents complete.
    pub(super) async fn notify_completed_subagents(&mut self) -> Result<(), error::AgentError> {
        let completed = self.poll_subagents().await;
        for (task_id, result) in completed {
            let notice = if result.is_empty() {
                format!("[sub-agent {id}] completed (no output)", id = &task_id[..8])
            } else {
                format!("[sub-agent {id}] completed:\n{result}", id = &task_id[..8])
            };
            if let Err(e) = self.channel.send(&notice).await {
                tracing::warn!(error = %e, "failed to send sub-agent completion notice");
            }
        }
        Ok(())
    }
    /// Poll a sub-agent until it reaches a terminal state, bridging secret requests to the
    /// channel. Returns a human-readable status string and success flag suitable for
    /// sending to the user and emitting lifecycle events.
    async fn poll_subagent_until_done(
        &mut self,
        task_id: &str,
        label: &str,
    ) -> Option<(String, bool)> {
        use zeph_subagent::SubAgentState;
        let result = loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            // Bridge secret requests from sub-agent to channel.confirm().
            // Fetch the pending request first, then release the borrow before
            // calling channel.confirm() (which requires &mut self).
            #[allow(clippy::redundant_closure_for_method_calls)]
            let pending = self
                .services
                .orchestration
                .subagent_manager
                .as_mut()
                .and_then(|m| m.try_recv_secret_request());
            if let Some((req_task_id, req)) = pending {
                // req.secret_key is pre-validated to [a-zA-Z0-9_-] in manager.rs
                // (SEC-P1-02), so it is safe to embed in the prompt string.
                let confirm_prompt = format!(
                    "Sub-agent requests secret '{}'. Allow?",
                    crate::text::truncate_to_chars(&req.secret_key, 100)
                );
                let approved = self.channel.confirm(&confirm_prompt).await.unwrap_or(false);
                if approved {
                    let ttl = std::time::Duration::from_mins(5);
                    let key = req.secret_key.clone();
                    let resolved = self.resolve_subagent_secret(&key);
                    if let Some(mgr) = self.services.orchestration.subagent_manager.as_mut() {
                        if let Some(secret) = resolved {
                            if mgr.approve_secret(&req_task_id, &key, ttl).is_ok()
                                && let Err(e) = mgr.deliver_secret(&req_task_id, &key, secret)
                            {
                                tracing::warn!(error = %e, "sub-agent secret delivery failed");
                                let _ = mgr.deny_secret(&req_task_id);
                            }
                        } else {
                            tracing::warn!(
                                "sub-agent requested secret not resolvable from vault; denying"
                            );
                            let _ = mgr.deny_secret(&req_task_id);
                        }
                    }
                } else if let Some(mgr) = self.services.orchestration.subagent_manager.as_mut() {
                    let _ = mgr.deny_secret(&req_task_id);
                }
            }

            let mgr = self.services.orchestration.subagent_manager.as_ref()?;
            let statuses = mgr.statuses();
            let Some((_, status)) = statuses.iter().find(|(id, _)| id == task_id) else {
                break (format!("{label} completed (no status available)."), true);
            };
            match status.state {
                SubAgentState::Completed => {
                    let msg = status.last_message.clone().unwrap_or_else(|| "done".into());
                    break (format!("{label} completed: {msg}"), true);
                }
                SubAgentState::Failed => {
                    let msg = status
                        .last_message
                        .clone()
                        .unwrap_or_else(|| "unknown error".into());
                    break (format!("{label} failed: {msg}"), false);
                }
                SubAgentState::Canceled => {
                    break (format!("{label} was cancelled."), false);
                }
                _ => {
                    self.channel
                        .send_status_best_effort(&format!(
                            "{label}: turn {}/{}",
                            status.turns_used,
                            self.services
                                .orchestration
                                .subagent_manager
                                .as_ref()
                                .and_then(|m| m.agents_def(task_id))
                                .map_or(20, |d| d.permissions.max_turns)
                        ))
                        .await;
                }
            }
        };
        Some(result)
    }
    /// Resolve a unique full `task_id` from a prefix. Returns `None` if the manager is absent,
    /// `Some(Err(msg))` on ambiguity/not-found, `Some(Ok(full_id))` on success.
    fn resolve_agent_id_prefix(&mut self, prefix: &str) -> Option<Result<String, String>> {
        let mgr = self.services.orchestration.subagent_manager.as_mut()?;
        let full_ids: Vec<String> = mgr
            .statuses()
            .into_iter()
            .map(|(tid, _)| tid)
            .filter(|tid| tid.starts_with(prefix))
            .collect();
        Some(match full_ids.as_slice() {
            [] => Err(format!("No sub-agent with id prefix '{prefix}'")),
            [fid] => Ok(fid.clone()),
            _ => Err(format!(
                "Ambiguous id prefix '{prefix}': matches {} agents",
                full_ids.len()
            )),
        })
    }
    fn handle_agent_list(&self) -> Option<String> {
        use std::fmt::Write as _;
        let mgr = self.services.orchestration.subagent_manager.as_ref()?;
        let mode_label = match mgr.delegation_mode() {
            zeph_config::DelegationMode::Disabled => "disabled",
            zeph_config::DelegationMode::ExplicitRequestOnly => "explicit_request_only",
            zeph_config::DelegationMode::Proactive => "proactive",
            _ => "unknown",
        };
        let defs = mgr.definitions();
        if defs.is_empty() {
            return Some(format!(
                "Delegation mode: {mode_label}\nNo sub-agent definitions found."
            ));
        }
        let mut out = format!("Delegation mode: {mode_label}\nAvailable sub-agents:\n");
        for d in defs {
            let memory_label = match d.memory {
                Some(zeph_subagent::MemoryScope::User) => " [memory:user]",
                Some(zeph_subagent::MemoryScope::Project) => " [memory:project]",
                Some(zeph_subagent::MemoryScope::Local) => " [memory:local]",
                Some(_) => " [memory:unknown]",
                None => "",
            };
            if let Some(ref src) = d.source {
                let _ = writeln!(
                    out,
                    "  {}{} — {} ({})",
                    d.name, memory_label, d.description, src
                );
            } else {
                let _ = writeln!(out, "  {}{} — {}", d.name, memory_label, d.description);
            }
        }
        Some(out)
    }
    fn handle_agent_status(&self) -> Option<String> {
        use std::fmt::Write as _;
        let mgr = self.services.orchestration.subagent_manager.as_ref()?;
        let statuses = mgr.statuses();
        if statuses.is_empty() {
            return Some("No active sub-agents.".into());
        }
        let mut out = String::from("Active sub-agents:\n");
        for (id, s) in &statuses {
            let state = format!("{:?}", s.state).to_lowercase();
            let elapsed = s.started_at.elapsed().as_secs();
            let _ = writeln!(
                out,
                "  [{short}] {state}  turns={t}  elapsed={elapsed}s  {msg}",
                short = &id[..8.min(id.len())],
                t = s.turns_used,
                msg = s.last_message.as_deref().unwrap_or(""),
            );
            // Show memory directory path for agents with memory enabled.
            if let Some(def) = mgr.agents_def(id)
                && let Some(scope) = def.memory
                && let Ok(dir) = zeph_subagent::memory::resolve_memory_dir(scope, &def.name)
            {
                let _ = writeln!(out, "       memory: {}", dir.display());
            }
        }
        Some(out)
    }
    fn handle_agent_approve(&mut self, id: &str) -> Option<String> {
        let full_id = match self.resolve_agent_id_prefix(id)? {
            Ok(fid) => fid,
            Err(msg) => return Some(msg),
        };
        let req = {
            let mgr = self.services.orchestration.subagent_manager.as_mut()?;
            mgr.try_recv_secret_request_for(&full_id)
        };
        let Some(req) = req else {
            return Some(format!(
                "No pending secret request for sub-agent '{full_id}'."
            ));
        };
        let key = req.secret_key.clone();
        let ttl = std::time::Duration::from_mins(5);
        let Some(secret) = self.resolve_subagent_secret(&key) else {
            let mgr = self.services.orchestration.subagent_manager.as_mut()?;
            let _ = mgr.deny_secret(&full_id);
            return Some(format!(
                "Secret '{key}' could not be resolved from the vault; request denied."
            ));
        };
        let mgr = self.services.orchestration.subagent_manager.as_mut()?;
        if let Err(e) = mgr.approve_secret(&full_id, &key, ttl) {
            return Some(format!("Approve failed: {e}"));
        }
        if let Err(e) = mgr.deliver_secret(&full_id, &key, secret) {
            let _ = mgr.deny_secret(&full_id);
            return Some(format!("Secret delivery failed: {e}"));
        }
        Some(format!("Secret '{key}' approved for sub-agent {full_id}."))
    }
    fn handle_agent_deny(&mut self, id: &str) -> Option<String> {
        let full_id = match self.resolve_agent_id_prefix(id)? {
            Ok(fid) => fid,
            Err(msg) => return Some(msg),
        };
        let mgr = self.services.orchestration.subagent_manager.as_mut()?;
        match mgr.deny_secret(&full_id) {
            Ok(()) => Some(format!("Secret request denied for sub-agent '{full_id}'.")),
            Err(e) => Some(format!("Deny failed: {e}")),
        }
    }
    pub(super) async fn handle_agent_command(
        &mut self,
        cmd: zeph_subagent::AgentCommand,
    ) -> Option<String> {
        use zeph_subagent::AgentCommand;

        match cmd {
            AgentCommand::List => self.handle_agent_list(),
            AgentCommand::Background { name, prompt } => {
                self.handle_agent_background(&name, &prompt).await
            }
            AgentCommand::Spawn { name, prompt }
            | AgentCommand::Mention {
                agent: name,
                prompt,
            } => self.handle_agent_spawn_foreground(&name, &prompt).await,
            AgentCommand::Status => self.handle_agent_status(),
            AgentCommand::Cancel { id } => self.handle_agent_cancel(&id),
            AgentCommand::Approve { id } => self.handle_agent_approve(&id),
            AgentCommand::Deny { id } => self.handle_agent_deny(&id),
            AgentCommand::Resume { id, prompt } => self.handle_agent_resume(&id, &prompt).await,
            _ => None,
        }
    }
    /// Return the sub-agent definitions section formatted for the `/agents` fleet view.
    ///
    /// Produces a "Sub-agents:" header followed by one line per definition.
    /// Returns an empty string when no sub-agent manager is configured.
    pub(crate) fn handle_agents_definitions_list(&self) -> String {
        use std::fmt::Write as _;

        let Some(mgr) = self.services.orchestration.subagent_manager.as_ref() else {
            return String::new();
        };
        let defs = mgr.definitions();
        if defs.is_empty() {
            return String::new();
        }
        let mut out = String::from("Sub-agents:\n");
        for d in defs {
            let memory_label = match d.memory {
                Some(zeph_subagent::MemoryScope::User) => " [memory:user]",
                Some(zeph_subagent::MemoryScope::Project) => " [memory:project]",
                Some(zeph_subagent::MemoryScope::Local) => " [memory:local]",
                Some(_) => " [memory:unknown]",
                None => "",
            };
            if let Some(ref src) = d.source {
                let _ = writeln!(
                    out,
                    "  {}{} — {} ({})",
                    d.name, memory_label, d.description, src
                );
            } else {
                let _ = writeln!(out, "  {}{} — {}", d.name, memory_label, d.description);
            }
        }
        out
    }
    /// Execute an `/agents` CRUD subcommand and return a formatted string.
    ///
    /// Handles `show`, `create`, `edit`, `delete` (the `list` case is handled by
    /// [`handle_agents_definitions_list`] and never reaches this method).
    pub(crate) fn handle_agents_crud(&mut self, cmd: zeph_subagent::AgentsCommand) -> String {
        use zeph_subagent::AgentsCommand;

        let Some(mgr) = self.services.orchestration.subagent_manager.as_ref() else {
            return "Sub-agent manager is not available.".to_owned();
        };

        match cmd {
            AgentsCommand::List => self.handle_agents_definitions_list(),
            AgentsCommand::Show { name } => {
                match mgr.definitions().iter().find(|d| d.name == name) {
                    Some(d) => format!(
                        "Agent: {}\nDescription: {}\nSource: {}\n",
                        d.name,
                        d.description,
                        d.source.as_deref().unwrap_or("unknown"),
                    ),
                    None => format!("No sub-agent definition named '{name}'."),
                }
            }
            AgentsCommand::Create { name } => {
                format!(
                    "To create a sub-agent definition, create a file at `.zeph/agents/{name}.md`.\n\
                     See the sub-agent documentation for the required frontmatter."
                )
            }
            AgentsCommand::Edit { name } => {
                format!("To edit '{name}', open its definition file in `.zeph/agents/{name}.md`.")
            }
            AgentsCommand::Delete { name } => {
                format!("To delete '{name}', remove the file `.zeph/agents/{name}.md`.")
            }
            _ => "Unknown agents command.".to_owned(),
        }
    }
    async fn handle_agent_background(&mut self, name: &str, prompt: &str) -> Option<String> {
        let provider = self.provider.clone();
        let tool_executor = Arc::clone(&self.tool_executor);
        let skills = self.filtered_skills_for(name);
        let cfg = self.services.orchestration.subagent_config.clone();
        let mut spawn_ctx = self.build_spawn_context(&cfg);
        // Background durable: seat wired so child can resolve; on a fresh run the promise
        // (await side) is dropped — background results are collected via poll_subagents. On a
        // resumed run whose child already finished, replay short-circuits below instead.
        self.ensure_session_durable_ctx().await;
        match resolve_durable_spawn_gate(
            self.services.session.durable_subagent,
            self.services.session.durable_ctx.as_deref(),
        )
        .await
        {
            DurableSpawnGate::Fresh(seat) => spawn_ctx.durable_resolver = Some(seat),
            DurableSpawnGate::Replayed { result, .. } => {
                let short = &result.task_id[..8.min(result.task_id.len())];
                return Some(if result.output.is_empty() {
                    format!(
                        "[sub-agent {short}] completed (no output, replayed from durable journal)"
                    )
                } else {
                    format!(
                        "[sub-agent {short}] completed (replayed from durable journal):\n{}",
                        result.output
                    )
                });
            }
            DurableSpawnGate::None => {}
        }
        let mgr = self.services.orchestration.subagent_manager.as_mut()?;
        match mgr
            .spawn(
                name,
                prompt,
                provider,
                tool_executor,
                skills,
                &cfg,
                spawn_ctx,
            )
            .await
        {
            Ok(id) => Some(format!(
                "Sub-agent '{name}' started in background (id: {short})",
                short = &id[..8.min(id.len())]
            )),
            Err(e) => Some(format!("Failed to spawn sub-agent: {e}")),
        }
    }
    /// Handle a [`DurableSpawnGate::Replayed`] result for a foreground spawn.
    ///
    /// Gates the channel side effects (user notice + TUI completion event) behind an
    /// out-of-band `notified_at` claim on the sub-agent's durable promise, so a parent that
    /// restarts *again* after already taking the replay branch once does not re-fire them
    /// (#6027). The claim consumes no durable step id, so unlike a `ctx.step()`-based guard it
    /// cannot perturb INV-2 step-id determinism or cause `ReplayDivergence`. Returns the
    /// journaled output/error text either way.
    async fn notify_replayed_foreground_subagent(
        &mut self,
        name: &str,
        result: zeph_subagent::SubagentResult,
        promise_id: zeph_durable::PromiseId,
    ) -> String {
        let success = result.state == zeph_subagent::SubAgentState::Completed;
        let task_id = result.task_id.clone();

        // Out-of-band, step-counter-independent claim: the FIRST caller to set `notified_at` fires
        // the channel side effects; every later replay is suppressed. Unlike a ctx.step this consumes
        // no StepId, so it cannot cause ReplayDivergence under any restart count (#6027). Degrade to
        // firing directly when durable is off (no replay can happen) or the claim errors.
        let should_notify = if let Some(ctx) = self.services.session.durable_ctx.clone() {
            match ctx.claim_promise_notification(promise_id).await {
                Ok(claimed) => claimed,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "durable: promise-notification claim failed; \
                         firing the replayed sub-agent notice directly"
                    );
                    true
                }
            }
        } else {
            true
        };

        let text = if success {
            result.output
        } else {
            result.error.unwrap_or_else(|| "unknown error".to_owned())
        };

        if should_notify {
            let _ = self
                .channel
                .send(&format!(
                    "Sub-agent '{name}' replayed from durable journal (already finished \
                     before the parent restarted)."
                ))
                .await;
            let _ = self
                .channel
                .notify_foreground_subagent_completed(&task_id, name, success)
                .await;
        }
        text
    }

    async fn handle_agent_spawn_foreground(&mut self, name: &str, prompt: &str) -> Option<String> {
        let provider = self.provider.clone();
        let tool_executor = Arc::clone(&self.tool_executor);
        let skills = self.filtered_skills_for(name);
        let cfg = self.services.orchestration.subagent_config.clone();
        let mut spawn_ctx = self.build_spawn_context(&cfg);
        // Wire the durable resolver seat so the child can resolve its promise on exit. On a
        // fresh run the promise (await side) is dropped here; foreground result is collected
        // via poll_subagent_until_done which reads the join-handle output directly. On a
        // resumed run whose child already finished, replay short-circuits below instead.
        self.ensure_session_durable_ctx().await;
        match resolve_durable_spawn_gate(
            self.services.session.durable_subagent,
            self.services.session.durable_ctx.as_deref(),
        )
        .await
        {
            DurableSpawnGate::Fresh(seat) => spawn_ctx.durable_resolver = Some(seat),
            DurableSpawnGate::Replayed { result, promise_id } => {
                return Some(
                    self.notify_replayed_foreground_subagent(name, result, promise_id)
                        .await,
                );
            }
            DurableSpawnGate::None => {}
        }
        let mgr = self.services.orchestration.subagent_manager.as_mut()?;
        let task_id = match mgr
            .spawn(
                name,
                prompt,
                provider,
                tool_executor,
                skills,
                &cfg,
                spawn_ctx,
            )
            .await
        {
            Ok(id) => id,
            Err(e) => return Some(format!("Failed to spawn sub-agent: {e}")),
        };
        let short = task_id[..8.min(task_id.len())].to_owned();
        let _ = self
            .channel
            .send(&format!("Sub-agent '{name}' running... (id: {short})"))
            .await;
        let _ = self
            .channel
            .notify_foreground_subagent_started(&task_id, name)
            .await;
        let label = format!("Sub-agent '{name}'");
        let Some((result, success)) = self.poll_subagent_until_done(&task_id, &label).await else {
            // Manager was dropped mid-poll; emit completed(false) so TUI does not stay stuck.
            let _ = self
                .channel
                .notify_foreground_subagent_completed(&task_id, name, false)
                .await;
            return None;
        };
        let _ = self
            .channel
            .notify_foreground_subagent_completed(&task_id, name, success)
            .await;
        Some(result)
    }
    fn handle_agent_cancel(&mut self, id: &str) -> Option<String> {
        let mgr = self.services.orchestration.subagent_manager.as_mut()?;
        // Accept prefix match on task_id.
        let ids: Vec<String> = mgr
            .statuses()
            .into_iter()
            .map(|(task_id, _)| task_id)
            .filter(|task_id| task_id.starts_with(id))
            .collect();
        match ids.as_slice() {
            [] => Some(format!("No sub-agent with id prefix '{id}'")),
            [full_id] => {
                let full_id = full_id.clone();
                match mgr.cancel(&full_id) {
                    Ok(()) => Some(format!("Cancelled sub-agent {full_id}.")),
                    Err(e) => Some(format!("Cancel failed: {e}")),
                }
            }
            _ => Some(format!(
                "Ambiguous id prefix '{id}': matches {} agents",
                ids.len()
            )),
        }
    }
    async fn handle_agent_resume(&mut self, id: &str, prompt: &str) -> Option<String> {
        let cfg = self.services.orchestration.subagent_config.clone();
        // Resolve definition name from transcript meta before spawning so we can
        // look up skills by definition name rather than the UUID prefix (S1 fix).
        let def_name = {
            let mgr = self.services.orchestration.subagent_manager.as_ref()?;
            match mgr.def_name_for_resume(id, &cfg).await {
                Ok(name) => name,
                Err(e) => return Some(format!("Failed to resume sub-agent: {e}")),
            }
        };
        let skills = self.filtered_skills_for(&def_name);
        let provider = self.provider.clone();
        let tool_executor = Arc::clone(&self.tool_executor);
        // Built before borrowing `subagent_manager` mutably below (build_spawn_context takes
        // `&self`). Previously this call site passed `None`, which meant resumed sub-agents
        // never got a `debug_dump_sink` — their LLM calls went uncaptured by `--debug-dump`
        // even though fresh spawns correctly wired it (#6391). `resume()` only reads
        // `max_trust_level`/`inherited_tool_allowlist`/`network_denied`/`debug_dump_sink` off
        // `spawn_context` (see `manager/spawn.rs::resume`) — the first three are already at
        // `build_spawn_context`'s top-level defaults (`None`/`None`/`false`, identical to what
        // `None` produced here), so this only changes `debug_dump_sink` for this call site.
        let spawn_ctx = self.build_spawn_context(&cfg);
        let mgr = self.services.orchestration.subagent_manager.as_mut()?;
        let (task_id, _) = match mgr
            .resume(
                id,
                prompt,
                provider,
                tool_executor,
                skills,
                &cfg,
                Some(&spawn_ctx),
            )
            .await
        {
            Ok(pair) => pair,
            Err(e) => return Some(format!("Failed to resume sub-agent: {e}")),
        };
        let short = task_id[..8.min(task_id.len())].to_owned();
        let _ = self
            .channel
            .send(&format!("Resuming sub-agent '{id}'... (new id: {short})"))
            .await;
        let _ = self
            .channel
            .notify_foreground_subagent_started(&task_id, &def_name)
            .await;
        let Some((result, success)) = self
            .poll_subagent_until_done(&task_id, "Resumed sub-agent")
            .await
        else {
            // Manager was dropped mid-poll; emit completed(false) so TUI does not stay stuck.
            let _ = self
                .channel
                .notify_foreground_subagent_completed(&task_id, &def_name, false)
                .await;
            return None;
        };
        let _ = self
            .channel
            .notify_foreground_subagent_completed(&task_id, &def_name, success)
            .await;
        Some(result)
    }
    /// Resolve the skill bodies to inject into a freshly spawned (or resumed) sub-agent's
    /// one-shot system prompt.
    ///
    /// A sub-agent definition with an empty `skills.include` filter inherits every skill in
    /// the registry (documented, intentional — see [`zeph_config::SkillFilter`]). Unlike the
    /// main agent's per-turn skill matcher, these bodies are injected once, at spawn time, with
    /// no relevance ranking and no later opportunity to trim — an unbounded include set can
    /// silently blow the turn-1 context budget (#6421).
    ///
    /// The `subagent_skill_token_budget` cap applies **only** to that empty-include case. A
    /// definition with an explicit, hand-curated `skills.include` list is never capped here —
    /// the operator opted into that specific set on purpose, and applying the same budget would
    /// silently regress configs that were never broken; #6421 is about the *default* (empty)
    /// case only.
    ///
    /// When capped, bodies are accumulated in the order [`zeph_subagent::filter_skills`] returns
    /// them — the registry's directory-walk order, i.e. alphabetical by skill directory name,
    /// **not** relevance-ranked. A task-critical skill whose directory happens to sort late is
    /// systematically the first cut on every default-include spawn; operators who hit this can
    /// curate `include` explicitly or raise the budget. Accumulation is a greedy best-fit, not a
    /// hard prefix cut: an over-budget skill is skipped (not a stopping point), so a smaller
    /// skill later in the order can still be packed in afterward. The first skill is always
    /// included even if it alone exceeds the budget, so a single oversized skill never starves
    /// the whole set. Any skills left out are surfaced via a synthetic marker entry rather than
    /// silently dropped.
    pub(super) fn filtered_skills_for(&self, agent_name: &str) -> Option<Vec<String>> {
        let mgr = self.services.orchestration.subagent_manager.as_ref()?;
        let def = mgr.definitions().iter().find(|d| d.name == agent_name)?;
        let reg = self.services.skill.registry.read();
        let skills = match zeph_subagent::filter_skills(&reg, &def.skills) {
            Ok(skills) => skills,
            Err(e) => {
                tracing::warn!(error = %e, "skill filtering failed for sub-agent");
                return None;
            }
        };
        if skills.is_empty() {
            return None;
        }

        // #6421 scope: only the empty-include "inherit everything" case is capped. An explicit,
        // hand-curated include list is trusted as-is (see doc comment above).
        if !def.skills.include.is_empty() {
            return Some(skills.into_iter().map(|s| s.body).collect());
        }

        let total = skills.len();
        let budget = self.services.skill.subagent_skill_token_budget;
        let counter = &self.runtime.metrics.token_counter;

        let mut bodies: Vec<String> = Vec::with_capacity(total);
        let mut running_tokens = 0usize;
        let mut omitted_names: Vec<&str> = Vec::new();

        for skill in &skills {
            let skill_tokens = counter.count_tokens(&skill.body);
            if !bodies.is_empty() && running_tokens + skill_tokens > budget {
                omitted_names.push(skill.meta.name.as_str());
                continue;
            }
            running_tokens += skill_tokens;
            bodies.push(skill.body.clone());
        }

        if !omitted_names.is_empty() {
            let included = bodies.len();
            tracing::warn!(
                agent_name,
                included,
                total,
                budget_tokens = budget,
                "sub-agent skill body budget exceeded; truncated skill set"
            );
            bodies.push(format!(
                "[skill budget: {included}/{total} skills included, budget={budget} tokens — omitted: {}]",
                omitted_names.join(", ")
            ));
        }

        Some(bodies)
    }
    /// The effective delegation mode currently in force (spec 042, issue #5857).
    ///
    /// Reads directly from `subagent_config` (always present, independent of whether a
    /// `SubAgentManager` happens to be constructed) via
    /// [`zeph_config::SubAgentConfig::effective_delegation_mode`], which folds in the
    /// `enabled` outer kill switch. This is the same fold `src/runner.rs` bootstrap applies
    /// before calling `SubAgentManager::set_delegation_mode` — reading it here independently
    /// keeps this choke point correct even where no manager is wired up (e.g. a test harness).
    pub(super) fn effective_delegation_mode(&self) -> zeph_config::DelegationMode {
        self.services
            .orchestration
            .subagent_config
            .effective_delegation_mode()
    }
    /// Build a `SpawnContext` from current agent state for sub-agent spawning.
    pub(super) fn build_spawn_context(
        &self,
        cfg: &zeph_config::SubAgentConfig,
    ) -> zeph_subagent::SpawnContext {
        zeph_subagent::SpawnContext {
            parent_messages: self.extract_parent_messages(cfg),
            parent_cancel: Some(self.runtime.lifecycle.cancel_token.clone()),
            parent_provider_name: {
                let name = &self.runtime.config.active_provider_name;
                if name.is_empty() {
                    None
                } else {
                    Some(name.clone())
                }
            },
            spawn_depth: self.runtime.config.spawn_depth,
            mcp_tool_names: self.extract_mcp_tool_names(),
            // F3 spec 050 §4: propagate seeded score when parent is >= Elevated.
            seed_trajectory_score: {
                let child = self.services.security.trajectory.spawn_child();
                let score = child.score_now();
                if score > 0.0 { Some(score) } else { None }
            },
            content_isolation: self.runtime.config.security.content_isolation.clone(),
            orchestrator_name: Some("zeph".to_owned()),
            orchestrator_role: Some("orchestrator".to_owned()),
            session_mcp_servers: Vec::new(),
            // Threaded down so sub-agent LLM calls are captured through the same
            // `--debug-dump` pipeline as the top-level agent loop (#6391). `None` when
            // debug dumps are disabled, mirroring the top-level `debug_dumper: None` case.
            // Wrapped in `PiiScrubbingDumpSink` so sub-agent dumps get the same optional
            // `PiiFilter` layer top-level dumps get via `write_chat_debug_dump` — the plain
            // `DebugDumpSink` impl on `DebugDumper` only applies the baseline
            // `scrub_content`/`redact_binary_blobs` pass (#6407).
            debug_dump_sink: self.runtime.debug.debug_dumper.clone().map(|d| {
                Arc::new(crate::debug_dump::PiiScrubbingDumpSink::new(
                    d,
                    self.services.security.pii_filter.clone(),
                )) as Arc<dyn zeph_llm::debug_dump::DebugDumpSink>
            }),
            // Constraint propagation (#3993/#6493): cap the spawned sub-agent's trust to the
            // parent session's own current effective trust level, so a sub-agent can never
            // receive higher privileges than the parent itself currently holds — this is the
            // only production call site that constructs a `SpawnContext`, so every spawn path
            // (foreground, background, and orchestration-driven via
            // `handle_scheduler_spawn_action`) is covered.
            max_trust_level: Some(self.parent_effective_trust_level()),
            // This helper's own three callers (`handle_agent_background`,
            // `handle_agent_spawn_foreground`, `handle_agent_resume`) are all dispatched from
            // the explicit `/agent spawn`/`/agent resume` slash command, so `Explicit` is the
            // correct base value here (spec 042, issue #5857). `handle_scheduler_spawn_action`
            // is the sole caller that needs `Autonomous` — it overrides `spawn_ctx.origin`
            // immediately after calling this helper, mirroring how it already overrides
            // `network_denied`/`progress_at` post-construction.
            origin: zeph_subagent::SpawnOrigin::Explicit,
            // #6527: derive a defense-in-depth / tool-visibility narrowing signal from the
            // parent session's own `[tool.permissions]` deny rules. This is NOT the runtime
            // security boundary — every spawned sub-agent's tool executor is a
            // `FilteredToolExecutor` wrapping `Arc::clone(&self.tool_executor)`, which is
            // itself the parent's `TrustGateExecutor`-gated tree (see `agent_setup.rs`
            // `TrustGateExecutor::new(inner, permission_policy.clone())` and `runner.rs`'s
            // `self.tool_executor` wiring). So every child tool call is already re-checked
            // against this same `permission_policy` at call time, regardless of what this
            // field narrows. This field only controls what the child's LLM *sees* in its
            // tool catalog, saving wasted turns on tools that would be denied anyway.
            //
            // INVARIANT (do not break silently): the `None` returns inside
            // `effective_tool_allowlist` (for `ReadOnly` autonomy and for "nothing is
            // wholesale-denied") are safe ONLY because the child inherits the parent's
            // gated executor as described above. If a future refactor gives subagents a
            // fresh/ungated executor (e.g. remote/sandboxed subagents), these `None` returns
            // become real escalation holes — a `ReadOnly` parent would spawn a write-capable
            // child, and an unrestricted-by-rules parent would give the child no runtime
            // gating at all. See `effective_tool_allowlist`'s own doc comment for the full
            // narrowing algorithm and its edge cases.
            inherited_tool_allowlist: self
                .runtime
                .config
                .permission_policy
                .effective_tool_allowlist(
                    self.tool_executor
                        .tool_definitions_erased()
                        .into_iter()
                        .map(|d| zeph_subagent::normalize_tool_id(d.id.as_ref())),
                ),
            ..Default::default()
        }
    }
    /// Compute the parent session's own current effective trust level (issue #6493).
    ///
    /// Mirrors the fold in [`crate::agent::context::assembly`]'s
    /// `apply_skill_trust_and_gating` exactly, so the cap handed to a spawned sub-agent is
    /// always consistent with what is actually enforced on the parent's own tool gate this
    /// turn: the least-trusted level among all skills active this turn, or `Trusted` when no
    /// skill is active.
    fn parent_effective_trust_level(&self) -> zeph_common::SkillTrustLevel {
        if self.services.skill.active_skill_names.is_empty() {
            return zeph_common::SkillTrustLevel::Trusted;
        }
        let snapshot = self.services.skill.trust_snapshot.read();
        self.services
            .skill
            .active_skill_names
            .iter()
            .filter_map(|name| snapshot.get(name).map(|s| s.trust_level))
            .fold(zeph_common::SkillTrustLevel::Trusted, |acc, lvl| {
                acc.min_trust(lvl)
            })
    }
    /// Extract recent parent messages for history propagation (Section 5.7 in spec).
    ///
    /// Filters system messages, applies `context_window_turns` and `max_parent_messages` caps,
    /// applies a 25% context window cap using a 4-chars-per-token heuristic, prunes orphaned
    /// `ToolUse`/`ToolResult` pairs at the slice boundary, and optionally sanitizes text parts
    /// through the IPI pipeline according to `parent_context_policy`.
    fn extract_parent_messages(
        &self,
        config: &zeph_config::SubAgentConfig,
    ) -> Vec<zeph_llm::provider::Message> {
        use zeph_config::ParentContextPolicy;
        use zeph_llm::provider::Role;

        if config.parent_context_policy == ParentContextPolicy::None
            || config.context_window_turns == 0
        {
            return Vec::new();
        }

        let non_system: Vec<_> = self
            .msg
            .messages
            .iter()
            .filter(|m| m.role != Role::System)
            .cloned()
            .collect();

        let take_count = config
            .context_window_turns
            .saturating_mul(2)
            .min(config.max_parent_messages);
        let start = non_system.len().saturating_sub(take_count);
        let mut msgs = non_system[start..].to_vec();

        // Cap at 25% of model context window and prune orphaned tool pairs.
        let max_chars = 128_000usize / 4;
        let requested = msgs.len();
        trim_parent_messages(&mut msgs, max_chars);
        if msgs.len() < requested {
            tracing::info!(
                kept = msgs.len(),
                requested,
                "[subagent] truncated parent history due to token budget or orphan pruning"
            );
        }

        if config.parent_context_policy == ParentContextPolicy::InheritSanitized {
            use zeph_sanitizer::{ContentSource, ContentSourceKind};
            let source =
                ContentSource::new(ContentSourceKind::A2aMessage).with_identifier("parent_history");
            msgs = sanitize_parent_messages(msgs, &self.services.security.sanitizer, &source);
        }

        msgs
    }
    /// Extract MCP tool names from the tool executor for diagnostic annotation.
    fn extract_mcp_tool_names(&self) -> Vec<String> {
        self.tool_executor
            .tool_definitions_erased()
            .into_iter()
            .filter(ToolDef::is_mcp_tool)
            .map(|t| t.id.to_string())
            .collect()
    }
    /// Classify a skill directory's source kind using on-disk markers and the bundled allowlist.
    ///
    /// Must be called from a blocking context (uses synchronous FS I/O).
    pub(super) fn classify_source_kind(
        skill_dir: &std::path::Path,
        managed_dir: Option<&std::path::PathBuf>,
        bundled_names: &std::collections::HashSet<String>,
    ) -> zeph_memory::store::SourceKind {
        if managed_dir.is_some_and(|d| skill_dir.starts_with(d)) {
            let skill_name = skill_dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let has_marker = skill_dir.join(".bundled").exists();
            if has_marker && bundled_names.contains(skill_name) {
                zeph_memory::store::SourceKind::Bundled
            } else {
                if has_marker {
                    tracing::warn!(
                        skill = %skill_name,
                        "skill has .bundled marker but is not in the bundled skill \
                         allowlist — classifying as Hub"
                    );
                }
                zeph_memory::store::SourceKind::Hub
            }
        } else {
            zeph_memory::store::SourceKind::Local
        }
    }
}

/// Outcome of checking the durable-execution gate before a sub-agent spawn (spec-064 §P4).
enum DurableSpawnGate {
    /// Fresh run: wire this seat into `SpawnContext::durable_resolver` so the child resolves
    /// the promise on exit (INV-9 channel rule).
    Fresh(zeph_subagent::DurableResolverSeat),
    /// Resumed run whose child already resolved its promise before the parent crashed. The
    /// caller must skip `spawn` entirely and replay this result instead — spawning here would
    /// duplicate the LLM calls and any side-effecting tool calls the finished child already
    /// performed (#5944). `promise_id` lets the foreground caller claim a one-time replay
    /// notification (#6027) via [`zeph_durable::DurableContext::claim_promise_notification`].
    Replayed {
        result: zeph_subagent::SubagentResult,
        promise_id: zeph_durable::PromiseId,
    },
    /// Gate closed: durable subagent support disabled, a resumed run whose child promise is
    /// still pending (out of v1 scope — see `durable.rs` module docs "Scope boundary"), or an
    /// error (logged at `warn`). The caller degrades to a plain spawn with no durable wiring.
    ///
    /// The still-pending case is safe only because the current architecture is
    /// LocalBackend-only, in-process tokio tasks (spec-064 INV-9): a parent-process crash
    /// necessarily kills its in-process children too, so a still-pending promise on resume
    /// means the original child is genuinely gone, and re-spawning cannot duplicate a live
    /// child. See `durable.rs` "Scope boundary".
    None,
}

/// Check the durable-execution gate for the next sub-agent spawn.
///
/// See [`DurableSpawnGate`] for the three possible outcomes.
async fn resolve_durable_spawn_gate(
    enabled: bool,
    ctx: Option<&zeph_durable::DurableContext>,
) -> DurableSpawnGate {
    let Some(ctx) = ctx.filter(|_| enabled) else {
        return DurableSpawnGate::None;
    };
    let (promise, seat) = match zeph_subagent::make_durable_promise(ctx).await {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!(error = %e, "durable: make_durable_promise failed — degrading to non-durable spawn");
            return DurableSpawnGate::None;
        }
    };
    if let Some(seat) = seat {
        return DurableSpawnGate::Fresh(seat);
    }
    // Resumed: token unrecoverable (INV-9). Check without blocking whether the child already
    // resolved the promise before the crash — replay it instead of re-spawning a duplicate.
    match zeph_subagent::try_replay_durable_subagent(ctx, &promise).await {
        Ok(Some(result)) => DurableSpawnGate::Replayed {
            result,
            promise_id: promise.id(),
        },
        Ok(None) => {
            // Safe to fall back to a plain spawn here only because the current architecture
            // is LocalBackend-only, in-process tokio tasks: the parent process crashing kills
            // its in-process children too, so a still-pending promise on resume means the
            // original child is genuinely gone, not merely unreachable. Re-attaching to a
            // live child would require cross-process liveness detection, which is out of v1
            // scope — see `durable.rs` module docs "Scope boundary" and spec-064 INV-9 (the
            // resolver token is unrecoverable by design, so it cannot be re-minted to attempt
            // reattachment).
            tracing::warn!(
                "durable: resumed sub-agent promise still pending after restart — original \
                 child did not resolve before the crash; re-spawning may duplicate side effects \
                 (#5944 residual v1 gap)"
            );
            DurableSpawnGate::None
        }
        Err(e) => {
            tracing::warn!(error = %e, "durable: replay check failed on resumed sub-agent promise — degrading to non-durable spawn");
            DurableSpawnGate::None
        }
    }
}

/// Estimates the JSON payload size of a single [`zeph_llm::provider::Message`] for token-budget
/// accounting.
///
/// When `parts` is empty the message is a legacy text-only message and `content.len()` is used
/// directly. Otherwise each part is measured individually so that structured variants (images,
/// tool invocations, thinking blocks) are accounted for rather than relying on the already-flat
/// `content` string, which may not reflect the actual API payload size.
pub(crate) fn estimate_parts_size(m: &zeph_llm::provider::Message) -> usize {
    use zeph_llm::provider::MessagePart;
    if m.parts.is_empty() {
        return m.content.len();
    }
    m.parts
        .iter()
        .map(|p| match p {
            MessagePart::Text { text }
            | MessagePart::Recall { text }
            | MessagePart::CodeContext { text }
            | MessagePart::Summary { text }
            | MessagePart::CrossSession { text } => text.len(),
            MessagePart::ToolOutput { body, .. } => body.len(),
            MessagePart::ToolUse { id, name, input } => {
                50 + id.len() + name.len() + input.to_string().len()
            }
            MessagePart::ToolResult {
                tool_use_id,
                content,
                ..
            } => 50 + tool_use_id.len() + content.len(),
            MessagePart::Image(img) => img.data.len() * 4 / 3,
            MessagePart::ThinkingBlock {
                thinking,
                signature,
            } => 50 + thinking.len() + signature.len(),
            MessagePart::RedactedThinkingBlock { data } => data.len(),
            MessagePart::Compaction { summary } => summary.len(),
            _ => 0,
        })
        .sum()
}

/// Applies token-budget truncation and orphaned-tool-pair pruning to a parent message slice.
///
/// Budget truncation keeps the **most recent** messages that fit within `max_chars`
/// (a suffix), so the subagent always receives the freshest context.
///
/// Two passes are performed after budget truncation:
///
/// 1. Remove `ToolResult` parts from user messages whose matching `ToolUse` is no longer in the
///    slice (truncated away).
/// 2. Remove `ToolUse` parts from **interior** assistant messages whose matching `ToolResult`
///    was removed in pass 1 or was already absent. The trailing assistant message is exempt —
///    its unanswered `ToolUse` calls are not orphaned; the slice just ends before the result.
///
/// Messages that become fully empty after pruning are removed from `msgs`.
///
/// `rebuild_content` is called **only** when `retain` actually removed parts — preserving the
/// existing `content` field (and any `ThinkingBlock` text embedded there) for unmodified
/// messages.
pub(crate) fn trim_parent_messages(msgs: &mut Vec<zeph_llm::provider::Message>, max_chars: usize) {
    use zeph_llm::provider::{MessagePart, Role};

    // Token-budget cap: keep the most recent messages that fit within max_chars.
    // We iterate from the end (newest) and drain from the front once the budget is exceeded,
    // so the subagent always receives the most recent context rather than stale early messages.
    let mut total_chars = 0usize;
    let mut drop_before = 0usize; // index of the first message to keep
    for (i, m) in msgs.iter().enumerate().rev() {
        total_chars += estimate_parts_size(m);
        if total_chars > max_chars {
            drop_before = i + 1;
            break;
        }
    }
    if drop_before > 0 {
        msgs.drain(..drop_before);
    }

    // Pass 1: collect ToolUse IDs emitted by assistant messages; prune orphaned ToolResult
    // parts from user messages that reference a ToolUse no longer present in the slice.
    // Use owned Strings to avoid holding immutable borrows across the subsequent mutable loop.
    let emitted_tool_ids: std::collections::HashSet<String> = msgs
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| {
            if let MessagePart::ToolUse { id, .. } = p {
                Some(id.clone())
            } else {
                None
            }
        })
        .collect();

    let mut orphans_removed = 0usize;
    for m in msgs.iter_mut() {
        if m.role != Role::User || m.parts.is_empty() {
            continue;
        }
        let before = m.parts.len();
        m.parts.retain(|p| match p {
            MessagePart::ToolResult { tool_use_id, .. } => {
                emitted_tool_ids.contains(tool_use_id.as_str())
            }
            _ => true,
        });
        let dropped = before - m.parts.len();
        if dropped > 0 {
            orphans_removed += dropped;
            if m.parts.is_empty() {
                m.content.clear();
            } else {
                m.rebuild_content();
            }
        }
    }

    // Pass 2: collect ToolResult IDs present in user messages after pass 1; prune ToolUse
    // parts from assistant messages whose result is confirmed absent.
    //
    // The trailing assistant message is exempt: it may legitimately contain unanswered
    // ToolUse calls (the slice ends before the result arrives). Only interior assistant
    // messages — those followed by at least one user message — can have provably orphaned
    // ToolUse parts (the conversation moved on without answering them).
    let consumed_tool_ids: std::collections::HashSet<String> = msgs
        .iter()
        .filter(|m| m.role == Role::User)
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| {
            if let MessagePart::ToolResult { tool_use_id, .. } = p {
                Some(tool_use_id.clone())
            } else {
                None
            }
        })
        .collect();

    // Index of the last assistant message — exempt from pass 2.
    let last_assistant_idx = msgs
        .iter()
        .enumerate()
        .rev()
        .find(|(_, m)| m.role == Role::Assistant)
        .map(|(i, _)| i);

    for (idx, m) in msgs.iter_mut().enumerate() {
        if m.role != Role::Assistant || m.parts.is_empty() {
            continue;
        }
        // Skip the trailing assistant message — its unanswered ToolUse calls are not orphaned.
        if Some(idx) == last_assistant_idx {
            continue;
        }
        let before = m.parts.len();
        m.parts.retain(|p| match p {
            MessagePart::ToolUse { id, .. } => consumed_tool_ids.contains(id.as_str()),
            _ => true,
        });
        let dropped = before - m.parts.len();
        if dropped > 0 {
            orphans_removed += dropped;
            if m.parts.is_empty() {
                m.content.clear();
            } else {
                m.rebuild_content();
            }
        }
    }

    // Remove messages that were emptied by orphan pruning.
    msgs.retain(|m| !m.content.is_empty() || !m.parts.is_empty());

    if orphans_removed > 0 {
        tracing::debug!(
            orphans = orphans_removed,
            "[subagent] pruned orphaned ToolUse/ToolResult parts from parent context boundary"
        );
    }
}

/// Sanitize text parts of `msgs` through the IPI pipeline.
///
/// Only [`MessagePart::Text`] parts are passed through the sanitizer; structured parts
/// (`ToolUse`, `ToolResult`, `Recall`, `CodeContext`) are left untouched.  After sanitization
/// the message `content` field is rebuilt to stay consistent with the updated parts.
fn sanitize_parent_messages(
    mut msgs: Vec<zeph_llm::provider::Message>,
    sanitizer: &zeph_sanitizer::ContentSanitizer,
    source: &zeph_sanitizer::ContentSource,
) -> Vec<zeph_llm::provider::Message> {
    use zeph_llm::provider::MessagePart;
    for msg in &mut msgs {
        let mut changed = false;
        for part in &mut msg.parts {
            if let MessagePart::Text { text } = part {
                let clean = sanitizer.sanitize(text, source.clone());
                if clean.body != *text {
                    *text = clean.body;
                    changed = true;
                }
            }
        }
        if changed {
            msg.rebuild_content();
        }
    }
    msgs
}

impl<C: Channel + Send + 'static> zeph_commands::SubagentAccess for Agent<C> {
    // ----- /agent, @mention -----

    fn handle_agent_dispatch<'a>(
        &'a mut self,
        input: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Option<String>, zeph_commands::CommandError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            match self.dispatch_agent_command(input).await {
                Some(Err(e)) => Err(zeph_commands::CommandError::new(e.to_string())),
                Some(Ok(())) | None => Ok(None),
            }
        })
    }

    // ----- /agents -----

    fn handle_agents<'a>(
        &'a mut self,
        args: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<String, zeph_commands::CommandError>>
                + Send
                + 'a,
        >,
    > {
        use zeph_commands::handlers::agents_fleet::{FleetEntry, format_fleet_section};
        use zeph_subagent::AgentsCommand;

        let args_owned = args.trim().to_owned();
        Box::pin(async move {
            // Fleet view: bare `/agents` or `/agents fleet` shows autonomous sessions + definitions.
            let show_fleet = args_owned.is_empty() || args_owned == "fleet";

            let fleet_section = if show_fleet {
                let snapshots = self.services.autonomous_registry.list();
                let entries: Vec<FleetEntry> = snapshots
                    .into_iter()
                    .map(|s| FleetEntry {
                        goal_id: s.goal_id,
                        goal_text_short: s.goal_text_short,
                        state: s.state,
                        turns_executed: s.turns_executed,
                        max_turns: s.max_turns,
                        elapsed: s.elapsed,
                    })
                    .collect();
                format_fleet_section(&entries)
            } else {
                String::new()
            };

            // Sub-agent definitions section.
            let definitions_section = if show_fleet || args_owned == "list" {
                self.handle_agents_definitions_list()
            } else {
                // CRUD subcommands: show, create, edit, delete.
                match AgentsCommand::parse(&format!("/agents {args_owned}")) {
                    Ok(cmd) => self.handle_agents_crud(cmd),
                    Err(e) => e.to_string(),
                }
            };

            let mut out = fleet_section;
            if !definitions_section.is_empty() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&definitions_section);
            }

            if out.is_empty() {
                "No active autonomous sessions or sub-agent definitions found."
                    .clone_into(&mut out);
            }

            Ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use zeph_tools::{ErasedToolExecutor, ToolCall};

    use super::*;
    use crate::agent::agent_tests::*;

    // ── resolve_subagent_secret tests (#5941/#5942) ─────────────────────────

    fn agent_with_custom_secret(stored_key: &str, value: &str) -> Agent<MockChannel> {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.services.skill.available_custom_secrets.insert(
            stored_key.to_owned(),
            crate::vault::Secret::new(value.to_owned()),
        );
        agent
    }

    #[test]
    fn resolve_subagent_secret_exact_match() {
        let agent = agent_with_custom_secret("my_key", "the-value");
        let resolved = agent.resolve_subagent_secret("my_key");
        assert_eq!(
            resolved.map(|s| s.expose().to_owned()),
            Some("the-value".to_owned())
        );
    }

    #[test]
    fn resolve_subagent_secret_normalizes_dash_to_underscore() {
        // Stored key is underscored (as produced by ZEPH_SECRET_<NAME> normalization);
        // the sub-agent may request it with dashes instead.
        let agent = agent_with_custom_secret("my_api_key", "dash-value");
        let resolved = agent.resolve_subagent_secret("my-api-key");
        assert_eq!(
            resolved.map(|s| s.expose().to_owned()),
            Some("dash-value".to_owned())
        );
    }

    #[test]
    fn resolve_subagent_secret_normalizes_case() {
        let agent = agent_with_custom_secret("upper_key", "case-value");
        let resolved = agent.resolve_subagent_secret("UPPER_KEY");
        assert_eq!(
            resolved.map(|s| s.expose().to_owned()),
            Some("case-value".to_owned())
        );
    }

    #[test]
    fn resolve_subagent_secret_missing_key_returns_none() {
        let agent = agent_with_custom_secret("known_key", "value");
        assert!(agent.resolve_subagent_secret("unknown_key").is_none());
    }

    #[test]
    fn resolve_subagent_secret_empty_map_returns_none() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let agent = Agent::new(provider, channel, registry, None, 5, executor);
        assert!(agent.resolve_subagent_secret("anything").is_none());
    }

    /// #5712 regression: MCP tool identification must key off `ToolDef::server_id`, not a
    /// `"mcp_"` name prefix that real `McpTool::sanitized_id()` output never produces.
    #[tokio::test]
    async fn extract_mcp_tool_names_uses_server_id_not_name_prefix() {
        use zeph_tools::registry::InvocationHint;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools().with_definitions(vec![
            ToolDef {
                id: "read".into(),
                description: "built-in tool".into(),
                schema: schemars::Schema::default(),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
                server_id: None,
            },
            ToolDef {
                id: "github_create_issue".into(),
                description: "MCP tool".into(),
                schema: schemars::Schema::default(),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
                server_id: Some("github".into()),
            },
        ]);
        let agent = Agent::new(provider, channel, registry, None, 5, executor);

        assert_eq!(agent.extract_mcp_tool_names(), vec!["github_create_issue"]);
    }

    /// Agent with `durable_ctx` populated via the real `ensure_session_durable_ctx` bootstrap
    /// path (mirrors `durable_bootstrap::tests::agent_with_conversation`), with
    /// `durable_subagent` set per `subagent_enabled` — used to test the FR-003/US-002 seat
    /// wiring gate at `resolve_durable_spawn_gate`, not just the config-to-builder plumbing.
    async fn agent_with_durable_ctx_ready(subagent_enabled: bool) -> Agent<MockChannel> {
        let provider = mock_provider(vec!["ok".into()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.services.memory.persistence.conversation_id = Some(zeph_memory::ConversationId(42));
        agent.services.session.durable_agent_turns_config = Some(zeph_config::DurableConfig {
            enabled: true,
            agent_turns: true,
            ..zeph_config::DurableConfig::default()
        });
        agent.services.session.durable_agent_turns_db_url = Some(":memory:".to_owned());
        agent.services.session.durable_subagent = subagent_enabled;

        agent.ensure_session_durable_ctx().await;
        assert!(
            agent.services.session.durable_ctx.is_some(),
            "test setup: durable_ctx must be populated before exercising the seat gate"
        );
        agent
    }

    #[tokio::test]
    async fn seat_wired_when_subagent_enabled_and_durable_ctx_populated() {
        let agent = Box::pin(agent_with_durable_ctx_ready(true)).await;

        let gate = resolve_durable_spawn_gate(
            agent.services.session.durable_subagent,
            agent.services.session.durable_ctx.as_deref(),
        )
        .await;

        assert!(
            matches!(gate, DurableSpawnGate::Fresh(_)),
            "US-002: [durable] subagent=true with a populated durable_ctx must yield a seat, \
             not just wire the config-to-builder plumbing"
        );
    }

    #[tokio::test]
    async fn seat_absent_when_subagent_disabled() {
        let agent = Box::pin(agent_with_durable_ctx_ready(false)).await;

        let gate = resolve_durable_spawn_gate(
            agent.services.session.durable_subagent,
            agent.services.session.durable_ctx.as_deref(),
        )
        .await;

        assert!(
            matches!(gate, DurableSpawnGate::None),
            "FR-008: durable_subagent=false must keep the seat gate closed even when \
             durable_ctx is populated"
        );
    }

    // ── #5944 end-to-end replay regression tests ────────────────────────────
    //
    // These simulate a real parent-process restart: two *separate* `Agent` instances
    // pointed at the same on-disk sqlite durable journal and the same `conversation_id`,
    // so the second instance's `DurableContext` genuinely re-derives the first's
    // `ExecutionId`/`PromiseId` (mirrors `try_replay_durable_subagent_sees_already_resolved_promise_on_resume`
    // in `zeph-subagent/src/durable.rs`, but at the `handle_agent_background`/
    // `handle_agent_spawn_foreground` call-site level rather than the adapter level).

    fn subagent_def(name: &str) -> zeph_subagent::SubAgentDef {
        use zeph_subagent::def::{SkillFilter, SubAgentPermissions, ToolPolicy};
        use zeph_subagent::hooks::SubagentHooks;

        zeph_subagent::SubAgentDef {
            name: name.to_owned(),
            description: "A helper bot".into(),
            model: None,
            tools: ToolPolicy::InheritAll,
            disallowed_tools: vec![],
            permissions: SubAgentPermissions::default(),
            skills: SkillFilter::default(),
            system_prompt: "You are helpful.".into(),
            hooks: SubagentHooks::default(),
            memory: None,
            source: None,
            file_path: None,
        }
    }

    /// Builds an `Agent` wired for durable sub-agent spawns against a real sqlite file at
    /// `db_url`, with a `SubAgentManager` carrying a single "helper" definition so
    /// `handle_agent_background`/`handle_agent_spawn_foreground` can run past the gate check.
    async fn agent_with_durable_and_manager(
        db_url: &str,
        conversation_id: i64,
    ) -> Agent<MockChannel> {
        let provider = mock_provider(vec!["ok".into()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.services.memory.persistence.conversation_id =
            Some(zeph_memory::ConversationId(conversation_id));
        agent.services.session.durable_agent_turns_config = Some(zeph_config::DurableConfig {
            enabled: true,
            agent_turns: true,
            ..zeph_config::DurableConfig::default()
        });
        agent.services.session.durable_agent_turns_db_url = Some(db_url.to_owned());
        agent.services.session.durable_subagent = true;

        let mut mgr = zeph_subagent::SubAgentManager::new(4);
        mgr.definitions_mut().push(subagent_def("helper"));
        agent.services.orchestration.subagent_manager = Some(mgr);

        agent.ensure_session_durable_ctx().await;
        assert!(
            agent.services.session.durable_ctx.is_some(),
            "test setup: durable_ctx must be populated before exercising the handler"
        );
        agent
    }

    #[tokio::test]
    async fn handle_agent_background_replays_finished_child_without_respawning() {
        let dir = tempfile::tempdir().unwrap();
        let db_url = dir.path().join("durable.db").to_string_lossy().into_owned();

        // "Run 1": the child finishes and resolves its promise before the parent crashes.
        let agent1 = Box::pin(agent_with_durable_and_manager(&db_url, 100)).await;
        let ctx1 = agent1.services.session.durable_ctx.clone().unwrap();
        let (_promise1, seat) = zeph_subagent::make_durable_promise(&ctx1).await.unwrap();
        let seat = seat.expect("test setup: run 1 must be fresh and yield a resolver seat");
        let loop_result: Result<String, zeph_subagent::SubAgentError> =
            Ok("child finished before crash".to_owned());
        zeph_subagent::resolve_durable_promise(seat, "task-e2e-01", &loop_result).await;
        agent1
            .services
            .session
            .durable_writer
            .as_ref()
            .unwrap()
            .flush()
            .await
            .unwrap();
        // Drop run 1 to release its process-exclusivity lock on the execution (INV-15, #6122) —
        // a real crash closes the process's file descriptors (and thus the flock) before the
        // restarted parent below re-opens the same execution; without this, run 2's
        // `open_execution_exclusive` would see run 1 as still live and correctly refuse to open.
        drop(agent1);

        // "Run 2": a brand-new `Agent` (simulating the restarted parent) with the same
        // conversation_id and db file re-derives the same promise and must see it resolved.
        let mut agent2 = Box::pin(agent_with_durable_and_manager(&db_url, 100)).await;

        let resp = agent2
            .handle_agent_background("helper", "do work")
            .await
            .unwrap();
        assert!(
            resp.contains("replayed from durable journal"),
            "expected a replay notice, got: {resp}"
        );
        assert!(
            resp.contains("child finished before crash"),
            "expected the journaled output to be surfaced, got: {resp}"
        );
        assert!(
            agent2
                .services
                .orchestration
                .subagent_manager
                .as_ref()
                .unwrap()
                .statuses()
                .is_empty(),
            "mgr.spawn must not be called when the child result is replayed"
        );
    }

    #[tokio::test]
    async fn handle_agent_spawn_foreground_replays_finished_child_without_respawning() {
        let dir = tempfile::tempdir().unwrap();
        let db_url = dir.path().join("durable.db").to_string_lossy().into_owned();

        // "Run 1": the child finishes and resolves its promise before the parent crashes.
        let agent1 = Box::pin(agent_with_durable_and_manager(&db_url, 200)).await;
        let ctx1 = agent1.services.session.durable_ctx.clone().unwrap();
        let (_promise1, seat) = zeph_subagent::make_durable_promise(&ctx1).await.unwrap();
        let seat = seat.expect("test setup: run 1 must be fresh and yield a resolver seat");
        let loop_result: Result<String, zeph_subagent::SubAgentError> =
            Ok("foreground child output".to_owned());
        zeph_subagent::resolve_durable_promise(seat, "task-e2e-02", &loop_result).await;
        // C1 regression guard (#6027): journal a durable step AFTER the promise, exactly the
        // foreground-spawn-followed-by-another-turn topology that triggered the original
        // ReplayDivergence bug (a replay-only `ctx.step()` used to land at this same ordinal
        // position and collide with whatever the fresh run had already recorded there). The
        // `notified_at` claim consumes no step id, so it can never collide with this marker —
        // if it regressed to a step-based mechanism, the assertions below would fail with a
        // `ReplayDivergence` error instead of the expected replayed output.
        ctx1.step(
            zeph_durable::StepDescriptor::idempotent(
                "post_spawn_marker",
                b"post_spawn_marker".to_vec(),
            ),
            |_handle| async move { Ok::<i64, zeph_durable::StepError>(42) },
        )
        .await
        .unwrap();
        agent1
            .services
            .session
            .durable_writer
            .as_ref()
            .unwrap()
            .flush()
            .await
            .unwrap();
        // Drop run 1 to release its process-exclusivity lock on the execution (INV-15, #6122) —
        // see the comment in `handle_agent_background_replays_finished_child_without_respawning`.
        drop(agent1);

        // "Run 2": a brand-new `Agent` re-derives the same promise and must see it resolved,
        // returning the journaled output directly instead of spawning and polling a new child.
        let mut agent2 = Box::pin(agent_with_durable_and_manager(&db_url, 200)).await;

        let resp = agent2
            .handle_agent_spawn_foreground("helper", "do work")
            .await
            .unwrap();
        assert_eq!(resp, "foreground child output");
        assert!(
            agent2
                .channel
                .sent_messages()
                .iter()
                .any(|m| m.contains("replayed from durable journal")),
            "expected the replay notice to be sent to the channel"
        );
        assert_eq!(
            agent2.channel.notify_completed_calls().len(),
            1,
            "expected exactly one TUI completion notification on the first replay"
        );
        assert!(
            agent2
                .services
                .orchestration
                .subagent_manager
                .as_ref()
                .unwrap()
                .statuses()
                .is_empty(),
            "mgr.spawn must not be called when the child result is replayed"
        );
        drop(agent2);

        // "Run 3": the parent restarts *again* after already taking the replay branch once.
        // Per #6027, the channel side effects (notice + completion event) must not re-fire on
        // this second replay — only the first winner of the out-of-band `notified_at` claim
        // fires them; the journaled output is still returned.
        let mut agent3 = Box::pin(agent_with_durable_and_manager(&db_url, 200)).await;

        let resp = agent3
            .handle_agent_spawn_foreground("helper", "do work")
            .await
            .unwrap();
        assert_eq!(resp, "foreground child output");
        assert!(
            !agent3
                .channel
                .sent_messages()
                .iter()
                .any(|m| m.contains("replayed from durable journal")),
            "replay notice must not re-fire on a second replay after a parent restart"
        );
        assert!(
            agent3.channel.notify_completed_calls().is_empty(),
            "TUI completion event must not re-fire on a second replay after a parent restart"
        );
    }

    #[tokio::test]
    async fn handle_agent_background_resumed_still_pending_falls_back_to_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let db_url = dir.path().join("durable.db").to_string_lossy().into_owned();

        // "Run 1": the promise is created (child spawned) but never resolved — simulates a
        // child that was still genuinely running (or lost) when the parent crashed.
        let agent1 = Box::pin(agent_with_durable_and_manager(&db_url, 300)).await;
        let ctx1 = agent1.services.session.durable_ctx.clone().unwrap();
        let (_promise1, seat) = zeph_subagent::make_durable_promise(&ctx1).await.unwrap();
        assert!(
            seat.is_some(),
            "test setup: run 1 must be fresh and yield a resolver seat"
        );
        agent1
            .services
            .session
            .durable_writer
            .as_ref()
            .unwrap()
            .flush()
            .await
            .unwrap();
        // Drop run 1 to release its process-exclusivity lock on the execution (INV-15, #6122) —
        // see the comment in `handle_agent_background_replays_finished_child_without_respawning`.
        drop(agent1);

        // "Run 2": resumed execution observes the same promise still pending — per the
        // documented v1 scope boundary (INV-9: no way to recover an orphaned resolver token)
        // the gate must degrade to a plain spawn rather than replay or block indefinitely.
        let mut agent2 = Box::pin(agent_with_durable_and_manager(&db_url, 300)).await;

        let resp = agent2
            .handle_agent_background("helper", "do work")
            .await
            .unwrap();
        assert!(
            resp.contains("started in background"),
            "still-pending resumed promise must fall back to a normal spawn, got: {resp}"
        );
        assert_eq!(
            agent2
                .services
                .orchestration
                .subagent_manager
                .as_ref()
                .unwrap()
                .statuses()
                .len(),
            1,
            "exactly one real spawn must occur on the still-pending fallback path"
        );
    }

    // ── build_spawn_context: debug_dump_sink wiring (#6391) ─────────────────

    #[test]
    fn build_spawn_context_leaves_debug_dump_sink_none_without_dumper() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let agent = Agent::new(
            provider,
            channel,
            registry,
            None,
            5,
            MockToolExecutor::no_tools(),
        );

        let ctx = agent.build_spawn_context(&zeph_config::SubAgentConfig::default());
        assert!(
            ctx.debug_dump_sink.is_none(),
            "no DebugDumper configured, so SpawnContext must carry no sink"
        );
    }

    #[tokio::test]
    async fn build_spawn_context_wires_debug_dump_sink_when_dumper_present() {
        let dir = tempfile::tempdir().unwrap();
        let dumper =
            crate::debug_dump::DebugDumper::new(dir.path(), crate::debug_dump::DumpFormat::Raw)
                .unwrap();

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let mut agent = Agent::new(
            provider,
            channel,
            registry,
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.runtime.debug.debug_dumper = Some(dumper);

        let ctx = agent.build_spawn_context(&zeph_config::SubAgentConfig::default());
        let sink = ctx
            .debug_dump_sink
            .expect("a configured DebugDumper must be threaded into SpawnContext");

        // Exercise the sink through the trait, same as `zeph-subagent`'s agent loop would —
        // proves the wiring produces a working `Arc<dyn DebugDumpSink>`, not just `Some(_)`.
        let id = sink.dump_request("mock", &[], &[], serde_json::Value::Null);
        sink.dump_response(id, &zeph_llm::provider::ChatResponse::Text("ok".into()));
    }

    // ── build_spawn_context: inherited_tool_allowlist wiring (#6527) ────────

    #[test]
    fn build_spawn_context_leaves_inherited_tool_allowlist_none_by_default() {
        // Default PermissionPolicy has no rules, so no tool is wholesale-denied — must
        // stay None, not Some(full universe) (§2a: would freeze InheritAll children).
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools().with_definitions(vec![ToolDef {
            id: "bash".into(),
            description: "shell".into(),
            schema: schemars::Schema::default(),
            invocation: zeph_tools::registry::InvocationHint::ToolCall,
            output_schema: None,
            server_id: None,
        }]);
        let agent = Agent::new(provider, channel, registry, None, 5, executor);

        let ctx = agent.build_spawn_context(&zeph_config::SubAgentConfig::default());
        assert!(ctx.inherited_tool_allowlist.is_none());
    }

    #[test]
    fn build_spawn_context_populates_inherited_tool_allowlist_from_parent_policy() {
        // A wholesale-Deny rule on the parent's own PermissionPolicy must narrow
        // SpawnContext::inherited_tool_allowlist, dropping the denied tool but keeping
        // everything else in the parent's tool universe.
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools().with_definitions(vec![
            ToolDef {
                id: "bash".into(),
                description: "shell".into(),
                schema: schemars::Schema::default(),
                invocation: zeph_tools::registry::InvocationHint::ToolCall,
                output_schema: None,
                server_id: None,
            },
            ToolDef {
                id: "read".into(),
                description: "read a file".into(),
                schema: schemars::Schema::default(),
                invocation: zeph_tools::registry::InvocationHint::ToolCall,
                output_schema: None,
                server_id: None,
            },
        ]);
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let mut rules = std::collections::HashMap::new();
        rules.insert(
            "bash".to_owned(),
            vec![zeph_config::tools::PermissionRule {
                pattern: "*".to_owned(),
                action: zeph_config::tools::PermissionAction::Deny,
            }],
        );
        agent.runtime.config.permission_policy = zeph_tools::PermissionPolicy::new(rules)
            .with_autonomy(zeph_config::tools::AutonomyLevel::Supervised);

        let ctx = agent.build_spawn_context(&zeph_config::SubAgentConfig::default());
        let allowlist = ctx
            .inherited_tool_allowlist
            .expect("a wholesale-denied bash tool must produce a narrowed Some(set)");
        assert!(!allowlist.contains("bash"));
        assert!(allowlist.contains("read"));
    }

    // ── build_spawn_context: trust-level constraint propagation (#6493) ─────

    #[test]
    fn build_spawn_context_leaves_max_trust_level_trusted_when_no_active_skills() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let agent = Agent::new(
            provider,
            channel,
            registry,
            None,
            5,
            MockToolExecutor::no_tools(),
        );

        let ctx = agent.build_spawn_context(&zeph_config::SubAgentConfig::default());
        assert_eq!(
            ctx.max_trust_level,
            Some(zeph_common::SkillTrustLevel::Trusted),
            "with no active skills this turn, the parent's own effective trust is Trusted, \
             so the cap must impose no additional restriction"
        );
    }

    #[test]
    fn build_spawn_context_caps_trust_to_least_trusted_active_skill() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let mut agent = Agent::new(
            provider,
            channel,
            registry,
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.services.skill.active_skill_names = vec!["trusted-skill".into(), "evil-skill".into()];
        agent.services.skill.trust_snapshot.write().insert(
            "trusted-skill".into(),
            crate::skill_invoker::SkillTrustSnapshot {
                trust_level: zeph_common::SkillTrustLevel::Trusted,
                requires_trust_check: false,
                blake3_hash: String::new(),
            },
        );
        agent.services.skill.trust_snapshot.write().insert(
            "evil-skill".into(),
            crate::skill_invoker::SkillTrustSnapshot {
                trust_level: zeph_common::SkillTrustLevel::Quarantined,
                requires_trust_check: false,
                blake3_hash: String::new(),
            },
        );

        let ctx = agent.build_spawn_context(&zeph_config::SubAgentConfig::default());
        assert_eq!(
            ctx.max_trust_level,
            Some(zeph_common::SkillTrustLevel::Quarantined),
            "the cap must be the LEAST-trusted of all active skills this turn (weakest-link), \
             matching the fold `apply_skill_trust_and_gating` applies to the parent's own gate"
        );
    }

    /// Records every `set_effective_trust` call — unlike `MockToolExecutor`, which falls
    /// through to the trait's no-op default. Used by
    /// [`spawning_a_subagent_caps_trust_to_parent_effective_level`] to observe the trust level
    /// that actually reached the sub-agent's tool executor through the REAL production spawn
    /// path (`handle_agent_background` → `build_spawn_context` → `SubAgentManager::spawn` →
    /// `FilteredToolExecutor::set_effective_trust` → this executor, the same `Arc` the parent
    /// itself uses), not a hand-built `SpawnContext` in a unit test.
    #[derive(Default)]
    struct TrustRecordingExecutor {
        recorded: Arc<Mutex<Option<zeph_tools::SkillTrustLevel>>>,
    }

    impl ToolExecutor for TrustRecordingExecutor {
        async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }

        fn set_skill_env(&self, _env: Option<std::collections::HashMap<String, String>>) {}

        fn set_effective_trust(&self, level: zeph_tools::SkillTrustLevel) {
            *self.recorded.lock().unwrap() = Some(level);
        }

        zeph_tools::tool_executor_no_inner_defaults!();
    }

    #[tokio::test]
    async fn spawning_a_subagent_caps_trust_to_parent_effective_level() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = TrustRecordingExecutor::default();
        let recorded = Arc::clone(&executor.recorded);
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let mut mgr = zeph_subagent::SubAgentManager::new(4);
        mgr.definitions_mut().push(subagent_def("helper"));
        agent.services.orchestration.subagent_manager = Some(mgr);

        // Parent's own current trust is restricted this turn by an active Quarantined skill.
        agent.services.skill.active_skill_names = vec!["evil-skill".into()];
        agent.services.skill.trust_snapshot.write().insert(
            "evil-skill".into(),
            crate::skill_invoker::SkillTrustSnapshot {
                trust_level: zeph_common::SkillTrustLevel::Quarantined,
                requires_trust_check: false,
                blake3_hash: String::new(),
            },
        );

        let resp = agent.handle_agent_background("helper", "do work").await;
        assert!(
            resp.is_some_and(|r| r.contains("started in background")),
            "test setup: the real production spawn path must succeed"
        );

        assert_eq!(
            *recorded.lock().unwrap(),
            Some(zeph_tools::SkillTrustLevel::Quarantined),
            "a sub-agent spawned while the parent's own effective trust is Quarantined must \
             never receive a higher (Trusted) effective trust on its own tool executor — \
             #6493's escalation gap"
        );
    }

    // ── S2 (#6527 critic): spawned sub-agent shares the parent's gated executor ──

    /// Records every tool call it receives via a shared counter cloned out *before*
    /// `Agent::new` takes ownership of the executor. Only a literal `Arc::clone` of this
    /// same allocation (not a fresh/ungated executor of the same shape) can increment the
    /// caller's copy of the counter — proving the sub-agent's tool call reached the exact
    /// same executor instance the parent itself holds.
    #[derive(Default)]
    struct RecordingExecutor {
        calls: Mutex<u32>,
    }

    impl ToolExecutor for RecordingExecutor {
        async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }

        async fn execute_tool_call(
            &self,
            call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            *self.calls.lock().unwrap() += 1;
            Ok(Some(ToolOutput {
                tool_name: call.tool_id.clone(),
                summary: "ran".into(),
                blocks_executed: 1,
                ..Default::default()
            }))
        }

        fn set_skill_env(&self, _env: Option<std::collections::HashMap<String, String>>) {}

        zeph_tools::tool_executor_no_inner_defaults!();
    }

    #[tokio::test]
    async fn spawning_a_subagent_tool_call_reaches_parents_own_executor() {
        // Backs the invariant comment on `build_spawn_context`'s `inherited_tool_allowlist`
        // wiring: `effective_tool_allowlist`'s `None` returns are safe only because the
        // child's tool calls are re-checked by whatever gates the parent's own
        // `self.tool_executor` (a `TrustGateExecutor` in production). This test proves the
        // production spawn path (`handle_agent_background` → `SubAgentManager::spawn` →
        // `FilteredToolExecutor` wrapping `Arc::clone(&self.tool_executor)`) really does
        // route the child's tool call through the SAME executor allocation the parent
        // holds, not a fresh/ungated one.
        use zeph_llm::provider::{ChatResponse, ToolUseRequest};

        let (mock, _counter) = MockProvider::default().with_tool_use(vec![
            ChatResponse::ToolUse {
                text: None,
                tool_calls: vec![ToolUseRequest {
                    id: "call-1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "echo hi"}),
                }],
                thinking_blocks: vec![],
            },
            ChatResponse::Text("final answer".into()),
        ]);

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let recorder = Arc::new(RecordingExecutor::default());
        let mut agent = Agent::new(
            AnyProvider::Mock(mock),
            channel,
            registry,
            None,
            5,
            RecordingExecutor::default(),
        );
        // Replace with the tracked Arc so the test can observe calls made against the exact
        // instance the production spawn path clones via `Arc::clone(&self.tool_executor)`.
        agent.tool_executor = Arc::clone(&recorder) as Arc<dyn ErasedToolExecutor>;

        let mut mgr = zeph_subagent::SubAgentManager::new(4);
        mgr.definitions_mut().push(subagent_def("helper"));
        agent.services.orchestration.subagent_manager = Some(mgr);

        let resp = agent.handle_agent_background("helper", "do work").await;
        assert!(
            resp.is_some_and(|r| r.contains("started in background")),
            "test setup: the real production spawn path must succeed"
        );

        for _ in 0..50 {
            if !agent.poll_subagents().await.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        assert!(
            *recorder.calls.lock().unwrap() >= 1,
            "the sub-agent's tool call must reach the parent's own tool executor instance, \
             proving no fresh/ungated executor is substituted for the child"
        );
    }

    // ── filtered_skills_for token-budget cap (#6421) ─────────────────────────

    /// Builds a `SkillRegistry` with `count` skills on disk, each named `skill-N` with a body
    /// made of `words_per_skill` repeated words — enough real text that `TokenCounter` charges
    /// a nontrivial, predictable-in-sign (if not exact) token count per skill.
    ///
    /// Returns the backing `TempDir` alongside the registry: skill bodies are loaded lazily
    /// from disk on first access (`SkillRegistry::skill`/`body`), so the caller must keep the
    /// directory alive for as long as the registry is used, not just during `load`.
    fn registry_with_skills(
        count: usize,
        words_per_skill: usize,
    ) -> (SkillRegistry, tempfile::TempDir) {
        let temp_dir = tempfile::tempdir().unwrap();
        for i in 0..count {
            let skill_dir = temp_dir.path().join(format!("skill-{i}"));
            std::fs::create_dir(&skill_dir).unwrap();
            let body = "lorem ".repeat(words_per_skill);
            std::fs::write(
                skill_dir.join("SKILL.md"),
                format!("---\nname: skill-{i}\ndescription: Test skill {i}\n---\n{body}"),
            )
            .unwrap();
        }
        let registry = SkillRegistry::load(&[temp_dir.path().to_path_buf()]);
        (registry, temp_dir)
    }

    fn agent_with_skill_registry_and_def(
        registry: SkillRegistry,
        def: zeph_subagent::SubAgentDef,
    ) -> Agent<MockChannel> {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let mut mgr = zeph_subagent::SubAgentManager::new(4);
        mgr.definitions_mut().push(def);
        agent.services.orchestration.subagent_manager = Some(mgr);
        agent
    }

    fn agent_with_skill_registry_and_helper_def(registry: SkillRegistry) -> Agent<MockChannel> {
        agent_with_skill_registry_and_def(registry, subagent_def("helper"))
    }

    #[test]
    fn filtered_skills_for_under_budget_returns_all_bodies_no_marker() {
        // Default `SkillFilter` (empty include/exclude) inherits every registry skill —
        // the #6421 scenario — but the budget here is generous enough that nothing is cut.
        let (registry, _temp_dir) = registry_with_skills(3, 20);
        let mut agent = agent_with_skill_registry_and_helper_def(registry);
        agent.services.skill.subagent_skill_token_budget = 1_000_000;

        let bodies = agent
            .filtered_skills_for("helper")
            .expect("3 skills with a huge budget must return Some");

        assert_eq!(
            bodies.len(),
            3,
            "no truncation marker expected when everything fits under budget"
        );
        for body in &bodies {
            assert!(
                body.contains("lorem"),
                "every returned entry must be a real skill body, not a marker: {body}"
            );
        }
    }

    #[test]
    fn filtered_skills_for_over_budget_truncates_with_marker() {
        // 5 skills, each with a large body; a tiny budget forces truncation well before the
        // full set is accumulated.
        let (registry, _temp_dir) = registry_with_skills(5, 500);
        let mut agent = agent_with_skill_registry_and_helper_def(registry);
        agent.services.skill.subagent_skill_token_budget = 10;

        let bodies = agent
            .filtered_skills_for("helper")
            .expect("at least the first skill must always be included");

        let marker_count = bodies
            .iter()
            .filter(|b| b.starts_with("[skill budget:"))
            .count();
        assert_eq!(
            marker_count, 1,
            "exactly one truncation marker entry must be appended, got bodies: {bodies:?}"
        );
        let marker = bodies
            .iter()
            .find(|b| b.starts_with("[skill budget:"))
            .unwrap();
        let included = bodies.len() - 1;
        assert!(
            included < 5,
            "budget=10 tokens must not fit all 5 large skills, included={included}"
        );
        assert!(
            included >= 1,
            "the first skill must always be included even when it alone exceeds the budget, \
             got included={included}"
        );
        assert!(
            bodies[0].contains("lorem"),
            "the always-included first entry must be a real skill body, not the marker: {}",
            bodies[0]
        );
        assert!(
            marker.contains(&format!("{included}/5 skills included")),
            "marker must report the correct included/total count: {marker}"
        );
        assert!(
            marker.contains("budget=10 tokens"),
            "marker must report the configured budget: {marker}"
        );
    }

    #[test]
    fn filtered_skills_for_mid_budget_greedily_fills_multiple_fitting_skills() {
        // 5 identical-body skills so each costs exactly the same token count T (computed via the
        // same TokenCounter the fix uses). A budget of `2*T + 1` fits skill-0 and skill-1 exactly
        // (running total 2T <= budget) but not a 3rd (3T > budget) — this exercises the greedy
        // `running_tokens + skill_tokens > budget` accumulation for a *fitting* 2nd skill, not
        // just the always-included first one (S2: the over-budget test alone never reaches this
        // arithmetic since its budget is too small to fit even a 2nd skill).
        let (registry, _temp_dir) = registry_with_skills(5, 500);
        let single_body = "lorem ".repeat(500);
        let per_skill_tokens = zeph_memory::TokenCounter::new().count_tokens(&single_body);
        assert!(
            per_skill_tokens > 1,
            "test setup: per-skill token count must be large enough for 2*T+1 to exclude a 3rd \
             skill, got {per_skill_tokens}"
        );

        let mut agent = agent_with_skill_registry_and_helper_def(registry);
        agent.services.skill.subagent_skill_token_budget = 2 * per_skill_tokens + 1;

        let bodies = agent
            .filtered_skills_for("helper")
            .expect("at least the first skill must always be included");

        let marker = bodies
            .iter()
            .find(|b| b.starts_with("[skill budget:"))
            .unwrap_or_else(|| panic!("expected a truncation marker, got bodies: {bodies:?}"));
        let included = bodies.len() - 1;
        assert_eq!(
            included, 2,
            "budget=2*T+1 must fit exactly 2 of the 5 identical-cost skills, got {included}"
        );
        assert!(
            marker.contains("2/5 skills included"),
            "marker must report the correct included/total count: {marker}"
        );
        // Registry order is alphabetical by skill directory name (skill-0..skill-4), so the
        // 2 included skills are skill-0/skill-1 and the 3 omitted are skill-2/3/4 — assert the
        // marker's omitted-name list matches exactly, not just the count (closes Gap 3).
        let omitted_segment = marker
            .split("omitted: ")
            .nth(1)
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or_else(|| panic!("marker missing 'omitted: ...]' segment: {marker}"));
        let mut omitted_names: Vec<&str> = omitted_segment.split(", ").collect();
        omitted_names.sort_unstable();
        assert_eq!(
            omitted_names,
            vec!["skill-2", "skill-3", "skill-4"],
            "marker must name exactly the 3 truncated skills, got marker: {marker}"
        );
    }

    #[test]
    fn filtered_skills_for_explicit_include_is_never_capped() {
        // S1 (scope decision): the budget cap applies only to the empty-include "inherit
        // everything" case #6421 is about. A definition with an explicit, hand-curated
        // `skills.include` list must be returned uncapped even when its total size would
        // otherwise exceed the configured budget — the operator opted into that set on purpose.
        use zeph_subagent::def::{SkillFilter, SubAgentPermissions, ToolPolicy};
        use zeph_subagent::hooks::SubagentHooks;

        let (registry, _temp_dir) = registry_with_skills(5, 500);
        let def = zeph_subagent::SubAgentDef {
            name: "curated".to_owned(),
            description: "A curated helper".into(),
            model: None,
            tools: ToolPolicy::InheritAll,
            disallowed_tools: vec![],
            permissions: SubAgentPermissions::default(),
            skills: SkillFilter {
                include: vec!["skill-*".to_owned()],
                exclude: vec![],
            },
            system_prompt: "You are helpful.".into(),
            hooks: SubagentHooks::default(),
            memory: None,
            source: None,
            file_path: None,
        };
        let mut agent = agent_with_skill_registry_and_def(registry, def);
        // Budget far too small to fit all 5 skills — would definitely truncate the empty-include
        // path, but must have zero effect here.
        agent.services.skill.subagent_skill_token_budget = 10;

        let bodies = agent
            .filtered_skills_for("curated")
            .expect("explicit include must still match all 5 skill-* skills");

        assert_eq!(
            bodies.len(),
            5,
            "explicit include list must never be truncated by the budget, got: {bodies:?}"
        );
        assert!(
            bodies.iter().all(|b| b.contains("lorem")),
            "every entry must be a real skill body, not a truncation marker: {bodies:?}"
        );
    }
}
