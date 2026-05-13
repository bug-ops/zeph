// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! DAG execution scheduler: drives task graph execution by emitting `SchedulerAction` commands.

mod persistence;
mod planner;
mod router;
mod tick;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use super::cascade::{CascadeConfig, CascadeDetector};
use super::dag;
use super::error::OrchestrationError;
use super::graph::{GraphStatus, TaskGraph, TaskId, TaskStatus};
use super::router::AgentRouter;
use super::topology::{TopologyAnalysis, TopologyClassifier};
pub(super) use super::verifier::inject_tasks as verifier_inject_tasks;
use zeph_config::OrchestrationConfig;
use zeph_sanitizer::{ContentIsolationConfig, ContentSanitizer};

/// Actions the scheduler requests the caller to perform.
///
/// The scheduler never holds `&mut SubAgentManager` — it produces these
/// command values for the caller to execute against its own agent pool (ADR-026
/// command pattern). Process each action, then call [`DagScheduler::record_spawn`] /
/// [`DagScheduler::record_spawn_failure`] for spawn outcomes, and
/// [`DagScheduler::wait_event`] before the next tick.
///
/// # Examples
///
/// ```rust,ignore
/// loop {
///     for action in scheduler.tick() {
///         match action {
///             SchedulerAction::Spawn { task_id, agent_def_name, prompt } => {
///                 match manager.spawn_for_task(task_id, &agent_def_name, &prompt) {
///                     Ok(handle_id) => scheduler.record_spawn(task_id, handle_id, agent_def_name),
///                     Err(e) => {
///                         for a in scheduler.record_spawn_failure(task_id, &e) {
///                             // execute cancel action…
///                         }
///                     }
///                 }
///             }
///             SchedulerAction::Cancel { agent_handle_id } => manager.cancel(&agent_handle_id),
///             SchedulerAction::Done { .. } => break,
///             _ => {}
///         }
///     }
///     scheduler.wait_event().await;
/// }
/// ```
#[derive(Debug)]
pub enum SchedulerAction {
    /// Spawn a sub-agent for the given task using the named agent definition.
    Spawn {
        /// Task to be executed.
        task_id: TaskId,
        /// Name of the agent definition to instantiate.
        agent_def_name: String,
        /// Full prompt to pass to the sub-agent.
        prompt: String,
    },
    /// Cancel a running sub-agent (issued on graph abort or `Skip` propagation).
    Cancel {
        /// Opaque handle ID returned by the sub-agent manager at spawn time.
        agent_handle_id: String,
    },
    /// Execute a task inline via the main agent (emitted when no sub-agents are configured).
    RunInline {
        /// Task to execute inline.
        task_id: TaskId,
        /// Full prompt for the inline execution.
        prompt: String,
    },
    /// Graph reached a terminal or paused state. The caller should stop looping.
    Done {
        /// Final graph status.
        status: GraphStatus,
    },
    /// Request predicate evaluation for a completed task.
    ///
    /// Emitted idempotently from `tick()` for every `Completed` task whose
    /// `verify_predicate.is_some()` AND `predicate_outcome.is_none()`. The caller
    /// must evaluate the predicate and call [`DagScheduler::record_predicate_outcome`].
    ///
    /// Downstream tasks remain blocked by `dag::ready_tasks()` until
    /// `predicate_outcome.passed == true`.
    VerifyPredicate {
        /// Task whose output must be evaluated.
        task_id: TaskId,
        /// The verification predicate to evaluate.
        predicate: super::verify_predicate::VerifyPredicate,
        /// Raw output text produced by the task.
        output: String,
    },
    /// Request verification of a completed task's output (emitted when `verify_completeness=true`).
    ///
    /// The task remains `Completed` during verification. Downstream tasks are unblocked
    /// immediately — verification is best-effort and does not gate dispatch. The caller
    /// should run [`PlanVerifier::verify`], optionally [`PlanVerifier::replan`], and then
    /// call [`DagScheduler::inject_tasks`] if new tasks were generated.
    ///
    /// [`PlanVerifier::verify`]: crate::verifier::PlanVerifier::verify
    /// [`PlanVerifier::replan`]: crate::verifier::PlanVerifier::replan
    Verify {
        /// Task whose output should be verified.
        task_id: TaskId,
        /// The raw output text produced by the task.
        output: String,
    },
}

