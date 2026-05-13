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

    pub(super) fn build_dag_scheduler(
        &mut self,
        graph: zeph_orchestration::TaskGraph,
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

        // Build admission gate from providers that have `max_concurrent` set (C1 fix).
        let admission_gate = {
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

        let scheduler = if graph.status == GraphStatus::Created {
            DagScheduler::new(
                graph,
                &self.services.orchestration.orchestration_config,
                Box::new(RuleBasedRouter),
                available_agents,
                admission_gate,
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

        let (mut scheduler, reserved) = self.build_dag_scheduler(graph)?;

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

        // Defensive save before `?` so a scheduler error still commits the last in-flight state.
        if let Some(ref persistence) = self.services.orchestration.graph_persistence {
            super::scheduler_loop::save_graph_snapshot(persistence, scheduler.graph().clone())
                .await;
        }

        let final_status = scheduler_result?;

        let extra_task_outputs = self
            .run_whole_plan_verify(&mut scheduler, final_status)
            .await;

        let mut completed_graph = scheduler.into_graph();

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

        let verify_provider = self
            .services
            .orchestration
            .verify_provider
            .as_ref()
            .or(self.services.orchestration.orchestrator_provider.as_ref())
            .unwrap_or(&self.provider)
            .clone();
        let mut verifier = PlanVerifier::new(
            verify_provider,
            self.services.security.sanitizer.clone(),
            &self.services.orchestration.orchestration_config,
        );
        let result = verifier
            .verify_plan(&goal, &truncated_output)
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

        if !should_replan {
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
                return None;
            }
        };

        if gap_tasks.is_empty() {
            return None;
        }

        self.execute_partial_replan_dag(gap_tasks, &goal).await
    }

    pub(super) async fn execute_partial_replan_dag(
        &mut self,
        gap_tasks: Vec<zeph_orchestration::TaskNode>,
        goal: &str,
    ) -> Option<Vec<zeph_orchestration::TaskNode>> {
        use zeph_orchestration::{DagScheduler, RuleBasedRouter, TaskStatus};

        let mut partial_graph = zeph_orchestration::TaskGraph::new(goal);
        partial_graph.tasks = gap_tasks;

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

        let mut partial_scheduler = match DagScheduler::new(
            partial_graph,
            &partial_config,
            Box::new(RuleBasedRouter),
            available_agents,
            partial_admission_gate,
        ) {
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
        self.update_metrics(|m| {
            m.orchestration.tasks_completed += completed_count;
            m.orchestration.tasks_skipped += skipped_count;
        });

        let aggregator_provider = self
            .services
            .orchestration
            .orchestrator_provider
            .as_ref()
            .unwrap_or(&self.provider)
            .clone();
        let aggregator = LlmAggregator::new(
            aggregator_provider,
            &self.services.orchestration.orchestration_config,
        );
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

        let planner_provider = self
            .services
            .orchestration
            .planner_provider
            .as_ref()
            .unwrap_or(&self.provider)
            .clone();
        let planner = LlmPlanner::new(
            planner_provider,
            &self.services.orchestration.orchestration_config,
        );
        let embed_model = self.services.skill.embedding_model.clone();
        let max_tasks = self.services.orchestration.orchestration_config.max_tasks;
        let (graph, planner_usage) = {
            use zeph_orchestration::Planner as _;
            let use_cache = topology_hint
                .as_ref()
                .is_none_or(|h| h.prompt_sentence().is_none());
            let planner_timeout = std::time::Duration::from_secs(
                self.services
                    .orchestration
                    .orchestration_config
                    .planner_timeout_secs,
            );
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
        use zeph_orchestration::GraphStatus;
        let Some(ref graph) = self.services.orchestration.pending_graph else {
            return "No active plan.".to_owned();
        };
        match graph.status {
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
        }
        .to_owned()
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
        use zeph_orchestration::{GraphStatus, dag};

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

        dag::reset_for_retry(&mut graph)
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
