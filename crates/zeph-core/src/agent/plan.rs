// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use tokio_util::sync::CancellationToken;
use zeph_llm::provider::LlmProvider;

use super::Agent;
use super::error;

pub(super) fn format_plan_summary(graph: &zeph_orchestration::TaskGraph) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "Plan: \"{}\"", graph.goal);
    let _ = writeln!(out, "Tasks: {}", graph.tasks.len());
    let _ = writeln!(out);
    for (i, task) in graph.tasks.iter().enumerate() {
        let deps = if task.depends_on.is_empty() {
            String::new()
        } else {
            let ids: Vec<String> = task.depends_on.iter().map(ToString::to_string).collect();
            format!(" (after: {})", ids.join(", "))
        };
        let agent = task.agent_hint.as_deref().unwrap_or("-");
        let _ = writeln!(out, "  {}. [{}] {}{}", i + 1, agent, task.title, deps);
    }
    out
}

/// Render the `/plan status` message for an active graph: a status-specific summary line,
/// plus (issue #6390) any Command-handoff `goto` rejections recorded on
/// `TaskNode::handoff_rejected` (spec-080) — previously persisted and logged but not
/// surfaced on any CLI/TUI display, leaving an operator to log-dive or query graph state
/// manually to notice a dropped routing intent. The task itself stays `Completed` with its
/// real output preserved; this section only surfaces that the *extra* routing never fired.
pub(super) fn format_plan_status(graph: &zeph_orchestration::TaskGraph) -> String {
    use zeph_orchestration::GraphStatus;

    let base = match graph.status {
        GraphStatus::Created => {
            "A plan is awaiting confirmation. Type `/plan confirm` to execute or `/plan cancel` to abort."
        }
        GraphStatus::Running => "Plan is currently running.",
        GraphStatus::Paused => {
            "Plan is paused. Use `/plan resume` to continue or `/plan cancel` to abort."
        }
        GraphStatus::Failed => {
            "Plan failed. Use `/plan retry` to retry or `/plan cancel` to discard."
        }
        GraphStatus::Completed => "Plan completed successfully.",
        GraphStatus::Canceled => "Plan was canceled.",
        _ => "Plan is in an unknown state.",
    };

    let rejected: Vec<String> = graph
        .tasks
        .iter()
        .filter_map(|t| {
            t.handoff_rejected
                .as_deref()
                .map(|reason| format!("  - Task {} \"{}\": {reason}", t.id, t.title))
        })
        .collect();
    if rejected.is_empty() {
        base.to_owned()
    } else {
        format!(
            "{base}\n\nRejected Command handoff(s):\n{}",
            rejected.join("\n")
        )
    }
}

pub(super) fn collect_and_truncate_task_outputs(
    graph: &zeph_orchestration::TaskGraph,
    max_tokens: u32,
) -> String {
    use zeph_orchestration::TaskStatus;

    let char_budget = max_tokens as usize * 4;
    let mut raw = String::new();
    for task in &graph.tasks {
        if task.status == TaskStatus::Completed
            && let Some(ref result) = task.result
        {
            if !raw.is_empty() {
                raw.push('\n');
            }
            raw.push_str(&result.output);
        }
    }
    if raw.len() > char_budget {
        tracing::warn!(
            original_len = raw.len(),
            truncated_to = char_budget,
            "whole-plan verify: output truncated to verify_max_tokens * 4 chars"
        );
        raw.chars().take(char_budget).collect()
    } else {
        raw
    }
}

impl<C: crate::channel::Channel> Agent<C> {
    pub(super) fn config_for_orchestration(&self) -> &crate::config::OrchestrationConfig {
        &self.services.orchestration.orchestration_config
    }

    pub(super) async fn init_plan_cache_if_needed(&mut self) {
        let plan_cache_config = self
            .services
            .orchestration
            .orchestration_config
            .plan_cache
            .clone();
        if !plan_cache_config.enabled || self.services.orchestration.plan_cache.is_some() {
            return;
        }
        if let Some(ref memory) = self.services.memory.persistence.memory {
            let pool = memory.sqlite().pool().clone();
            let embed_model = self.services.skill.embedding_model.clone();
            match zeph_orchestration::PlanCache::new(pool, plan_cache_config, &embed_model).await {
                Ok(cache) => self.services.orchestration.plan_cache = Some(cache),
                Err(e) => {
                    tracing::warn!(error = %e, "plan cache: init failed, proceeding without cache");
                }
            }
        } else {
            tracing::warn!("plan cache: memory not configured, proceeding without cache");
        }
    }