/// Event sent by a sub-agent loop when it terminates.
///
/// Sub-agent tasks send this through the channel cloned from
/// [`DagScheduler::event_sender`]. The scheduler matches `agent_handle_id`
/// against its running map to guard against stale events from timed-out agents.
#[derive(Debug)]
pub struct TaskEvent {
    /// Task that finished.
    pub task_id: TaskId,
    /// Opaque handle ID that was returned by the sub-agent manager at spawn time.
    pub agent_handle_id: String,
    /// Success or failure outcome.
    pub outcome: TaskOutcome,
}

/// Outcome of a sub-agent execution.
///
/// Returned inside a [`TaskEvent`] and processed by [`DagScheduler::tick`].
#[derive(Debug)]
pub enum TaskOutcome {
    /// Agent completed successfully.
    Completed {
        /// Raw text output.
        output: String,
        /// File-system artifacts produced (may be empty).
        artifacts: Vec<PathBuf>,
    },
    /// Agent failed.
    Failed {
        /// Human-readable error description.
        error: String,
    },
}

/// Tracks a running task's spawn time and definition name for timeout detection.
pub(super) struct RunningTask {
    pub(super) agent_handle_id: String,
    pub(super) agent_def_name: String,
    pub(super) started_at: Instant,
    /// Admission control permit; dropped when the task completes to release the provider slot.
    /// `OwnedSemaphorePermit` does not implement `Debug`, so the manual `Debug` impl on
    /// `DagScheduler` skips `running` (it logs `running_count` instead).
    #[allow(dead_code)]
    pub(super) admission_permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

/// DAG execution engine.
///
/// Drives task graph execution by producing `SchedulerAction` values
/// that the caller executes against `SubAgentManager`.
///
/// # Caller Loop
///
/// ```text
/// loop {
///     let actions = scheduler.tick();
///     for action in actions {
///         match action {
///             Spawn { task_id, agent_def_name, prompt } => {
///                 match manager.spawn_for_task(...) {
///                     Ok(handle_id) => scheduler.record_spawn(task_id, handle_id),
///                     Err(e) => { for a in scheduler.record_spawn_failure(task_id, &e) { /* exec */ } }
///                 }
///             }
///             Cancel { agent_handle_id } => { manager.cancel(&agent_handle_id); }
///             Done { .. } => break,
///         }
///     }
///     scheduler.wait_event().await;
/// }
/// ```
#[allow(clippy::struct_excessive_bools)] // independent boolean flags; bitflags or enum would obscure semantics without reducing complexity
pub struct DagScheduler {
    pub(super) graph: TaskGraph,
    pub(super) max_parallel: usize,
    /// Immutable base parallelism limit from config. Never changes after construction.
    ///
    /// `max_parallel` is derived from this via `TopologyClassifier::compute_max_parallel`
    /// and may be lower (e.g., 1 for `LinearChain`). Using `config_max_parallel` as the
    /// base prevents drift: successive replan cycles always compute from the original
    /// config value, not from a previously reduced `max_parallel`.
    pub(super) config_max_parallel: usize,
    /// Maps `TaskId` -> running sub-agent state.
    pub(super) running: HashMap<TaskId, RunningTask>,
    /// Receives completion/failure events from sub-agent loops.
    pub(super) event_rx: mpsc::Receiver<TaskEvent>,
    /// Sender cloned into each spawned sub-agent via `spawn_for_task`.
    pub(super) event_tx: mpsc::Sender<TaskEvent>,
    /// Per-task wall-clock timeout.
    pub(super) task_timeout: Duration,
    /// Router for agent selection.
    pub(super) router: Box<dyn AgentRouter>,
    /// Available agent definitions (cached from `SubAgentManager`).
    pub(super) available_agents: Vec<zeph_subagent::SubAgentDef>,
    /// Total character budget for cross-task dependency context injection.
    pub(super) dependency_context_budget: usize,
    /// Events buffered by `wait_event` for processing in the next `tick`.
    pub(super) buffered_events: VecDeque<TaskEvent>,
    /// Sanitizer for dependency output injected into task prompts (SEC-ORCH-01).
    pub(super) sanitizer: ContentSanitizer,
    /// Backoff duration before retrying deferred tasks when all ready tasks hit the concurrency limit.
    pub(super) deferral_backoff: Duration,
    /// Consecutive spawn failures due to concurrency limits. Used to compute exponential backoff.
    pub(super) consecutive_spawn_failures: u32,
    /// Topology analysis result. Recomputed on next tick when `topology_dirty=true`.
    pub(super) topology: TopologyAnalysis,
    /// When true, topology is re-analyzed at the start of the next tick.
    /// Set by `inject_tasks()` after appending replan tasks (critic C2).
    pub(super) topology_dirty: bool,
    /// Current dispatch level for `LevelBarrier` strategy.
    pub(super) current_level: usize,
    /// Whether post-task verification is enabled (`config.verify_completeness`).
    pub(super) verify_completeness: bool,
    /// Provider name for verification LLM calls (`config.verify_provider`).
    /// Empty string = use the agent's primary provider.
    pub(super) verify_provider: String,
    /// Per-task replan count. Limits replanning to 1 cycle per task (critic S2).
    pub(super) task_replan_counts: HashMap<TaskId, u32>,
    /// Global replan counter across the entire scheduler run (critic S2).
    pub(super) global_replan_count: u32,
    /// Global replan hard cap from config.
    pub(super) max_replans: u32,
    /// Completeness score threshold from config. Replan is triggered when
    /// `VerificationResult::confidence < completeness_threshold_value` AND gaps exist.
    pub(super) completeness_threshold_value: f32,
    /// Cascade failure detector. `Some` when `cascade_routing = true`.
    pub(super) cascade_detector: Option<CascadeDetector>,
    /// Whether `tree_optimized_dispatch` was enabled at construction.
    /// Stored so the dirty-reanalysis path can reproduce the same strategy mapping.
    pub(super) tree_optimized_dispatch: bool,
    /// Whether `cascade_routing` was enabled at construction.
    pub(super) cascade_routing: bool,
    /// Per-task error lineage chains. Side-table on scheduler — NOT on `TaskNode` (S4).
    /// Reset on `inject_tasks()` mirroring `cascade_detector` reset.
    pub(super) lineage_chains: HashMap<TaskId, super::lineage::ErrorLineage>,
    /// Consecutive-chain abort threshold from config.
    pub(super) cascade_chain_threshold: usize,
    /// Fan-out failure-rate abort threshold from config (0.0 = disabled).
    pub(super) cascade_failure_rate_abort_threshold: f32,
    /// TTL for lineage entries in seconds.
    pub(super) lineage_ttl_secs: u64,
    /// Whether the predicate gate is enabled (`config.verify_predicate_enabled`).
    pub(super) verify_predicate_enabled: bool,
    /// Provider name for predicate evaluation (empty = fall back to `orchestrator_provider`
    /// then `verify_provider` then primary).
    pub(super) predicate_provider: String,
    /// Provider name for scheduling-tier LLM calls (empty = fall back to primary).
    /// Stored for logging and diagnostics; actual resolution happens in `zeph-core`.
    pub(super) orchestrator_provider: String,
    /// Maximum predicate-driven re-runs across the whole DAG (S1 — independent of `max_replans`).
    pub(super) max_predicate_replans: u32,
    /// Counter of predicate-driven re-runs used so far.
    pub(super) predicate_replans_used: u32,
    /// Per-task accumulated predicate failure reasons, injected into the re-run prompt.
    pub(super) predicate_reasons: HashMap<TaskId, String>,
    /// Per-provider admission gate. `None` when no provider has `max_concurrent` set.
    pub(super) admission_gate: Option<super::admission::AdmissionGate>,
    /// Default per-task cost budget in cents (`0.0` = unlimited).
    pub(super) default_task_budget_cents: f64,
    /// Temporary holding map for admission permits between `dispatch_ready_tasks` and
    /// `record_spawn`. Permits are transferred into `RunningTask::admission_permit` when
    /// the caller confirms a successful spawn via `record_spawn`.
    pub(super) pending_permits: HashMap<TaskId, tokio::sync::OwnedSemaphorePermit>,
    /// Maps agent definition name → provider name for admission gate lookups.
    ///
    /// Built at construction from `available_agents[*].model` (`ModelSpec::Named(provider_name)`).
    /// Agents with `model = None` or `Inherit` are absent from this map; their tasks
    /// bypass the gate (treated as ungated).
    pub(super) agent_provider_map: HashMap<String, String>,
}

impl std::fmt::Debug for DagScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DagScheduler")
            .field("graph_id", &self.graph.id)
            .field("graph_status", &self.graph.status)
            .field("running_count", &self.running.len())
            .field("max_parallel", &self.max_parallel)
            .field("task_timeout_secs", &self.task_timeout.as_secs())
            .field("topology", &self.topology.topology)
            .field("strategy", &self.topology.strategy)
            .field("current_level", &self.current_level)
            .field("global_replan_count", &self.global_replan_count)
            .field("cascade_routing", &self.cascade_routing)
            .field("tree_optimized_dispatch", &self.tree_optimized_dispatch)
            .finish_non_exhaustive()
    }
}

impl DagScheduler {
    /// Create a new scheduler for the given graph.
    ///
    /// The graph must be in `Created` status. The scheduler transitions
    /// it to `Running` and marks root tasks as `Ready`.
    ///
    /// # Errors
    ///
    /// Returns `OrchestrationError::InvalidGraph` if the graph is not in
    /// `Created` status or has no tasks.
    pub fn new(
        mut graph: TaskGraph,
        config: &OrchestrationConfig,
        router: Box<dyn AgentRouter>,
        available_agents: Vec<zeph_subagent::SubAgentDef>,
        admission_gate: Option<super::admission::AdmissionGate>,
    ) -> Result<Self, OrchestrationError> {
        if graph.status != GraphStatus::Created {
            return Err(OrchestrationError::InvalidGraph(format!(
                "graph must be in Created status, got {}",
                graph.status
            )));
        }

        dag::validate(&graph.tasks, config.max_tasks as usize)?;

        graph.status = GraphStatus::Running;

        for task in &mut graph.tasks {
            if task.depends_on.is_empty() && task.status == TaskStatus::Pending {
                task.status = TaskStatus::Ready;
            }
        }

        let agent_provider_map: HashMap<String, String> = available_agents
            .iter()
            .filter_map(|def| {
                if let Some(zeph_subagent::ModelSpec::Named(provider)) = &def.model {
                    Some((def.name.clone(), provider.clone()))
                } else {
                    None
                }
            })
            .collect();

        let (event_tx, event_rx) = mpsc::channel(64);

        let task_timeout = if config.task_timeout_secs > 0 {
            Duration::from_secs(config.task_timeout_secs)
        } else {
            Duration::from_mins(10)
        };

        let topology = TopologyClassifier::analyze(&graph, config);
        let max_parallel = topology.max_parallel;
        let config_max_parallel = config.max_parallel as usize;

        if config.topology_selection {
            tracing::debug!(
                topology = ?topology.topology,
                strategy = ?topology.strategy,
                max_parallel,
                "topology-aware concurrency limit applied"
            );
        }

        // Validate cascade_routing dependency on topology_selection.
        if config.cascade_routing && !config.topology_selection {
            tracing::warn!(
                "cascade_routing = true requires topology_selection = true; \
                 cascade routing is disabled (topology_selection is off)"
            );
        }

        let cascade_detector = if config.cascade_routing && config.topology_selection {
            Some(CascadeDetector::new(CascadeConfig {
                failure_threshold: config.cascade_failure_threshold,
            }))
        } else {
            None
        };

        Ok(Self {
            graph,
            max_parallel,
            config_max_parallel,
            running: HashMap::new(),
            event_rx,
            event_tx,
            task_timeout,
            router,
            available_agents,
            dependency_context_budget: config.dependency_context_budget,
            buffered_events: VecDeque::new(),
            sanitizer: ContentSanitizer::new(&ContentIsolationConfig::default()),
            deferral_backoff: Duration::from_millis(config.deferral_backoff_ms),
            consecutive_spawn_failures: 0,
            topology,
            topology_dirty: false,
            current_level: 0,
            verify_completeness: config.verify_completeness,
            verify_provider: config.verify_provider.as_str().trim().to_owned(),
            task_replan_counts: HashMap::new(),
            global_replan_count: 0,
            max_replans: config.max_replans,
            completeness_threshold_value: config.completeness_threshold,
            cascade_detector,
            tree_optimized_dispatch: config.tree_optimized_dispatch,
            cascade_routing: config.cascade_routing && config.topology_selection,
            lineage_chains: HashMap::new(),
            cascade_chain_threshold: config.cascade_chain_threshold,
            cascade_failure_rate_abort_threshold: config.cascade_failure_rate_abort_threshold,
            lineage_ttl_secs: config.lineage_ttl_secs,
            verify_predicate_enabled: config.verify_predicate_enabled,
            predicate_provider: config.predicate_provider.as_str().trim().to_owned(),
            orchestrator_provider: config.orchestrator_provider.as_str().trim().to_owned(),
            max_predicate_replans: config.max_predicate_replans,
            predicate_replans_used: 0,
            predicate_reasons: HashMap::new(),
            admission_gate,
            default_task_budget_cents: config.default_task_budget_cents,
            pending_permits: HashMap::new(),
            agent_provider_map,
        })
    }