    pub(super) async fn goal_embedding_for_cache(&mut self, goal: &str) -> Option<Vec<f32>> {
        use zeph_orchestration::normalize_goal;

        self.services.orchestration.plan_cache.as_ref()?;
        let normalized = normalize_goal(goal);
        // Clone provider before .await so &self is not held across the await boundary.
        let provider = self.embedding_provider.clone();
        match provider.embed(&normalized).await {
            Ok(emb) => Some(emb),
            Err(zeph_llm::LlmError::EmbedUnsupported { .. }) => {
                tracing::debug!(
                    "plan cache: provider does not support embeddings, skipping cache lookup"
                );
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, "plan cache: goal embedding failed, skipping cache");
                None
            }
        }
    }

    pub(super) async fn validate_pending_graph(
        &mut self,
        graph: zeph_orchestration::TaskGraph,
    ) -> Result<zeph_orchestration::TaskGraph, ()> {
        use zeph_orchestration::GraphStatus;

        if self.services.orchestration.subagent_manager.is_none() {
            let _ = self
                .channel
                .send(
                    "No sub-agents configured. Add sub-agent definitions to config \
                     to enable plan execution.",
                )
                .await;
            self.services.orchestration.pending_graph = Some(graph);
            return Err(());
        }

        if graph.tasks.is_empty() {
            let _ = self.channel.send("Plan has no tasks.").await;
            self.services.orchestration.pending_graph = Some(graph);
            return Err(());
        }

        if matches!(graph.status, GraphStatus::Completed | GraphStatus::Canceled) {
            let _ = self
                .channel
                .send(&format!(
                    "Cannot re-execute a {} plan. Use `/plan <goal>` to create a new one.",
                    graph.status
                ))
                .await;
            self.services.orchestration.pending_graph = Some(graph);
            return Err(());
        }

        Ok(graph)
    }

    fn build_admission_gate(&self) -> Option<zeph_orchestration::AdmissionGate> {
        let pairs: Vec<(String, usize)> = self
            .runtime
            .providers
            .provider_pool
            .iter()
            .filter_map(|e| e.max_concurrent.map(|c| (e.effective_name(), c as usize)))
            .collect();
        if pairs.is_empty() {
            None
        } else {
            Some(zeph_orchestration::AdmissionGate::new(&pairs))
        }
    }

    pub(super) fn build_dag_scheduler(
        &mut self,
        graph: zeph_orchestration::TaskGraph,
        durable_budget: Option<zeph_orchestration::durable::ReplanBudgetSnapshot>,
    ) -> Result<(zeph_orchestration::DagScheduler, usize), error::AgentError> {
        use zeph_orchestration::{DagScheduler, GraphStatus, RuleBasedRouter};

        let available_agents = self
            .services
            .orchestration
            .subagent_manager
            .as_ref()
            .map(|m| m.definitions().to_vec())
            .unwrap_or_default();

        let max_concurrent = self.services.orchestration.subagent_config.max_concurrent;
        let max_parallel = self
            .services
            .orchestration
            .orchestration_config
            .max_parallel as usize;
        if max_concurrent < max_parallel + 1 {
            tracing::warn!(
                max_concurrent,
                max_parallel,
                "max_concurrent < max_parallel + 1: orchestration tasks may be starved by \
                 planning-phase sub-agents; recommend setting max_concurrent >= {}",
                max_parallel + 1
            );
        }

        let reserved = max_parallel.min(max_concurrent.saturating_sub(1));
        if let Some(mgr) = self.services.orchestration.subagent_manager.as_mut() {
            mgr.reserve_slots(reserved);
        }

        let admission_gate = self.build_admission_gate();

        let sanitizer_arc: std::sync::Arc<dyn zeph_common::OutputSanitizer> =
            std::sync::Arc::new(self.services.security.sanitizer.clone());

        let scheduler = if graph.status == GraphStatus::Created {
            DagScheduler::new(
                graph,
                &self.services.orchestration.orchestration_config,
                Box::new(RuleBasedRouter),
                available_agents,
                admission_gate,
            )
        } else if let Some(snap) = durable_budget {
            DagScheduler::resume_from_durable(
                graph,
                &self.services.orchestration.orchestration_config,
                Box::new(RuleBasedRouter),
                available_agents,
                admission_gate,
                snap,
            )
        } else {
            DagScheduler::resume_from(
                graph,
                &self.services.orchestration.orchestration_config,
                Box::new(RuleBasedRouter),
                available_agents,
                admission_gate,
            )
        }
        .map(|s| s.with_sanitizer(sanitizer_arc))
        .map_err(|e| {
            if let Some(mgr) = self.services.orchestration.subagent_manager.as_mut() {
                mgr.release_reservation(reserved);
            }
            error::OrchestrationFailure::SchedulerInit(e.to_string())
        })?;

        let provider_names: Vec<&str> = self
            .runtime
            .providers
            .provider_pool
            .iter()
            .filter_map(|e| e.name.as_deref())
            .collect();
        scheduler
            .validate_verify_config(&provider_names)
            .map_err(|e| {
                if let Some(mgr) = self.services.orchestration.subagent_manager.as_mut() {
                    mgr.release_reservation(reserved);
                }
                error::OrchestrationFailure::VerifyConfig(e.to_string())
            })?;

        // M1: warn-only validation for orchestrator_provider (typos silently fall back at runtime).
        let op = self
            .services
            .orchestration
            .orchestration_config
            .orchestrator_provider
            .as_str();
        if !op.is_empty() && !provider_names.contains(&op) {
            tracing::warn!(
                provider = op,
                "orchestration.orchestrator_provider not found in [[llm.providers]]; \
                 will fall back to primary provider"
            );
        }

        Ok((scheduler, reserved))
    }

    /// Ensure the durable backend for P2 budget snapshots is open, initialising it lazily on
    /// the first call.  Returns `(Arc<DurableBackendEnum>, JournalWriterHandle)` or `None` when
    /// durable is disabled / not configured.
    async fn ensure_durable_backend(
        &mut self,
    ) -> Option<(
        std::sync::Arc<zeph_durable::DurableBackendEnum>,
        zeph_durable::JournalWriterHandle,
    )> {
        if self.services.orchestration.durable_backend.is_none() {
            let cfg = self.services.orchestration.durable_config.clone()?;
            if !cfg.enabled || !cfg.orchestration {
                return None;
            }
            let db_url = self.services.orchestration.durable_db_url.clone()?;
            let cipher = self.services.orchestration.durable_cipher.clone();
            let hmac_key = self.services.orchestration.durable_hmac_key;
            let hwm_key = self.services.orchestration.durable_hwm_key;
            let (backend, handle, task_handle) =
                crate::agent::durable_bootstrap::open_durable_backend(
                    &self.runtime.lifecycle.task_supervisor,
                    "agent.durable.journal_writer",
                    &cfg,
                    &db_url,
                    cipher,
                    hmac_key,
                    hwm_key,
                )
                .await?;
            self.services.orchestration.durable_backend = Some(backend);
            self.services.orchestration.durable_writer = Some(handle);
            self.services.orchestration.durable_writer_task = Some(task_handle);
        }
        let backend = self.services.orchestration.durable_backend.clone()?;
        let writer = self.services.orchestration.durable_writer.clone()?;
        Some((backend, writer))
    }

    /// Attempt to restore the durable replan budget for a graph that is being resumed.
    ///
    /// Returns `Some(snapshot)` when durable is enabled, the graph is being resumed (not
    /// Created), and a snapshot was found in the journal. Returns `None` in all other cases
    /// so the caller falls back to zeroing counters — identical to pre-durable behaviour.
    async fn try_restore_durable_budget(
        &mut self,
        graph: &zeph_orchestration::TaskGraph,
    ) -> Option<zeph_orchestration::durable::ReplanBudgetSnapshot> {
        use zeph_orchestration::GraphStatus;

        if graph.status == GraphStatus::Created {
            return None;
        }
        if !durable_orchestration_enabled(self.services.orchestration.durable_config.as_ref()) {
            return None;
        }
        let (backend, _writer) = self.ensure_durable_backend().await?;
        let generation = graph.durable_save_generation;
        match zeph_orchestration::durable::restore_budget(&graph.id, generation, backend).await {
            Ok(snap) => snap,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    graph_id = %graph.id,
                    "P2 durable: restore_budget failed; falling back to zero counters"
                );
                None
            }
        }
    }

    /// Capture the current replan budget snapshot if durable is enabled; returns `None`
    /// when off or not configured.
    fn take_durable_budget_snapshot(
        &self,
        scheduler: &zeph_orchestration::DagScheduler,
    ) -> Option<zeph_orchestration::durable::ReplanBudgetSnapshot> {
        if !durable_orchestration_enabled(self.services.orchestration.durable_config.as_ref()) {
            return None;
        }
        Some(scheduler.budget_snapshot())
    }

    /// Journal the replan budget snapshot for a DAG that is pausing.
    /// Returns the next generation value when the journal write succeeds, so the caller can
    /// persist it onto the final `TaskGraph` snapshot before writing to disk.  Returns `None`
    /// when durable is disabled or the write fails (caller falls back to zeroing counters on
    /// the next resume — safe degraded behaviour).
    async fn journal_durable_budget(
        &mut self,
        graph: &zeph_orchestration::TaskGraph,
        snapshot: zeph_orchestration::durable::ReplanBudgetSnapshot,
    ) -> Option<u32> {
        if !durable_orchestration_enabled(self.services.orchestration.durable_config.as_ref()) {
            return None;
        }
        let cfg = self.services.orchestration.durable_config.clone()?;
        let (backend, writer) = self.ensure_durable_backend().await?;
        let generation = graph.durable_save_generation;
        if let Err(e) = zeph_orchestration::durable::journal_budget(
            &graph.id, generation, backend, writer, &cfg, snapshot,
        )
        .await
        {
            tracing::warn!(
                error = %e,
                graph_id = %graph.id,
                generation,
                "P2 durable: journal_budget failed; budget will be zeroed on next resume"
            );
            return None;
        }
        Some(generation.saturating_add(1))
    }

    pub(super) async fn handle_plan_confirm(&mut self) -> Result<(), error::AgentError> {
        let Some(graph) = self.services.orchestration.pending_graph.take() else {
            self.channel
                .send("No pending plan to confirm. Use `/plan <goal>` to create one.")
                .await?;
            return Ok(());
        };

        let Ok(graph) = self.validate_pending_graph(graph).await else {
            return Ok(());
        };

        // P2 durable: restore replan budget if durable is enabled and this is a resume.
        let durable_budget = self.try_restore_durable_budget(&graph).await;
        let (mut scheduler, reserved) = self.build_dag_scheduler(graph, durable_budget)?;

        let task_count = scheduler.graph().tasks.len();
        self.channel
            .send(&format!(
                "Confirmed. Executing plan ({task_count} tasks)..."
            ))
            .await?;

        let plan_token = CancellationToken::new();
        self.services.orchestration.plan_cancel_token = Some(plan_token.clone());

        let scheduler_result = self
            .run_scheduler_loop(&mut scheduler, task_count, plan_token)
            .await;
        self.services.orchestration.plan_cancel_token = None;

        if let Some(mgr) = self.services.orchestration.subagent_manager.as_mut() {
            mgr.release_reservation(reserved);
        }

        // P2 durable: snapshot budget before defensive save (scheduler still alive here).
        let budget_snap = self.take_durable_budget_snapshot(&scheduler);
        // Defensive save before `?` so a scheduler error still commits the last in-flight state.
        if let Some(ref persistence) = self.services.orchestration.graph_persistence {
            super::scheduler_loop::save_graph_snapshot(persistence, scheduler.graph().clone())
                .await;
        }
        // Journal and capture next generation before consuming the scheduler.
        let next_generation = if let Some(snap) = budget_snap {
            self.journal_durable_budget(scheduler.graph(), snap).await
        } else {
            None
        };

        let final_status = scheduler_result?;

        let extra_task_outputs = self
            .run_whole_plan_verify(&mut scheduler, final_status)
            .await;

        let mut completed_graph = scheduler.into_graph();
        // Persist the incremented generation so the next pause uses a fresh ExecutionId.
        if let Some(gn) = next_generation {
            completed_graph.durable_save_generation = gn;
        }

        if let Some(extra_tasks) = extra_task_outputs {
            completed_graph.tasks.extend(extra_tasks);
        }

        let snapshot = crate::metrics::TaskGraphSnapshot::from(&completed_graph);
        self.update_metrics(|m| {
            m.orchestration_graph = Some(snapshot);
        });

        // Authoritative terminal save after extra_task_outputs are merged — log at ERROR on failure.
        if let Some(ref persistence) = self.services.orchestration.graph_persistence {
            let final_id = completed_graph.id.clone();
            let snapshot = completed_graph.clone();
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                persistence.save(&snapshot),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::error!(
                    error = %e, graph_id = %final_id,
                    "terminal graph persistence save failed — /plan list may be stale"
                ),
                Err(_) => tracing::error!(
                    graph_id = %final_id,
                    "terminal graph persistence save timed out after 5s — /plan list may be stale"
                ),
            }
        }

        let result_label = self
            .finalize_plan_execution(completed_graph, final_status)
            .await?;

        let now = std::time::Instant::now();
        self.update_metrics(|m| {
            if let Some(ref mut s) = m.orchestration_graph {
                result_label.clone_into(&mut s.status);
                s.completed_at = Some(now);
            }
        });
        Ok(())
    }

    pub(super) async fn run_whole_plan_verify(
        &mut self,
        scheduler: &mut zeph_orchestration::DagScheduler,
        final_status: zeph_orchestration::GraphStatus,
    ) -> Option<Vec<zeph_orchestration::TaskNode>> {
        use tracing::Instrument as _;
        use zeph_orchestration::{GraphStatus, PlanVerifier};

        if final_status != GraphStatus::Completed
            || !self
                .services
                .orchestration
                .orchestration_config
                .verify_completeness
            || scheduler.max_replans_remaining() == 0
        {
            return None;
        }

        let threshold = scheduler.completeness_threshold();
        let max_tokens = self
            .services
            .orchestration
            .orchestration_config
            .verify_max_tokens;
        let max_tasks = self.services.orchestration.orchestration_config.max_tasks;
        let goal = scheduler.graph().goal.clone();
        let truncated_output = collect_and_truncate_task_outputs(scheduler.graph(), max_tokens);

        if truncated_output.is_empty() {
            return None;
        }

        let trace_paths = self.resolve_whole_plan_trace_paths(scheduler.graph());
        let tool_trace = match trace_paths {
            Some(paths) => Self::build_whole_plan_tool_trace(paths).await,
            None => None,
        };
        if tool_trace.is_none() {
            tracing::debug!(
                "whole-plan verify: tool-trace union unavailable — at least one completed \
                 task's trace could not be resolved (e.g. a RunInline task, whose in-loop trace \
                 is never persisted, or an unreadable/partial transcript); grounding skipped for \
                 this whole-plan verify (fail-open, matches per-task behavior on an unavailable \
                 trace)"
            );
        }

        let verify_provider = self
            .services
            .orchestration
            .verify_provider
            .as_ref()
            .or(self.services.orchestration.orchestrator_provider.as_ref())
            .unwrap_or(&self.provider)
            .clone();
        let verifier_sanitizer: std::sync::Arc<dyn zeph_common::OutputSanitizer> =
            std::sync::Arc::new(self.services.security.sanitizer.clone());
        let mut verifier = PlanVerifier::new(
            verify_provider,
            verifier_sanitizer,
            &self.services.orchestration.orchestration_config,
        );
        let result = verifier
            .verify_plan(&goal, &truncated_output, tool_trace.as_deref())
            .instrument(tracing::info_span!("core.plan.whole_plan_verify"))
            .await;

        tracing::debug!(
            complete = result.complete,
            confidence = result.confidence,
            gaps = result.gaps.len(),
            threshold,
            "whole-plan verification result"
        );

        let should_replan =
            !result.complete && result.confidence < f64::from(threshold) && !result.gaps.is_empty();

        if result.complete {
            // Plan judged complete — nothing to signal, no replan.
            return None;
        }
        if !should_replan {
            // !complete but the low-confidence/gaps gate wasn't met (confidently
            // incomplete, or no actionable gaps) — #6265: surface a visible signal
            // since no replan will be attempted to resolve it.
            self.signal_plan_incomplete(&result).await;
            return None;
        }

        scheduler.record_whole_plan_replan();

        let next_id = u32::try_from(scheduler.graph().tasks.len()).unwrap_or(u32::MAX);
        let gap_tasks = match verifier
            .replan_from_plan(&goal, &result.gaps, next_id, max_tasks)
            .await
        {
            Ok(tasks) => tasks,
            Err(e) => {
                tracing::warn!(error = %e, "whole-plan replan_from_plan failed (fail-open)");
                self.signal_plan_incomplete(&result).await;
                return None;
            }
        };

        if gap_tasks.is_empty() {
            self.signal_plan_incomplete(&result).await;
            return None;
        }

        let repaired = self.execute_partial_replan_dag(gap_tasks, &goal).await;
        if repaired.is_none() {
            // Replan ran but produced no usable completed task — the gap is still open.
            self.signal_plan_incomplete(&result).await;
        }
        repaired
    }

    /// #6265: emit a persistent, user-visible signal when whole-plan verification judged
    /// the plan's output incomplete and no successful replan resolved it. Fail-open —
    /// matches the surrounding `tracing::warn!` convention for verify-path hiccups.
    async fn signal_plan_incomplete(&mut self, result: &zeph_orchestration::VerificationResult) {
        let msg = format!(
            "Note: the plan output may be incomplete — verification found {} unresolved \
             gap(s) (verification confidence {:.0}%) and automatic repair did not resolve it.",
            result.gaps.len(),
            result.confidence * 100.0
        );
        if let Err(e) = self.channel.send(&msg).await {
            tracing::warn!(
                error = %e,
                "failed to send whole-plan verification-incompleteness signal"
            );
        }
    }

    /// Resolve the transcript path for every completed-with-result task in `graph`, as a
    /// synchronous prerequisite step for [`Self::build_whole_plan_tool_trace`].
    ///
    /// Takes a plain `&TaskGraph` (not `&DagScheduler`) so it is testable without spinning up
    /// full scheduler machinery — the caller passes `scheduler.graph()`.
    ///
    /// Pure in-memory work (`transcript_path_for` is computed from `config` alone, no I/O and no
    /// dependence on handle residency in `SubAgentManager` — see issue #6288) — deliberately kept
    /// synchronous and `&self`-borrowing so it never needs to cross an `.await` point, which
    /// would otherwise require `Agent<C>: Sync` to keep the caller's future `Send` (spec 009 §
    /// Whole-Plan Grounding, issue #6287). Returns `None` the moment any one task's path cannot
    /// be resolved (missing `agent_id` — e.g. a `RunInline` task, whose in-loop trace is never
    /// persisted — or no `SubAgentManager`), matching the all-or-nothing availability contract.
    fn resolve_whole_plan_trace_paths(
        &self,
        graph: &zeph_orchestration::TaskGraph,
    ) -> Option<Vec<std::path::PathBuf>> {
        use zeph_orchestration::TaskStatus;

        let mut paths: Vec<std::path::PathBuf> = Vec::new();
        for task in graph
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Completed && t.result.is_some())
        {
            let Some(agent_id) = task.result.as_ref().and_then(|r| r.agent_id.as_deref()) else {
                tracing::debug!(
                    task_id = %task.id,
                    "whole-plan tool-trace union: task has no agent_id (RunInline dispatch or \
                     missing), aggregate unavailable"
                );
                return None;
            };
            let Some(mgr) = self.services.orchestration.subagent_manager.as_ref() else {
                tracing::debug!(
                    task_id = %task.id,
                    agent_id = %agent_id,
                    "whole-plan tool-trace union: no SubAgentManager configured, aggregate \
                     unavailable"
                );
                return None;
            };
            let cfg = &self.services.orchestration.subagent_config;
            paths.push(mgr.transcript_path_for(cfg, agent_id));
        }
        Some(paths)
    }

    /// Build the DAG-wide **union** of every completed task's real `tool_trace`, rebuilt from
    /// transcripts at whole-plan-verify time (spec 009 § Whole-Plan Grounding, issue #6287).
    ///
    /// Availability is all-or-nothing: returns `Some(union)` iff **every** completed task with
    /// a `result` resolves to a trace (`Some`, including `Some(vec![])` for a task that
    /// genuinely ran zero tools); returns `None` the moment any one task's trace cannot be
    /// resolved (missing `agent_id` — e.g. a `RunInline` task, whose in-loop trace is never
    /// persisted — or an unreadable/partial transcript). An incomplete union is never returned:
    /// a union missing part of the real record could false-positive an honest claim, mirroring
    /// the per-task `None`-means-unavailable contract lifted to the DAG level.
    ///
    /// Transcript path resolution ([`Self::resolve_whole_plan_trace_paths`]) happens entirely
    /// before this method's only `.await`, so `self` is never held across it — this is a free
    /// associated function taking owned `paths` rather than `&self` specifically to keep the
    /// generated future `Send` regardless of `Agent<C>`'s `Sync` bound. The actual synchronous
    /// file reads + JSON parsing (`TranscriptReader::load_strict`) are offloaded to
    /// `spawn_blocking` since this loop can run N reads back-to-back with no yield point on the
    /// async finalization path.
    async fn build_whole_plan_tool_trace(
        paths: Vec<std::path::PathBuf>,
    ) -> Option<Vec<zeph_orchestration::ToolCallSummary>> {
        if paths.is_empty() {
            return Some(Vec::new());
        }

        let read_result = tokio::task::spawn_blocking(move || {
            let mut union = Vec::new();
            for path in &paths {
                match zeph_subagent::TranscriptReader::load_strict(path) {
                    Ok(messages) => {
                        union.extend(super::scheduler_loop::tool_trace_from_messages(&messages));
                    }
                    Err(e) => return Err(format!("{}: {e}", path.display())),
                }
            }
            Ok(union)
        })
        .await;

        match read_result {
            Ok(Ok(union)) => Some(union),
            Ok(Err(e)) => {
                tracing::debug!(
                    error = %e,
                    "whole-plan tool-trace union: transcript read failed, aggregate unavailable"
                );
                None
            }
            Err(join_err) => {
                tracing::warn!(
                    error = %join_err,
                    "whole-plan tool-trace union: spawn_blocking panicked, aggregate unavailable"
                );
                None
            }
        }
    }

    pub(super) async fn execute_partial_replan_dag(
        &mut self,
        gap_tasks: Vec<zeph_orchestration::TaskNode>,
        goal: &str,
    ) -> Option<Vec<zeph_orchestration::TaskNode>> {
        use zeph_orchestration::{DagScheduler, RuleBasedRouter, TaskId, TaskStatus};

        // `replan_from_plan` assigns gap-task IDs continuing the parent graph's numbering
        // (`next_id..`, so downstream merges into `completed_graph.tasks` stay globally
        // unique), but `dag::validate` requires a freshly-constructed standalone `TaskGraph`'s
        // task IDs to be 0-based and positional (`tasks[i].id == TaskId(i)`) — otherwise
        // `DagScheduler::new` rejects the graph outright. Remap to local 0-based IDs for this
        // scheduler run, then remap back to the original global IDs on the way out. Safe
        // because whole-plan gap tasks are always independent roots with no `depends_on`
        // cross-references to fix up (see `replan_from_plan`'s doc comment).
        let base_id = gap_tasks.first().map_or(0, |t| t.id.0);
        let mut partial_graph = zeph_orchestration::TaskGraph::new(goal);
        partial_graph.tasks = gap_tasks
            .into_iter()
            .enumerate()
            .map(|(i, mut task)| {
                task.id = TaskId(u32::try_from(i).unwrap_or(u32::MAX));
                task
            })
            .collect();

        let mut partial_config = self.services.orchestration.orchestration_config.clone();
        partial_config.max_replans = 0;
        partial_config.verify_completeness = false;

        let available_agents = self
            .services
            .orchestration
            .subagent_manager
            .as_ref()
            .map(|m| m.definitions().to_vec())
            .unwrap_or_default();

        // A1 fix: replan DAG also needs admission control, same as the primary DAG.
        let partial_admission_gate = {
            let pairs: Vec<(String, usize)> = self
                .runtime
                .providers
                .provider_pool
                .iter()
                .filter_map(|e| e.max_concurrent.map(|c| (e.effective_name(), c as usize)))
                .collect();
            if pairs.is_empty() {
                None
            } else {
                Some(zeph_orchestration::AdmissionGate::new(&pairs))
            }
        };

        let partial_sanitizer: std::sync::Arc<dyn zeph_common::OutputSanitizer> =
            std::sync::Arc::new(self.services.security.sanitizer.clone());

        let mut partial_scheduler = match DagScheduler::new(
            partial_graph,
            &partial_config,
            Box::new(RuleBasedRouter),
            available_agents,
            partial_admission_gate,
        )
        .map(|s| s.with_sanitizer(partial_sanitizer))
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "whole-plan replan: failed to create partial DagScheduler (fail-open)"
                );
                return None;
            }
        };

        let partial_task_count = partial_scheduler.graph().tasks.len();
        let cancel_token = CancellationToken::new();
        if let Err(e) = self
            .run_scheduler_loop(&mut partial_scheduler, partial_task_count, cancel_token)
            .await
        {
            tracing::warn!(
                error = %e,
                "whole-plan replan: partial DAG run failed (fail-open)"
            );
        }

        let completed: Vec<_> = partial_scheduler
            .into_graph()
            .tasks
            .into_iter()
            .filter(|t| t.status == TaskStatus::Completed)
            .map(|mut t| {
                t.id = TaskId(t.id.0 + base_id);
                t
            })
            .collect();

        if completed.is_empty() {
            None
        } else {
            Some(completed)
        }
    }

    pub(super) async fn finalize_plan_execution(
        &mut self,
        completed_graph: zeph_orchestration::TaskGraph,
        final_status: zeph_orchestration::GraphStatus,
    ) -> Result<&'static str, error::AgentError> {
        use zeph_orchestration::GraphStatus;

        // AdaptOrch: record outcome synchronously before aggregation.
        if let Some(verdict) = self.services.orchestration.last_advisor_verdict.take()
            && let Some(ref advisor) = self.services.orchestration.topology_advisor
        {
            let reward = if final_status == GraphStatus::Completed {
                1.0
            } else {
                0.0
            };
            advisor.record_outcome(verdict.class, verdict.hint, reward);
        }

        let result_label = match final_status {
            GraphStatus::Completed => self.finalize_plan_completed(completed_graph).await?,
            GraphStatus::Failed => self.finalize_plan_failed(completed_graph).await?,
            GraphStatus::Paused => {
                self.channel
                    .send(
                        "Plan paused due to a task failure (ask strategy). \
                         Use `/plan resume` to continue or `/plan retry` to retry failed tasks.",
                    )
                    .await?;
                self.services.orchestration.pending_graph = Some(completed_graph);
                "paused"
            }
            GraphStatus::Canceled => {
                let done_count = completed_graph
                    .tasks
                    .iter()
                    .filter(|t| t.status == zeph_orchestration::TaskStatus::Completed)
                    .count();
                self.update_metrics(|m| m.orchestration.tasks_completed += done_count as u64);
                let total = completed_graph.tasks.len();
                self.channel
                    .send(&format!(
                        "Plan canceled. {done_count}/{total} tasks completed before cancellation."
                    ))
                    .await?;
                self.services.orchestration.pending_goal_embedding.take();
                "canceled"
            }
            _ => {
                self.services.orchestration.pending_goal_embedding.take();
                "unknown"
            }
        };
        Ok(result_label)
    }

    async fn finalize_plan_completed(
        &mut self,
        completed_graph: zeph_orchestration::TaskGraph,
    ) -> Result<&'static str, error::AgentError> {
        use tracing::Instrument as _;
        use zeph_orchestration::{Aggregator, LlmAggregator};

        let completed_count = completed_graph
            .tasks
            .iter()
            .filter(|t| t.status == zeph_orchestration::TaskStatus::Completed)
            .count() as u64;
        let skipped_count = completed_graph
            .tasks
            .iter()
            .filter(|t| t.status == zeph_orchestration::TaskStatus::Skipped)
            .count() as u64;
        // D3 (spec-075 FR-D-01): on a Completed graph, a Failed task can only be a
        // rerouted Mode-2 source — Mode-1 relabels Failed -> Completed, Skip relabels
        // Failed -> Skipped, and Abort/retry-exhausted-without-reroute sets the graph
        // Failed (not Completed), so this branch is never reached with an unrecovered
        // Failed task. If a future recovery mechanism ever leaves a Failed task inside
        // a Completed graph without being a rerouted source, this tally would silently
        // misclassify it as "rerouted" — re-verify the invariant before adding one.
        let rerouted_failed_count = completed_graph
            .tasks
            .iter()
            .filter(|t| t.status == zeph_orchestration::TaskStatus::Failed)
            .count() as u64;
        self.update_metrics(|m| {
            m.orchestration.tasks_completed += completed_count;
            m.orchestration.tasks_skipped += skipped_count;
            m.orchestration.tasks_failed += rerouted_failed_count;
        });

        let aggregator_provider = self
            .services
            .orchestration
            .orchestrator_provider
            .as_ref()
            .unwrap_or(&self.provider)
            .clone();
        let aggregator_sanitizer: std::sync::Arc<dyn zeph_common::OutputSanitizer> =
            std::sync::Arc::new(self.services.security.sanitizer.clone());
        let aggregator = LlmAggregator::new(
            aggregator_provider,
            &self.services.orchestration.orchestration_config,
        )
        .with_sanitizer(aggregator_sanitizer);
        match aggregator
            .aggregate(&completed_graph)
            .instrument(tracing::info_span!("core.plan.finalize_completed"))
            .await
        {
            Ok((synthesis, aggregator_usage)) => {
                let (aggr_prompt, aggr_completion) = aggregator_usage.unwrap_or((0, 0));
                self.update_metrics(|m| {
                    m.api_calls += 1;
                    m.prompt_tokens += aggr_prompt;
                    m.completion_tokens += aggr_completion;
                    m.total_tokens = m.prompt_tokens + m.completion_tokens;
                });
                self.record_cost_and_cache(aggr_prompt, aggr_completion);
                self.record_successful_task();
                self.channel.send(&synthesis).await?;
            }
            Err(e) => {
                tracing::error!(error = %e, "aggregation failed");
                self.channel
                    .send(
                        "Plan completed but aggregation failed. \
                         Check individual task results.",
                    )
                    .await?;
            }
        }

        if let Some(ref cache) = self.services.orchestration.plan_cache
            && let Some(embedding) = self.services.orchestration.pending_goal_embedding.take()
        {
            let embed_model = self.services.skill.embedding_model.clone();
            if let Err(e) = cache
                .cache_plan(&completed_graph, &embedding, &embed_model)
                .await
            {
                tracing::warn!(error = %e, "plan cache: failed to cache completed plan");
            }
        }

        Ok("completed")
    }

    async fn finalize_plan_failed(
        &mut self,
        completed_graph: zeph_orchestration::TaskGraph,
    ) -> Result<&'static str, error::AgentError> {
        use std::fmt::Write;

        // M2 (spec-075 FR-D-01, cosmetic): a Mode-2 route_to fallback can persist in
        // TaskStatus::Dormant into a Failed graph (the Abort/retry-exhausted path sets
        // graph.status = Failed and returns before the completion sweep runs — see
        // `check_graph_completion`'s `resolve_dormant_after_terminal` doc). A Dormant
        // task is counted in none of the buckets below, so failed+cancelled+completed+
        // skipped may not sum to tasks.len() on this path. Benign: `/plan retry`
        // re-arms it if its source is reset, or the completion sweep resolves it once
        // a retried graph heads to Completed. Not treated as an error here.
        let failed_tasks: Vec<_> = completed_graph
            .tasks
            .iter()
            .filter(|t| t.status == zeph_orchestration::TaskStatus::Failed)
            .collect();
        let cancelled_tasks: Vec<_> = completed_graph
            .tasks
            .iter()
            .filter(|t| t.status == zeph_orchestration::TaskStatus::Canceled)
            .collect();
        let completed_count = completed_graph
            .tasks
            .iter()
            .filter(|t| t.status == zeph_orchestration::TaskStatus::Completed)
            .count() as u64;
        let skipped_count = completed_graph
            .tasks
            .iter()
            .filter(|t| t.status == zeph_orchestration::TaskStatus::Skipped)
            .count() as u64;
        self.update_metrics(|m| {
            m.orchestration.tasks_failed += failed_tasks.len() as u64;
            m.orchestration.tasks_completed += completed_count;
            m.orchestration.tasks_skipped += skipped_count;
        });
        let total = completed_graph.tasks.len();
        let msg = if failed_tasks.is_empty() && !cancelled_tasks.is_empty() {
            format!(
                "Plan canceled. {}/{} tasks did not run.\n\
                 Use `/plan retry` to retry or check logs for details.",
                cancelled_tasks.len(),
                total
            )
        } else if failed_tasks.is_empty() && cancelled_tasks.is_empty() {
            tracing::warn!(
                "plan finished with GraphStatus::Failed but no failed or canceled tasks"
            );
            "Plan failed. No task errors recorded; check logs for details.".to_string()
        } else {
            let mut m = if cancelled_tasks.is_empty() {
                format!(
                    "Plan failed. {}/{} tasks failed:\n",
                    failed_tasks.len(),
                    total
                )
            } else {
                format!(
                    "Plan failed. {}/{} tasks failed, {} canceled:\n",
                    failed_tasks.len(),
                    total,
                    cancelled_tasks.len()
                )
            };
            for t in &failed_tasks {
                let err: std::borrow::Cow<str> =
                    t.result.as_ref().map_or("unknown error".into(), |r| {
                        if r.output.len() > 500 {
                            r.output.chars().take(500).collect::<String>().into()
                        } else {
                            r.output.as_str().into()
                        }
                    });
                let _ = writeln!(m, "  - {}: {err}", t.title);
            }
            m.push_str("\nUse `/plan retry` to retry failed tasks.");
            m
        };
        self.channel.send(&msg).await?;
        self.services.orchestration.pending_graph = Some(completed_graph);
        Ok("failed")
    }

    // ----- _as_string variants (used by AgentAccess / CommandHandler) -----

    async fn compute_topology_hint(
        &mut self,
        goal: &str,
    ) -> Option<zeph_orchestration::TopologyHint> {
        let advisor = self.services.orchestration.topology_advisor.clone()?;
        let verdict = advisor.recommend(goal).await;
        tracing::debug!(
            class = ?verdict.class,
            hint = ?verdict.hint,
            exploit = verdict.exploit,
            fallback = verdict.fallback,
            "adaptorch verdict"
        );
        let hint = verdict.hint;
        self.services.orchestration.last_advisor_verdict = Some(verdict);
        Some(hint)
    }

    fn record_plan_metrics(
        &mut self,
        graph: &zeph_orchestration::TaskGraph,
        usage: Option<(u64, u64)>,
    ) {
        let task_count = graph.tasks.len() as u64;
        let snapshot = crate::metrics::TaskGraphSnapshot::from(graph);
        let (planner_prompt, planner_completion) = usage.unwrap_or((0, 0));
        self.update_metrics(|m| {
            m.api_calls += 1;
            m.prompt_tokens += planner_prompt;
            m.completion_tokens += planner_completion;
            m.total_tokens = m.prompt_tokens + m.completion_tokens;
            m.orchestration.plans_total += 1;
            m.orchestration.tasks_total += task_count;
            m.orchestration_graph = Some(snapshot);
        });
        self.record_cost_and_cache(planner_prompt, planner_completion);
        self.record_successful_task();
    }

    pub(super) async fn handle_plan_goal_as_string(
        &mut self,
        goal: &str,
    ) -> Result<String, error::AgentError> {
        use zeph_orchestration::{LlmPlanner, plan_with_cache};

        if self.services.orchestration.pending_graph.is_some() {
            return Ok("A plan is already pending confirmation. \
                 Use /plan confirm to execute it or /plan cancel to discard."
                .to_owned());
        }

        let available_agents = self
            .services
            .orchestration
            .subagent_manager
            .as_ref()
            .map(|m| m.definitions().to_vec())
            .unwrap_or_default();
        let confirm_before_execute = self
            .services
            .orchestration
            .orchestration_config
            .confirm_before_execute;

        self.init_plan_cache_if_needed().await;
        let goal_embedding = self.goal_embedding_for_cache(goal).await;
        tracing::debug!(
            cache_enabled = self
                .services
                .orchestration
                .orchestration_config
                .plan_cache
                .enabled,
            has_embedding = goal_embedding.is_some(),
            "plan cache state for goal"
        );

        let topology_hint = self.compute_topology_hint(goal).await;

        let cfg = &self.services.orchestration.orchestration_config;
        let planner_provider = self
            .services
            .orchestration
            .planner_provider
            .as_ref()
            .unwrap_or(&self.provider)
            .clone();
        let planner = LlmPlanner::new(planner_provider, cfg);
        let embed_model = self.services.skill.embedding_model.clone();
        let max_tasks = cfg.max_tasks;
        let verify_predicate_enabled = cfg.verify_predicate_enabled;
        let (graph, planner_usage) = {
            use zeph_orchestration::Planner as _;
            let use_cache = topology_hint
                .as_ref()
                .is_none_or(|h| h.prompt_sentence().is_none());
            let planner_timeout = std::time::Duration::from_secs(cfg.planner_timeout_secs);
            let result = if use_cache {
                plan_with_cache(
                    &planner,
                    self.services.orchestration.plan_cache.as_ref(),
                    &self.provider,
                    goal_embedding.as_deref(),
                    &embed_model,
                    goal,
                    &available_agents,
                    max_tasks,
                    planner_timeout,
                    verify_predicate_enabled,
                )
                .await
            } else {
                planner
                    .plan_with_hint(goal, &available_agents, topology_hint)
                    .await
            };
            result.map_err(|e| error::OrchestrationFailure::PlannerError(e.to_string()))?
        };

        self.services.orchestration.pending_goal_embedding = goal_embedding;
        self.record_plan_metrics(&graph, planner_usage);

        let summary = format_plan_summary(&graph);
        if confirm_before_execute {
            self.services.orchestration.pending_graph = Some(graph);
            Ok(format!(
                "{summary}\nType `/plan confirm` to execute, or `/plan cancel` to abort."
            ))
        } else {
            let now = std::time::Instant::now();
            self.update_metrics(|m| {
                if let Some(ref mut s) = m.orchestration_graph {
                    "completed".clone_into(&mut s.status);
                    s.completed_at = Some(now);
                }
            });
            Ok(format!(
                "{summary}\nPlan ready. Full execution will be available in a future phase."
            ))
        }
    }

    pub(super) fn handle_plan_status_as_string(&mut self, _graph_id: Option<&str>) -> String {
        let Some(ref graph) = self.services.orchestration.pending_graph else {
            return "No active plan.".to_owned();
        };
        format_plan_status(graph)
    }

    pub(super) fn handle_plan_list_as_string(&mut self) -> String {
        if let Some(ref graph) = self.services.orchestration.pending_graph {
            let summary = format_plan_summary(graph);
            let status_label = match graph.status {
                zeph_orchestration::GraphStatus::Created => "awaiting confirmation",
                zeph_orchestration::GraphStatus::Running => "running",
                zeph_orchestration::GraphStatus::Paused => "paused",
                zeph_orchestration::GraphStatus::Failed => "failed (retryable)",
                _ => "unknown",
            };
            format!("{summary}\nStatus: {status_label}")
        } else {
            "No recent plans.".to_owned()
        }
    }

    pub(super) fn handle_plan_cancel_as_string(&mut self, _graph_id: Option<&str>) -> String {
        if let Some(token) = self.services.orchestration.plan_cancel_token.take() {
            token.cancel();
            "Canceling plan execution...".to_owned()
        } else if self.services.orchestration.pending_graph.take().is_some() {
            let now = std::time::Instant::now();
            self.update_metrics(|m| {
                if let Some(ref mut s) = m.orchestration_graph {
                    "canceled".clone_into(&mut s.status);
                    s.completed_at = Some(now);
                }
            });
            self.services.orchestration.pending_goal_embedding = None;
            "Plan canceled.".to_owned()
        } else {
            "No active plan to cancel.".to_owned()
        }
    }

    fn resume_loaded_graph(
        &mut self,
        loaded: zeph_orchestration::TaskGraph,
        id_str: &str,
    ) -> String {
        use zeph_orchestration::{GraphStatus, TaskStatus};
        match loaded.status {
            GraphStatus::Completed => {
                format!("Plan '{id_str}' is already Completed. Use `/plan status` to view results.")
            }
            GraphStatus::Canceled => format!(
                "Plan '{id_str}' was Canceled and cannot be resumed. \
                 Start a new plan with `/plan <goal>`."
            ),
            GraphStatus::Paused => {
                let msg = format!(
                    "Resuming plan: {}\nUse `/plan confirm` to continue execution.",
                    loaded.goal
                );
                tracing::info!(graph_id = %loaded.id, "rehydrated paused graph from disk");
                self.services.orchestration.pending_graph = Some(loaded);
                msg
            }
            GraphStatus::Running => {
                // Crash recovery: reset in-flight tasks to Ready and treat as Paused.
                let mut graph = loaded;
                let running_count = graph
                    .tasks
                    .iter()
                    .filter(|t| t.status == TaskStatus::Running)
                    .count();
                for task in &mut graph.tasks {
                    if task.status == TaskStatus::Running {
                        task.status = TaskStatus::Ready;
                        task.assigned_agent = None;
                    }
                }
                graph.status = GraphStatus::Paused;
                let msg = format!(
                    "Recovered plan after interruption ({running_count} in-flight task(s) reset). \
                     Use `/plan confirm` to continue."
                );
                tracing::info!(
                    graph_id = %graph.id,
                    running_count,
                    "crash-recovery: rehydrated Running graph from disk, reset to Paused"
                );
                self.services.orchestration.pending_graph = Some(graph);
                msg
            }
            GraphStatus::Failed => {
                let msg = format!(
                    "Plan '{id_str}' is in Failed status. \
                     Use `/plan retry` to retry failed tasks or `/plan status` to inspect."
                );
                tracing::info!(graph_id = %loaded.id, "rehydrated failed graph from disk");
                self.services.orchestration.pending_graph = Some(loaded);
                msg
            }
            GraphStatus::Created => {
                let msg = format!(
                    "Plan '{id_str}' has not started executing. Use `/plan confirm` to start."
                );
                tracing::info!(graph_id = %loaded.id, "rehydrated created graph from disk");
                self.services.orchestration.pending_graph = Some(loaded);
                msg
            }
            _ => format!("Plan '{id_str}' is in an unrecognised state and cannot be resumed."),
        }
    }

    pub(super) async fn handle_plan_resume_as_string(&mut self, graph_id: Option<&str>) -> String {
        use zeph_orchestration::{GraphId, GraphStatus};

        // Path A: active pending_graph exists — use existing status-gate logic.
        if let Some(ref graph) = self.services.orchestration.pending_graph {
            if let Some(id) = graph_id
                && graph.id.to_string() != id
            {
                return format!(
                    "Graph id '{id}' does not match the active plan ({}). \
                     Use `/plan status` to see the active plan id.",
                    graph.id
                );
            }
            if graph.status != GraphStatus::Paused {
                return format!(
                    "The active plan is in '{}' status and cannot be resumed. \
                     Only Paused plans can be resumed.",
                    graph.status
                );
            }
            let graph = self
                .services
                .orchestration
                .pending_graph
                .take()
                .expect("just checked Some");
            tracing::info!(graph_id = %graph.id, "resuming paused graph");
            let msg = format!(
                "Resuming plan: {}\nUse `/plan confirm` to continue execution.",
                graph.goal
            );
            self.services.orchestration.pending_graph = Some(graph);
            return msg;
        }

        // Path B: no active pending_graph — try disk rehydration.
        let Some(id_str) = graph_id else {
            return "No paused plan to resume. Use `/plan status` to check the current state."
                .to_owned();
        };
        let graph_id_parsed = match id_str.parse::<GraphId>() {
            Ok(id) => id,
            Err(e) => return format!("Invalid graph id '{id_str}': {e}"),
        };
        let Some(ref persistence) = self.services.orchestration.graph_persistence else {
            return "Graph persistence is disabled. \
                    Set `orchestration.persistence_enabled = true` in config."
                .to_owned();
        };
        let loaded = match persistence.load(&graph_id_parsed).await {
            Ok(Some(g)) => g,
            Ok(None) => return format!("Graph '{id_str}' not found in persistence."),
            Err(e) => return format!("Failed to load graph '{id_str}' from persistence: {e}"),
        };

        self.resume_loaded_graph(loaded, id_str)
    }

    pub(super) fn handle_plan_retry_as_string(
        &mut self,
        graph_id: Option<&str>,
    ) -> Result<String, error::AgentError> {
        use zeph_orchestration::{GraphStatus, dag, topology::build_rev_adj};

        let Some(ref graph) = self.services.orchestration.pending_graph else {
            return Ok(
                "No active plan to retry. Use `/plan status` to check the current state."
                    .to_owned(),
            );
        };

        if let Some(id) = graph_id
            && graph.id.to_string() != id
        {
            return Ok(format!(
                "Graph id '{id}' does not match the active plan ({}). \
                 Use `/plan status` to see the active plan id.",
                graph.id
            ));
        }

        if graph.status != GraphStatus::Failed && graph.status != GraphStatus::Paused {
            return Ok(format!(
                "The active plan is in '{}' status. Only Failed or Paused plans can be retried.",
                graph.status
            ));
        }

        // SAFETY: `pending_graph` was verified to be `Some` at line 943 above; no other
        // code path between that check and here can set it to `None`.
        let mut graph = self
            .services
            .orchestration
            .pending_graph
            .take()
            .expect("BUG: pending_graph was Some at entry but became None before take()");

        let failed_count = graph
            .tasks
            .iter()
            .filter(|t| t.status == zeph_orchestration::TaskStatus::Failed)
            .count();

        let rev_adj = build_rev_adj(&graph.tasks);
        dag::reset_for_retry(&mut graph, &rev_adj)
            .map_err(|e| error::OrchestrationFailure::RetryReset(e.to_string()))?;

        for task in &mut graph.tasks {
            if task.status == zeph_orchestration::TaskStatus::Running {
                task.status = zeph_orchestration::TaskStatus::Ready;
                task.assigned_agent = None;
            }
        }

        tracing::info!(
            graph_id = %graph.id,
            failed_count,
            "retrying failed tasks in graph"
        );

        let msg = format!(
            "Retrying {failed_count} failed task(s) in plan: {}\n\
             Use `/plan confirm` to execute.",
            graph.goal
        );
        self.services.orchestration.pending_graph = Some(graph);
        Ok(msg)
    }

    pub(super) async fn handle_plan_command_as_string(
        &mut self,
        cmd: zeph_orchestration::PlanCommand,
    ) -> Result<String, error::AgentError> {
        use zeph_orchestration::PlanCommand;

        if !self.config_for_orchestration().enabled {
            return Ok(
                "Task orchestration is disabled. Set `orchestration.enabled = true` in config."
                    .to_owned(),
            );
        }

        match cmd {
            PlanCommand::Goal(goal) => self.handle_plan_goal_as_string(&goal).await,
            PlanCommand::Confirm => {
                // handle_plan_confirm sends progress and result messages directly via
                // self.channel (long-running, multi-message). Empty string signals
                // CommandOutput::Silent to the registry — output is already delivered.
                self.handle_plan_confirm().await?;
                Ok(String::new())
            }
            PlanCommand::Status(id) => Ok(self.handle_plan_status_as_string(id.as_deref())),
            PlanCommand::List => Ok(self.handle_plan_list_as_string()),
            PlanCommand::Cancel(id) => Ok(self.handle_plan_cancel_as_string(id.as_deref())),
            PlanCommand::Resume(id) => Ok(self.handle_plan_resume_as_string(id.as_deref()).await),
            PlanCommand::Retry(id) => self.handle_plan_retry_as_string(id.as_deref()),
            _ => Ok(String::new()),
        }
    }

    pub(super) async fn dispatch_plan_command_as_string(
        &mut self,
        trimmed: &str,
    ) -> Result<String, error::AgentError> {
        match zeph_orchestration::PlanCommand::parse(trimmed) {
            Ok(cmd) => self.handle_plan_command_as_string(cmd).await,
            Err(e) => Ok(e.to_string()),
        }
    }
}