    /// Create a scheduler from a graph that is in `Paused` or `Failed` status.
    ///
    /// Used for resume and retry flows. The caller is responsible for calling
    /// [`dag::reset_for_retry`] (for retry) before passing the graph here.
    ///
    /// This constructor sets `graph.status = Running` (II3) and reconstructs
    /// the `running` map from tasks that are still in `Running` state (IC1), so
    /// their completion events are not silently dropped on the next tick.
    ///
    /// # Errors
    ///
    /// Returns `OrchestrationError::InvalidGraph` if the graph is in `Completed`
    /// or `Canceled` status (terminal states that cannot be resumed).
    pub fn resume_from(
        mut graph: TaskGraph,
        config: &OrchestrationConfig,
        router: Box<dyn AgentRouter>,
        available_agents: Vec<zeph_subagent::SubAgentDef>,
        admission_gate: Option<super::admission::AdmissionGate>,
    ) -> Result<Self, OrchestrationError> {
        if graph.status == GraphStatus::Completed || graph.status == GraphStatus::Canceled {
            return Err(OrchestrationError::InvalidGraph(format!(
                "cannot resume a {} graph; only Paused, Failed, or Running graphs are resumable",
                graph.status
            )));
        }

        // II3: ensure the graph is in Running state so tick() does not immediately
        // return Done{Paused}.
        graph.status = GraphStatus::Running;

        // IC1: reconstruct the `running` map from tasks that were still Running at
        // pause time. Without this their completion events would arrive but
        // process_event would ignore them (it checks self.running), leaving the
        // task stuck until timeout.
        let running: HashMap<TaskId, RunningTask> = graph
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Running)
            .filter_map(|t| {
                let handle_id = t.assigned_agent.clone()?;
                let def_name = t.agent_hint.clone().unwrap_or_default();
                Some((
                    t.id,
                    RunningTask {
                        agent_handle_id: handle_id,
                        agent_def_name: def_name,
                        // Conservative: treat as just-started so timeout window is reset.
                        started_at: Instant::now(),
                        // Permits are not persisted; resumed tasks start without a slot.
                        admission_permit: None,
                    },
                ))
            })
            .collect();

        let agent_provider_map: HashMap<String, String> = available_agents
            .iter()
            .filter_map(|def| {
                if let Some(zeph_subagent::ModelSpec::Named(provider)) = &def.model {
                    Some((def.name.clone(), provider.clone()))
                } else {
                    None
                }
            })
            .collect();

        let (event_tx, event_rx) = mpsc::channel(64);

        let task_timeout = if config.task_timeout_secs > 0 {
            Duration::from_secs(config.task_timeout_secs)
        } else {
            Duration::from_mins(10)
        };

        let topology = TopologyClassifier::analyze(&graph, config);
        let max_parallel = topology.max_parallel;
        let config_max_parallel = config.max_parallel as usize;

        let cascade_detector = if config.cascade_routing && config.topology_selection {
            Some(CascadeDetector::new(CascadeConfig {
                failure_threshold: config.cascade_failure_threshold,
            }))
        } else {
            None
        };

        Ok(Self {
            graph,
            max_parallel,
            config_max_parallel,
            running,
            event_rx,
            event_tx,
            task_timeout,
            router,
            available_agents,
            dependency_context_budget: config.dependency_context_budget,
            buffered_events: VecDeque::new(),
            sanitizer: ContentSanitizer::new(&ContentIsolationConfig::default()),
            deferral_backoff: Duration::from_millis(config.deferral_backoff_ms),
            consecutive_spawn_failures: 0,
            topology,
            topology_dirty: false,
            current_level: 0,
            verify_completeness: config.verify_completeness,
            verify_provider: config.verify_provider.as_str().trim().to_owned(),
            task_replan_counts: HashMap::new(),
            global_replan_count: 0,
            max_replans: config.max_replans,
            completeness_threshold_value: config.completeness_threshold,
            cascade_detector,
            tree_optimized_dispatch: config.tree_optimized_dispatch,
            cascade_routing: config.cascade_routing && config.topology_selection,
            lineage_chains: HashMap::new(),
            cascade_chain_threshold: config.cascade_chain_threshold,
            cascade_failure_rate_abort_threshold: config.cascade_failure_rate_abort_threshold,
            lineage_ttl_secs: config.lineage_ttl_secs,
            verify_predicate_enabled: config.verify_predicate_enabled,
            predicate_provider: config.predicate_provider.as_str().trim().to_owned(),
            orchestrator_provider: config.orchestrator_provider.as_str().trim().to_owned(),
            max_predicate_replans: config.max_predicate_replans,
            predicate_replans_used: 0,
            predicate_reasons: HashMap::new(),
            admission_gate,
            default_task_budget_cents: config.default_task_budget_cents,
            // `pending_permits` is not persisted; resume always starts with an empty map.
            // Resumed tasks that were `Running` start without an admission permit (see
            // `RunningTask::admission_permit: None` above). This is intentional: their
            // slots were released when the process exited and must be re-acquired on next
            // dispatch if the task is re-spawned.
            pending_permits: HashMap::new(),
            agent_provider_map,
        })
    }

    /// Validate that `verify_provider` references a known provider name.
    ///
    /// Call this after construction when `verify_completeness = true` to catch
    /// misconfiguration early rather than failing open at runtime.
    ///
    /// - Empty `verify_provider` is always valid (falls back to the primary provider).
    /// - If `provider_names` is empty, validation is skipped (provider set is unknown).
    /// - Provider names are compared case-sensitively (matching the existing resolution convention).
    ///
    /// # Errors
    ///
    /// Returns `OrchestrationError::InvalidConfig` when `verify_completeness = true`,
    /// `verify_provider` is non-empty, and the name is not present in `provider_names`.
    pub fn validate_verify_config(
        &self,
        provider_names: &[&str],
    ) -> Result<(), OrchestrationError> {
        if !self.verify_completeness {
            return Ok(());
        }
        let name = self.verify_provider.as_str();
        if name.is_empty() || provider_names.is_empty() {
            return Ok(());
        }
        if !provider_names.contains(&name) {
            return Err(OrchestrationError::InvalidConfig(format!(
                "verify_provider \"{}\" not found in [[llm.providers]]; available: [{}]",
                name,
                provider_names.join(", ")
            )));
        }
        Ok(())
    }

    /// Get a clone of the event sender for injection into sub-agent loops.
    #[must_use]
    pub fn event_sender(&self) -> mpsc::Sender<TaskEvent> {
        self.event_tx.clone()
    }

    /// Immutable reference to the current graph state.
    #[must_use]
    pub fn graph(&self) -> &TaskGraph {
        &self.graph
    }

    /// Return the final graph state.
    ///
    /// Clones the graph since `Drop` is implemented on the scheduler.
    #[must_use]
    pub fn into_graph(&self) -> TaskGraph {
        self.graph.clone()
    }

    /// Current topology analysis.
    #[must_use]
    pub fn topology(&self) -> &TopologyAnalysis {
        &self.topology
    }

    /// Minimum completeness score threshold from config.
    ///
    /// Used by the agent loop to gate whole-plan replan: replan is triggered when
    /// `VerificationResult::confidence < completeness_threshold` AND gaps exist.
    #[must_use]
    pub fn completeness_threshold(&self) -> f32 {
        self.completeness_threshold_value
    }

    /// Provider name for verification LLM calls (empty = use primary provider).
    #[must_use]
    pub fn verify_provider_name(&self) -> &str {
        &self.verify_provider
    }

    /// Provider name for predicate evaluation (empty = fall back to `orchestrator_provider`
    /// then `verify_provider` then primary).
    #[must_use]
    pub fn predicate_provider_name(&self) -> &str {
        &self.predicate_provider
    }

    /// Provider name for scheduling-tier LLM calls (empty = fall back to primary).
    #[must_use]
    pub fn orchestrator_provider_name(&self) -> &str {
        &self.orchestrator_provider
    }

    /// Whether the predicate gate is enabled.
    #[must_use]
    pub fn verify_predicate_enabled(&self) -> bool {
        self.verify_predicate_enabled
    }

    /// Remaining whole-plan replan budget: `max_replans - global_replan_count`.
    ///
    /// Returns 0 when the global cap has been reached.
    #[must_use]
    pub fn max_replans_remaining(&self) -> u32 {
        self.max_replans.saturating_sub(self.global_replan_count)
    }

    /// Increment `global_replan_count` to record a whole-plan replan cycle.
    ///
    /// Called by the agent loop after executing a partial DAG from whole-plan gaps.
    /// Does NOT inject tasks into the original graph (the partial DAG is a separate run).
    pub fn record_whole_plan_replan(&mut self) {
        self.global_replan_count = self.global_replan_count.saturating_add(1);
    }

    /// Returns `true` when at least one sub-agent task is currently in flight.
    #[must_use]
    pub fn has_running_tasks(&self) -> bool {
        !self.running.is_empty()
    }
}