/// Returns `true` when the P2 durable orchestration adapter is active.
///
/// Used by the no-op guard in `try_restore_durable_budget`, `take_durable_budget_snapshot`, and
/// `journal_durable_budget` so the early-return condition can be tested without constructing a
/// full `Agent<C>`.
pub(crate) fn durable_orchestration_enabled(cfg: Option<&zeph_config::DurableConfig>) -> bool {
    cfg.is_some_and(|c| c.enabled && c.orchestration)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disabled_cfg() -> zeph_config::DurableConfig {
        zeph_config::DurableConfig {
            enabled: false,
            ..zeph_config::DurableConfig::default()
        }
    }

    fn enabled_cfg() -> zeph_config::DurableConfig {
        zeph_config::DurableConfig {
            enabled: true,
            orchestration: true,
            ..zeph_config::DurableConfig::default()
        }
    }

    fn orchestration_off_cfg() -> zeph_config::DurableConfig {
        zeph_config::DurableConfig {
            enabled: true,
            orchestration: false,
            ..zeph_config::DurableConfig::default()
        }
    }

    // #6390: format_plan_status surfaces TaskNode::handoff_rejected (spec-080) — this was
    // persisted and logged but had no CLI/TUI display surface before this fix.
    #[test]
    fn format_plan_status_no_rejections_is_base_message_only() {
        use zeph_orchestration::TaskGraph;
        let graph = TaskGraph::new("goal");
        let msg = format_plan_status(&graph);
        assert!(!msg.contains("Rejected Command handoff"));
    }

    #[test]
    fn format_plan_status_includes_rejected_handoff_with_task_id_and_title() {
        use zeph_orchestration::{TaskGraph, TaskNode};
        let mut graph = TaskGraph::new("goal");
        let mut task = TaskNode::new(0, "Router", "d");
        task.handoff_rejected = Some("goto target already completed".to_string());
        graph.tasks.push(task);

        let msg = format_plan_status(&graph);
        assert!(msg.contains("Rejected Command handoff(s):"));
        assert!(msg.contains("Task 0"));
        assert!(msg.contains("Router"));
        assert!(msg.contains("goto target already completed"));
    }

    #[test]
    fn format_plan_status_lists_multiple_rejected_handoffs() {
        use zeph_orchestration::{TaskGraph, TaskNode};
        let mut graph = TaskGraph::new("goal");
        let mut t0 = TaskNode::new(0, "A", "d");
        t0.handoff_rejected = Some("reason A".to_string());
        let mut t1 = TaskNode::new(1, "B", "d");
        t1.handoff_rejected = Some("reason B".to_string());
        graph.tasks.push(t0);
        graph.tasks.push(t1);

        let msg = format_plan_status(&graph);
        assert!(msg.contains("reason A"));
        assert!(msg.contains("reason B"));
    }

    // FR-DE-13 AC: when durable is absent, disabled, or orchestration=false → no-op.
    #[test]
    fn durable_disabled_when_config_absent() {
        assert!(!durable_orchestration_enabled(None));
    }

    #[test]
    fn durable_disabled_when_enabled_false() {
        assert!(!durable_orchestration_enabled(Some(&disabled_cfg())));
    }

    #[test]
    fn durable_disabled_when_orchestration_false() {
        assert!(!durable_orchestration_enabled(Some(
            &orchestration_off_cfg()
        )));
    }

    #[test]
    fn durable_enabled_when_both_flags_true() {
        assert!(durable_orchestration_enabled(Some(&enabled_cfg())));
    }

    // --- #6287: whole-plan verifier grounding — DAG-wide tool-trace union (spec 009 §
    // Whole-Plan Grounding) ---

    /// Spawns a "worker" sub-agent via [`crate::agent::Agent`]'s `AgentCommand::Background`
    /// path — the same machinery production code and `scheduler_loop`'s
    /// `spawn_worker_and_wait_completed` use — but reuses an already-configured
    /// `SubAgentManager` across multiple calls so several agent ids stay simultaneously
    /// resolvable via `agent_transcript_dir`. Needed to build a multi-task DAG-wide trace union
    /// in these tests (the `scheduler_loop.rs` helper recreates the manager on every call,
    /// which would evict the previous spawn's id).
    async fn spawn_worker_and_wait_completed_shared(
        agent: &mut crate::agent::Agent<crate::agent::agent_tests::MockChannel>,
        tmp: &std::path::Path,
    ) -> String {
        use zeph_subagent::def::{SkillFilter, SubAgentPermissions, ToolPolicy};
        use zeph_subagent::hooks::SubagentHooks;
        use zeph_subagent::{AgentCommand, SubAgentDef, SubAgentManager, SubAgentState};

        if agent.services.orchestration.subagent_manager.is_none() {
            agent.services.orchestration.subagent_config.transcript_dir = Some(tmp.to_path_buf());
            agent
                .services
                .orchestration
                .subagent_config
                .transcript_enabled = true;

            let mut mgr = SubAgentManager::new(4);
            mgr.definitions_mut().push(SubAgentDef {
                name: "worker".into(),
                description: "A worker bot".into(),
                model: None,
                tools: ToolPolicy::InheritAll,
                disallowed_tools: vec![],
                permissions: SubAgentPermissions {
                    max_turns: 1,
                    ..SubAgentPermissions::default()
                },
                skills: SkillFilter::default(),
                system_prompt: "You are a worker.".into(),
                hooks: SubagentHooks::default(),
                memory: None,
                source: None,
                file_path: None,
            });
            agent.services.orchestration.subagent_manager = Some(mgr);
        }

        let spawn_resp = agent
            .handle_agent_command(AgentCommand::Background {
                name: "worker".into(),
                prompt: "do a task".into(),
            })
            .await
            .expect("Background spawn must return Some");
        let short_id = spawn_resp
            .split("id: ")
            .nth(1)
            .expect("response must contain 'id: '")
            .trim_end_matches(')')
            .trim()
            .to_string();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let mgr = agent
                .services
                .orchestration
                .subagent_manager
                .as_ref()
                .unwrap();
            let statuses = mgr.statuses();
            let found = statuses.iter().find(|(id, _)| id.starts_with(&short_id));
            if let Some((id, status)) = found {
                match status.state {
                    SubAgentState::Completed => break id.clone(),
                    SubAgentState::Failed => {
                        panic!("sub-agent Failed unexpectedly: {:?}", status.last_message);
                    }
                    _ => {}
                }
            }
            assert!(
                std::time::Instant::now() <= deadline,
                "sub-agent did not complete within timeout"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// Appends a real `ToolUse`("bash", `{"command": "cargo test"}`)/`ToolResult` round to the
    /// `.jsonl` transcript at `jsonl_path` (mirrors `scheduler_loop.rs`'s private helper of the
    /// same shape — duplicated here since test helpers are module-private).
    async fn append_tool_round(jsonl_path: &std::path::Path) {
        let writer = zeph_subagent::TranscriptWriter::new(jsonl_path).unwrap();
        writer
            .append(
                1000,
                &zeph_llm::provider::Message::from_parts(
                    zeph_llm::provider::Role::Assistant,
                    vec![zeph_llm::provider::MessagePart::ToolUse {
                        id: "call-1".into(),
                        name: "bash".into(),
                        input: serde_json::json!({ "command": "cargo test" }),
                    }],
                ),
            )
            .await
            .unwrap();
        writer
            .append(
                1001,
                &zeph_llm::provider::Message::from_parts(
                    zeph_llm::provider::Role::User,
                    vec![zeph_llm::provider::MessagePart::ToolResult {
                        tool_use_id: "call-1".into(),
                        content: "ok".into(),
                        is_error: false,
                    }],
                ),
            )
            .await
            .unwrap();
    }

    fn completed_task_with_agent(id: u32, agent_id: &str) -> zeph_orchestration::TaskNode {
        let mut task = zeph_orchestration::TaskNode::new(id, format!("t{id}"), "d");
        task.status = zeph_orchestration::TaskStatus::Completed;
        task.result = Some(zeph_orchestration::TaskResult {
            output: "done".to_string(),
            artifacts: vec![],
            duration_ms: 0,
            agent_id: Some(agent_id.to_string()),
            agent_def: None,
        });
        task
    }

    /// AC-13/M3 (integration-level): two completed spawn tasks with intact-but-empty
    /// transcripts (zero tool calls) resolve to an aggregate `Some(vec![])`, NOT `None` — the
    /// pitfall the critic flagged as most likely to be gotten wrong (spec 009 § Whole-Plan
    /// Grounding). An empty-but-available union is the tightest detection case, so collapsing it
    /// to `None` would silently fail the whole feature open for every pure-LLM-task plan.
    #[tokio::test]
    async fn whole_plan_trace_union_stays_some_empty_for_zero_tool_completed_tasks() {
        use crate::agent::agent_tests::*;

        let tmp = tempfile::tempdir().unwrap();
        let provider = mock_provider(vec![
            "task completed successfully".into(),
            "task completed successfully".into(),
        ]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

        let id1 = spawn_worker_and_wait_completed_shared(&mut agent, tmp.path()).await;
        let id2 = spawn_worker_and_wait_completed_shared(&mut agent, tmp.path()).await;

        let mut graph = zeph_orchestration::TaskGraph::new("goal");
        graph.tasks = vec![
            completed_task_with_agent(0, &id1),
            completed_task_with_agent(1, &id2),
        ];

        let paths = agent
            .resolve_whole_plan_trace_paths(&graph)
            .expect("both tasks have resolvable agent_ids/transcript dirs");
        assert_eq!(paths.len(), 2);

        let union = Agent::<MockChannel>::build_whole_plan_tool_trace(paths)
            .await
            .expect("intact-but-empty transcripts must resolve to Some(union), not None");
        assert!(
            union.is_empty(),
            "neither worker ran a tool, so the union must be Some(vec![]): {union:?}"
        );
    }

    /// AC-13 (integration-level): a real tool call recorded in ONE task's transcript is present
    /// in the DAG-wide union alongside the other (tool-free) task's contribution.
    #[tokio::test]
    async fn whole_plan_trace_union_combines_multiple_tasks() {
        use crate::agent::agent_tests::*;

        let tmp = tempfile::tempdir().unwrap();
        let provider = mock_provider(vec![
            "task completed successfully".into(),
            "task completed successfully".into(),
        ]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

        let id1 = spawn_worker_and_wait_completed_shared(&mut agent, tmp.path()).await;
        let id2 = spawn_worker_and_wait_completed_shared(&mut agent, tmp.path()).await;

        let dir = agent
            .services
            .orchestration
            .subagent_manager
            .as_ref()
            .unwrap()
            .agent_transcript_dir(&id1)
            .unwrap()
            .to_path_buf();
        append_tool_round(&dir.join(format!("{id1}.jsonl"))).await;

        let mut graph = zeph_orchestration::TaskGraph::new("goal");
        graph.tasks = vec![
            completed_task_with_agent(0, &id1),
            completed_task_with_agent(1, &id2),
        ];

        let paths = agent.resolve_whole_plan_trace_paths(&graph).unwrap();
        let union = Agent::<MockChannel>::build_whole_plan_tool_trace(paths)
            .await
            .expect("both transcripts are intact");
        assert!(
            union
                .iter()
                .any(|t| t.tool == "bash" && t.args_summary.as_deref() == Some("cargo test")),
            "union must include the tool call recorded on task 0's transcript: {union:?}"
        );
    }

    /// Regression test for issue #6288 (hazard found during implementation, not part of the
    /// original debugger sketch): `run_whole_plan_verify` runs strictly after `run_scheduler_loop`
    /// returns, i.e. after every per-tick `collect_finished_subagents()` call has already reaped
    /// completed spawn-dispatched handles. Before `resolve_whole_plan_trace_paths` was re-plumbed
    /// to use `transcript_path_for` (residency-independent) instead of `agent_transcript_dir`
    /// (requires the handle still resident in `mgr.agents`), wiring `collect()` into the
    /// dispatch path would have silently and permanently degraded whole-plan grounding to `None`
    /// for every plan run — this proves the union still resolves after both handles are
    /// collected, not just while they remain resident (as
    /// `whole_plan_trace_union_combines_multiple_tasks` above already covers).
    #[tokio::test]
    async fn whole_plan_trace_union_resolves_after_handles_are_collected() {
        use crate::agent::agent_tests::*;

        let tmp = tempfile::tempdir().unwrap();
        let provider = mock_provider(vec![
            "task completed successfully".into(),
            "task completed successfully".into(),
        ]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

        let id1 = spawn_worker_and_wait_completed_shared(&mut agent, tmp.path()).await;
        let id2 = spawn_worker_and_wait_completed_shared(&mut agent, tmp.path()).await;

        let dir = agent
            .services
            .orchestration
            .subagent_manager
            .as_ref()
            .unwrap()
            .agent_transcript_dir(&id1)
            .unwrap()
            .to_path_buf();
        append_tool_round(&dir.join(format!("{id1}.jsonl"))).await;

        let mgr = agent
            .services
            .orchestration
            .subagent_manager
            .as_mut()
            .unwrap();
        mgr.collect(&id1).await.expect("collect must succeed");
        mgr.collect(&id2).await.expect("collect must succeed");
        assert!(
            mgr.statuses().is_empty(),
            "both handles must be gone before resolving trace paths, to prove residency \
             independence"
        );

        let mut graph = zeph_orchestration::TaskGraph::new("goal");
        graph.tasks = vec![
            completed_task_with_agent(0, &id1),
            completed_task_with_agent(1, &id2),
        ];

        let paths = agent.resolve_whole_plan_trace_paths(&graph).expect(
            "trace paths must still resolve after collect() — must not degrade to None \
                 merely because the handles are no longer resident",
        );
        let union = Agent::<MockChannel>::build_whole_plan_tool_trace(paths)
            .await
            .expect("post-collection transcripts must still be readable");
        assert!(
            union
                .iter()
                .any(|t| t.tool == "bash" && t.args_summary.as_deref() == Some("cargo test")),
            "post-collection union must still include the recorded tool call: {union:?}"
        );
    }

    /// AC-15 (integration-level): a `RunInline` task anywhere in the DAG (no `agent_id`, its
    /// in-loop trace is never persisted) makes the WHOLE aggregate `None` — fail-open,
    /// reproducing today's ungrounded behavior exactly — even though another task in the same
    /// DAG has a perfectly resolvable transcript.
    #[tokio::test]
    async fn whole_plan_trace_union_none_when_any_task_is_run_inline() {
        use crate::agent::agent_tests::*;

        let tmp = tempfile::tempdir().unwrap();
        let provider = mock_provider(vec!["task completed successfully".into()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);

        let id1 = spawn_worker_and_wait_completed_shared(&mut agent, tmp.path()).await;

        let mut inline_task = zeph_orchestration::TaskNode::new(1, "t1", "d");
        inline_task.status = zeph_orchestration::TaskStatus::Completed;
        inline_task.result = Some(zeph_orchestration::TaskResult {
            output: "done inline".to_string(),
            artifacts: vec![],
            duration_ms: 0,
            agent_id: None, // RunInline: no agent_id, trace is ephemeral/never persisted.
            agent_def: None,
        });

        let mut graph = zeph_orchestration::TaskGraph::new("goal");
        graph.tasks = vec![completed_task_with_agent(0, &id1), inline_task];

        let paths = agent.resolve_whole_plan_trace_paths(&graph);
        assert!(
            paths.is_none(),
            "any RunInline task in the DAG must degrade the whole aggregate to None (fail-open)"
        );
    }

    /// AC-16 (empty/single-task DAG): no completed tasks in the graph means the trace-path
    /// resolution loop never runs, and the union is vacuously available (`Some(vec![])`) — the
    /// caller (`run_whole_plan_verify`) never actually reaches this path in practice since
    /// `truncated_output.is_empty()` already short-circuits first, but the helper itself must
    /// not spuriously report unavailability for an empty task set.
    #[tokio::test]
    async fn whole_plan_trace_union_empty_graph_is_vacuously_available() {
        use crate::agent::agent_tests::*;

        let agent = QuickTestAgent::minimal("noop").agent;
        let graph = zeph_orchestration::TaskGraph::new("goal");

        let paths = agent
            .resolve_whole_plan_trace_paths(&graph)
            .expect("no completed tasks means vacuously available, not unavailable");
        assert!(paths.is_empty());

        let union = Agent::<MockChannel>::build_whole_plan_tool_trace(paths)
            .await
            .expect("empty path list must resolve to Some(vec![])");
        assert!(union.is_empty());
    }

    /// End-to-end wiring test for `run_whole_plan_verify` itself (code review Important-1):
    /// every sub-component (trace-union building, `verify_plan` grounding) is covered in
    /// isolation above, but nothing previously called `run_whole_plan_verify` directly — this
    /// is the project's documented "wire X into Y" defect class (a piece built and unit-tested
    /// in isolation, but never proven reachable from its real call site). A hallucinated
    /// whole-plan claim (unmatched against the completed task's real, empty trace) must flow
    /// through grounding -> `should_replan` -> `replan_from_plan` -> `execute_partial_replan_dag`
    /// and produce a non-`None`, non-empty result — proving the pipeline is genuinely wired, not
    /// just each piece in isolation.
    #[tokio::test]
    async fn run_whole_plan_verify_end_to_end_hallucinated_claim_triggers_replan() {
        use crate::agent::agent_tests::*;
        use zeph_orchestration::{DagScheduler, GraphStatus, RuleBasedRouter};

        let tmp = tempfile::tempdir().unwrap();
        let provider = mock_provider(vec![
            "task completed successfully".into(),
            r#"{"complete": true, "gaps": [], "confidence": 0.5,
                "claimed_executions": ["bash: cargo test"]}"#
                .into(),
            r#"{"tasks": [{"title": "fix gap", "description": "address the gap",
                "agent_hint": null}]}"#
                .into(),
            "gap task completed successfully".into(),
        ]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
        agent.services.orchestration.orchestration_config = crate::config::OrchestrationConfig {
            enabled: true,
            verify_completeness: true,
            ..crate::config::OrchestrationConfig::default()
        };

        let id1 = spawn_worker_and_wait_completed_shared(&mut agent, tmp.path()).await;

        let mut graph = zeph_orchestration::TaskGraph::new("goal");
        graph.tasks = vec![completed_task_with_agent(0, &id1)];

        let available_agents = agent
            .services
            .orchestration
            .subagent_manager
            .as_ref()
            .map(|m| m.definitions().to_vec())
            .unwrap_or_default();
        let mut scheduler = DagScheduler::resume_from(
            graph,
            &agent.services.orchestration.orchestration_config,
            Box::new(RuleBasedRouter),
            available_agents,
            None,
        )
        .unwrap();

        let result = agent
            .run_whole_plan_verify(&mut scheduler, GraphStatus::Completed)
            .await;

        let extra_tasks = result.expect(
            "hallucinated whole-plan claim must trigger the full grounding -> replan -> \
             execute pipeline, not silently fail open",
        );
        assert!(
            !extra_tasks.is_empty(),
            "the replan must actually produce and execute at least one gap task"
        );
    }

    /// Companion to the hallucinated-claim wiring test above: an honest whole-plan claim
    /// (matches nothing because there is nothing to match, and the LLM claims nothing) must
    /// pass through `run_whole_plan_verify` end-to-end and correctly return `None` — no
    /// spurious replan triggered by the real pipeline.
    #[tokio::test]
    async fn run_whole_plan_verify_end_to_end_honest_claim_returns_none() {
        use crate::agent::agent_tests::*;
        use zeph_orchestration::{DagScheduler, GraphStatus, RuleBasedRouter};

        let tmp = tempfile::tempdir().unwrap();
        let provider = mock_provider(vec![
            "task completed successfully".into(),
            r#"{"complete": true, "gaps": [], "confidence": 0.95}"#.into(),
        ]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
        agent.services.orchestration.orchestration_config = crate::config::OrchestrationConfig {
            enabled: true,
            verify_completeness: true,
            ..crate::config::OrchestrationConfig::default()
        };

        let id1 = spawn_worker_and_wait_completed_shared(&mut agent, tmp.path()).await;

        let mut graph = zeph_orchestration::TaskGraph::new("goal");
        graph.tasks = vec![completed_task_with_agent(0, &id1)];

        let available_agents = agent
            .services
            .orchestration
            .subagent_manager
            .as_ref()
            .map(|m| m.definitions().to_vec())
            .unwrap_or_default();
        let mut scheduler = DagScheduler::resume_from(
            graph,
            &agent.services.orchestration.orchestration_config,
            Box::new(RuleBasedRouter),
            available_agents,
            None,
        )
        .unwrap();

        let result = agent
            .run_whole_plan_verify(&mut scheduler, GraphStatus::Completed)
            .await;

        assert!(
            result.is_none(),
            "an honest, grounded whole-plan verdict must not trigger a replan: {result:?}"
        );
    }
}