impl Drop for DagScheduler {
    fn drop(&mut self) {
        if !self.running.is_empty() {
            tracing::warn!(
                running_tasks = self.running.len(),
                "DagScheduler dropped with running tasks; agents may continue until their \
                 CancellationToken fires or they complete naturally"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::default_trait_access)]

    use super::*;
    use crate::graph::{GraphStatus, TaskGraph, TaskNode, TaskStatus};

    pub(super) fn make_node(id: u32, deps: &[u32]) -> TaskNode {
        let mut n = TaskNode::new(
            id,
            format!("task-{id}"),
            format!("description for task {id}"),
        );
        n.depends_on = deps.iter().map(|&d| TaskId(d)).collect();
        n
    }

    pub(super) fn graph_from_nodes(nodes: Vec<TaskNode>) -> TaskGraph {
        let mut g = TaskGraph::new("test goal");
        g.tasks = nodes;
        g
    }

    pub(super) fn make_def(name: &str) -> zeph_subagent::SubAgentDef {
        use zeph_subagent::{SkillFilter, SubAgentPermissions, SubagentHooks, ToolPolicy};
        zeph_subagent::SubAgentDef {
            name: name.to_string(),
            description: format!("{name} agent"),
            model: None,
            tools: ToolPolicy::InheritAll,
            disallowed_tools: vec![],
            permissions: SubAgentPermissions::default(),
            skills: SkillFilter::default(),
            system_prompt: String::new(),
            hooks: SubagentHooks::default(),
            memory: None,
            source: None,
            file_path: None,
        }
    }

    pub(super) fn make_config() -> zeph_config::OrchestrationConfig {
        zeph_config::OrchestrationConfig {
            enabled: true,
            max_tasks: 20,
            max_parallel: 4,
            default_failure_strategy: "abort".to_string(),
            default_max_retries: 3,
            task_timeout_secs: 300,
            planner_provider: Default::default(),
            planner_max_tokens: 4096,
            dependency_context_budget: 16384,
            confirm_before_execute: true,
            aggregator_max_tokens: 4096,
            deferral_backoff_ms: 250,
            plan_cache: zeph_config::PlanCacheConfig::default(),
            topology_selection: false,
            verify_provider: Default::default(),
            verify_max_tokens: 1024,
            max_replans: 2,
            verify_completeness: false,
            completeness_threshold: 0.7,
            tool_provider: Default::default(),
            cascade_routing: false,
            cascade_failure_threshold: 0.5,
            tree_optimized_dispatch: false,
            adaptorch: Default::default(),
            cascade_chain_threshold: 3,
            cascade_failure_rate_abort_threshold: 0.0,
            lineage_ttl_secs: 300,
            verify_predicate_enabled: false,
            predicate_provider: Default::default(),
            max_predicate_replans: 2,
            predicate_timeout_secs: 30,
            persistence_enabled: true,
            orchestrator_provider: Default::default(),
            default_task_budget_cents: 0.0,
            aggregator_timeout_secs: 60,
            planner_timeout_secs: 120,
            verifier_timeout_secs: 30,
        }
    }

    pub(super) struct FirstRouter;
    impl AgentRouter for FirstRouter {
        fn route(
            &self,
            _task: &TaskNode,
            available: &[zeph_subagent::SubAgentDef],
        ) -> Option<String> {
            available.first().map(|d| d.name.clone())
        }
    }

    pub(super) struct NoneRouter;
    impl AgentRouter for NoneRouter {
        fn route(
            &self,
            _task: &TaskNode,
            _available: &[zeph_subagent::SubAgentDef],
        ) -> Option<String> {
            None
        }
    }

    pub(super) fn make_scheduler_with_router(
        graph: TaskGraph,
        router: Box<dyn AgentRouter>,
    ) -> DagScheduler {
        let config = make_config();
        let defs = vec![make_def("worker")];
        DagScheduler::new(graph, &config, router, defs, None).unwrap()
    }

    pub(super) fn make_scheduler(graph: TaskGraph) -> DagScheduler {
        let config = make_config();
        let defs = vec![make_def("worker")];
        DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, None).unwrap()
    }

    // --- constructor tests ---

    #[test]
    fn test_new_validates_graph_status() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.status = GraphStatus::Running; // wrong status
        let config = make_config();
        let result = DagScheduler::new(graph, &config, Box::new(FirstRouter), vec![], None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, OrchestrationError::InvalidGraph(_)));
    }

    #[test]
    fn test_new_marks_roots_ready() {
        let graph = graph_from_nodes(vec![
            make_node(0, &[]),
            make_node(1, &[]),
            make_node(2, &[0, 1]),
        ]);
        let scheduler = make_scheduler(graph);
        assert_eq!(scheduler.graph().tasks[0].status, TaskStatus::Ready);
        assert_eq!(scheduler.graph().tasks[1].status, TaskStatus::Ready);
        assert_eq!(scheduler.graph().tasks[2].status, TaskStatus::Pending);
        assert_eq!(scheduler.graph().status, GraphStatus::Running);
    }

    #[test]
    fn test_new_validates_empty_graph() {
        let graph = graph_from_nodes(vec![]);
        let config = make_config();
        let result = DagScheduler::new(graph, &config, Box::new(FirstRouter), vec![], None);
        assert!(result.is_err());
    }

    #[test]
    fn orchestrator_provider_name_returns_config_value() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let config = zeph_config::OrchestrationConfig {
            orchestrator_provider: zeph_config::ProviderName::new("quality"),
            ..make_config()
        };
        let scheduler =
            DagScheduler::new(graph, &config, Box::new(FirstRouter), vec![], None).unwrap();
        assert_eq!(scheduler.orchestrator_provider_name(), "quality");
    }

    #[test]
    fn orchestrator_provider_name_empty_is_default() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let scheduler =
            DagScheduler::new(graph, &make_config(), Box::new(FirstRouter), vec![], None).unwrap();
        assert!(
            scheduler.orchestrator_provider_name().is_empty(),
            "default config must yield empty orchestrator_provider_name"
        );
    }
}
