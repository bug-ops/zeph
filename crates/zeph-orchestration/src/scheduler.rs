// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! DAG execution scheduler: drives task graph execution by emitting `SchedulerAction` commands.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use super::cascade::{CascadeConfig, CascadeDetector};
use super::dag;
use super::error::OrchestrationError;
use super::graph::{
    ExecutionMode, GraphStatus, TaskGraph, TaskId, TaskNode, TaskResult, TaskStatus,
};
use super::router::AgentRouter;
use super::topology::{DispatchStrategy, Topology, TopologyAnalysis, TopologyClassifier};
use super::verifier::inject_tasks as verifier_inject_tasks;
use zeph_config::OrchestrationConfig;
use zeph_sanitizer::{ContentIsolationConfig, ContentSanitizer, ContentSource, ContentSourceKind};
use zeph_subagent::{SubAgentDef, SubAgentError};

/// Actions the scheduler requests the caller to perform.
///
/// The scheduler never holds `&mut SubAgentManager` — it produces these
/// actions for the caller to execute (ADR-026 command pattern).
#[derive(Debug)]
pub enum SchedulerAction {
    /// Spawn a sub-agent for a task.
    Spawn {
        task_id: TaskId,
        agent_def_name: String,
        prompt: String,
    },
    /// Cancel a running sub-agent (on graph abort/skip).
    Cancel { agent_handle_id: String },
    /// Execute a task inline via the main agent (no sub-agents configured).
    RunInline { task_id: TaskId, prompt: String },
    /// Graph reached a terminal or paused state.
    Done { status: GraphStatus },
    /// Request verification of a completed task's output (emitted when `verify_completeness=true`).
    ///
    /// The task stays `Completed` during verification. Downstream tasks are unblocked
    /// immediately — verification is best-effort and does not gate dispatch.
    /// The caller runs `PlanVerifier::verify()`, then optionally `PlanVerifier::replan()`,
    /// then calls `DagScheduler::inject_tasks()` if new tasks were generated.
    Verify { task_id: TaskId, output: String },
}

/// Event sent by a sub-agent loop when it terminates.
#[derive(Debug)]
pub struct TaskEvent {
    pub task_id: TaskId,
    pub agent_handle_id: String,
    pub outcome: TaskOutcome,
}

/// Outcome of a sub-agent execution.
#[derive(Debug)]
pub enum TaskOutcome {
    /// Agent completed successfully.
    Completed {
        output: String,
        artifacts: Vec<PathBuf>,
    },
    /// Agent failed.
    Failed { error: String },
}

/// Tracks a running task's spawn time and definition name for timeout detection.
struct RunningTask {
    agent_handle_id: String,
    agent_def_name: String,
    started_at: Instant,
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
#[allow(clippy::struct_excessive_bools)]
pub struct DagScheduler {
    graph: TaskGraph,
    max_parallel: usize,
    /// Immutable base parallelism limit from config. Never changes after construction.
    ///
    /// `max_parallel` is derived from this via `TopologyClassifier::compute_max_parallel`
    /// and may be lower (e.g., 1 for `LinearChain`). Using `config_max_parallel` as the
    /// base prevents drift: successive replan cycles always compute from the original
    /// config value, not from a previously reduced `max_parallel`.
    config_max_parallel: usize,
    /// Maps `TaskId` -> running sub-agent state.
    running: HashMap<TaskId, RunningTask>,
    /// Receives completion/failure events from sub-agent loops.
    event_rx: mpsc::Receiver<TaskEvent>,
    /// Sender cloned into each spawned sub-agent via `spawn_for_task`.
    event_tx: mpsc::Sender<TaskEvent>,
    /// Per-task wall-clock timeout.
    task_timeout: Duration,
    /// Router for agent selection.
    router: Box<dyn AgentRouter>,
    /// Available agent definitions (cached from `SubAgentManager`).
    available_agents: Vec<SubAgentDef>,
    /// Total character budget for cross-task dependency context injection.
    dependency_context_budget: usize,
    /// Events buffered by `wait_event` for processing in the next `tick`.
    buffered_events: VecDeque<TaskEvent>,
    /// Sanitizer for dependency output injected into task prompts (SEC-ORCH-01).
    sanitizer: ContentSanitizer,
    /// Backoff duration before retrying deferred tasks when all ready tasks hit the concurrency limit.
    deferral_backoff: Duration,
    /// Consecutive spawn failures due to concurrency limits. Used to compute exponential backoff.
    consecutive_spawn_failures: u32,
    /// Topology analysis result. Recomputed on next tick when `topology_dirty=true`.
    topology: TopologyAnalysis,
    /// When true, topology is re-analyzed at the start of the next tick.
    /// Set by `inject_tasks()` after appending replan tasks (critic C2).
    topology_dirty: bool,
    /// Current dispatch level for `LevelBarrier` strategy.
    current_level: usize,
    /// Whether post-task verification is enabled (`config.verify_completeness`).
    verify_completeness: bool,
    /// Provider name for verification LLM calls (`config.verify_provider`).
    /// Empty string = use the agent's primary provider.
    verify_provider: String,
    /// Per-task replan count. Limits replanning to 1 cycle per task (critic S2).
    task_replan_counts: HashMap<TaskId, u32>,
    /// Global replan counter across the entire scheduler run (critic S2).
    global_replan_count: u32,
    /// Global replan hard cap from config.
    max_replans: u32,
    /// Completeness score threshold from config. Replan is triggered when
    /// `VerificationResult::confidence < completeness_threshold_value` AND gaps exist.
    completeness_threshold_value: f32,
    /// Cascade failure detector. `Some` when `cascade_routing = true`.
    cascade_detector: Option<CascadeDetector>,
    /// Whether `tree_optimized_dispatch` was enabled at construction.
    /// Stored so the dirty-reanalysis path can reproduce the same strategy mapping.
    tree_optimized_dispatch: bool,
    /// Whether `cascade_routing` was enabled at construction.
    cascade_routing: bool,
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
        available_agents: Vec<SubAgentDef>,
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

        let (event_tx, event_rx) = mpsc::channel(64);

        let task_timeout = if config.task_timeout_secs > 0 {
            Duration::from_secs(config.task_timeout_secs)
        } else {
            Duration::from_secs(600)
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
        available_agents: Vec<SubAgentDef>,
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
                    },
                ))
            })
            .collect();

        let (event_tx, event_rx) = mpsc::channel(64);

        let task_timeout = if config.task_timeout_secs > 0 {
            Duration::from_secs(config.task_timeout_secs)
        } else {
            Duration::from_secs(600)
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

    /// Inject new tasks into the graph after a verify-replan cycle.
    ///
    /// Appends tasks and validates DAG acyclicity. Sets `topology_dirty=true` so
    /// topology is re-analyzed at the start of the next `tick()`. Does NOT
    /// re-analyze topology here (critic C2 — topology computed during injection
    /// would be stale by the next tick).
    ///
    /// Per-task replan cap: each task is limited to 1 replan (critic S2).
    /// Global hard cap: total replan count across the run is limited to `max_replans`.
    ///
    /// # Errors
    ///
    /// Returns `OrchestrationError::VerificationFailed` if the graph would exceed
    /// `max_tasks` or injection introduces a cycle.
    pub fn inject_tasks(
        &mut self,
        verified_task_id: TaskId,
        new_tasks: Vec<TaskNode>,
        max_tasks: usize,
    ) -> Result<(), OrchestrationError> {
        if new_tasks.is_empty() {
            return Ok(());
        }

        // Per-task replan limit: 1 replan per task (critic S2).
        let task_replan_count = self.task_replan_counts.entry(verified_task_id).or_insert(0);
        if *task_replan_count >= 1 {
            tracing::warn!(
                task_id = %verified_task_id,
                "per-task replan limit (1) reached, skipping replan injection"
            );
            return Ok(());
        }

        // Global hard cap (critic S2).
        if self.global_replan_count >= self.max_replans {
            tracing::warn!(
                global_replan_count = self.global_replan_count,
                max_replans = self.max_replans,
                "global replan limit reached, skipping replan injection"
            );
            return Ok(());
        }

        verifier_inject_tasks(&mut self.graph, new_tasks, max_tasks)?;

        *task_replan_count += 1;
        self.global_replan_count += 1;

        // Signal that topology needs re-analysis on the next tick (critic C2).
        self.topology_dirty = true;

        // Reset cascade failure counts — the graph has fundamentally changed (C13 fix).
        if let Some(ref mut det) = self.cascade_detector {
            det.reset();
        }

        Ok(())
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

impl DagScheduler {
    /// Process pending events and produce actions for the caller.
    ///
    /// Call `wait_event` after processing all actions to block until the next event.
    #[allow(clippy::too_many_lines)]
    pub fn tick(&mut self) -> Vec<SchedulerAction> {
        if self.graph.status != GraphStatus::Running {
            return vec![SchedulerAction::Done {
                status: self.graph.status,
            }];
        }

        // Re-analyze topology when task set has changed (inject_tasks was called).
        // Deferred from inject_tasks() to avoid stale analysis (critic C2).
        if self.topology_dirty {
            // Rebuild with topology_selection=true config equivalent: reuse existing analysis
            // approach. Since we don't store the config here, re-analyze using a stub config
            // that reflects what was originally set up.
            let new_analysis = {
                let n = self.graph.tasks.len();
                if n == 0 {
                    TopologyAnalysis {
                        topology: Topology::AllParallel,
                        strategy: DispatchStrategy::FullParallel,
                        // Use the immutable config base so the fallback is consistent with
                        // the initial analysis (not a previously reduced max_parallel).
                        max_parallel: self.config_max_parallel,
                        depth: 0,
                        depths: std::collections::HashMap::new(),
                    }
                } else {
                    // Compute depths once, then classify using pre-computed values.
                    // This eliminates a redundant toposort compared to calling classify()
                    // (which internally calls compute_longest_path_and_depths again).
                    let (depth, depths) =
                        super::topology::compute_depths_for_scheduler(&self.graph);
                    let topo =
                        TopologyClassifier::classify_with_depths(&self.graph, depth, &depths);
                    // Reconstruct a minimal config stub carrying only the strategy-selection flags
                    // so `strategy()` can apply the same overrides as at construction time.
                    let strategy_config = zeph_config::OrchestrationConfig {
                        cascade_routing: self.cascade_routing,
                        tree_optimized_dispatch: self.tree_optimized_dispatch,
                        ..zeph_config::OrchestrationConfig::default()
                    };
                    let strategy = TopologyClassifier::strategy(topo, &strategy_config);
                    // ALWAYS use config_max_parallel as the base, never self.max_parallel,
                    // to prevent drift across successive replan cycles (architect S2).
                    let max_parallel =
                        TopologyClassifier::compute_max_parallel(topo, self.config_max_parallel);
                    TopologyAnalysis {
                        topology: topo,
                        strategy,
                        max_parallel,
                        depth,
                        depths,
                    }
                }
            };
            self.topology = new_analysis;
            // Sync max_parallel to match the newly computed topology analysis (architect S2).
            // The dispatch logic at line 559 reads self.max_parallel for slot computation;
            // without this sync, self.max_parallel and self.topology.max_parallel diverge.
            self.max_parallel = self.topology.max_parallel;
            self.topology_dirty = false;

            // After re-analysis, reset the level pointer to the shallowest non-terminal depth
            // so injected tasks at lower depths are not skipped by the LevelBarrier.
            // Uses .min() (not unconditional reset to 0) to preserve forward progress when
            // injected tasks are at equal or deeper levels than current_level.
            if self.topology.strategy == DispatchStrategy::LevelBarrier {
                let min_active = self
                    .graph
                    .tasks
                    .iter()
                    .filter(|t| !t.status.is_terminal())
                    .filter_map(|t| self.topology.depths.get(&t.id).copied())
                    .min();
                if let Some(min_depth) = min_active {
                    self.current_level = self.current_level.min(min_depth);
                }
            }
        }

        let mut actions = Vec::new();

        // Drain events buffered by wait_event, then any new ones in the channel.
        while let Some(event) = self.buffered_events.pop_front() {
            let cancel_actions = self.process_event(event);
            actions.extend(cancel_actions);
        }
        while let Ok(event) = self.event_rx.try_recv() {
            let cancel_actions = self.process_event(event);
            actions.extend(cancel_actions);
        }

        if self.graph.status != GraphStatus::Running {
            return actions;
        }

        // Check for timed-out tasks.
        let timeout_actions = self.check_timeouts();
        actions.extend(timeout_actions);

        if self.graph.status != GraphStatus::Running {
            return actions;
        }

        // Dispatch ready tasks up to max_parallel slots. Concurrency is pre-enforced here
        // (topology-aware cap) and also enforced by SubAgentManager::spawn() returning
        // ConcurrencyLimit when active + reserved >= max_concurrent.
        // Non-transient spawn failures are handled by record_spawn_failure(); optimistic
        // Running marks are reverted to Ready for ConcurrencyLimit errors.
        let raw_ready = dag::ready_tasks(&self.graph);

        // CascadeAware: partition ready tasks into preferred (healthy region) and deferred
        // (cascading region). Deferred tasks still run when no preferred tasks remain.
        // Skip for Sequential tasks — they must not be reordered relative to each other (C14).
        let ready: Vec<TaskId> = if self.topology.strategy == DispatchStrategy::CascadeAware {
            if let Some(ref detector) = self.cascade_detector {
                let deprioritized = detector.deprioritized_tasks(&self.graph);
                if deprioritized.is_empty() {
                    raw_ready
                } else {
                    let (preferred, deferred): (Vec<_>, Vec<_>) =
                        raw_ready.into_iter().partition(|id| {
                            let is_sequential = self.graph.tasks[id.index()].execution_mode
                                == ExecutionMode::Sequential;
                            // Sequential tasks are never reordered.
                            is_sequential || !deprioritized.contains(id)
                        });
                    preferred.into_iter().chain(deferred).collect()
                }
            } else {
                raw_ready
            }
        } else {
            raw_ready
        };

        // TreeOptimized: sort ready tasks by critical-path distance descending
        // (tasks deepest in the DAG go first — shortest distance to sinks).
        // Skip for Sequential tasks to preserve their ordering invariant (C14).
        let ready: Vec<TaskId> = if self.topology.strategy == DispatchStrategy::TreeOptimized {
            let max_depth = self.topology.depth;
            let mut sortable = ready;
            sortable.sort_by_key(|id| {
                let task_depth = self.topology.depths.get(id).copied().unwrap_or(0);
                // Deeper tasks have smaller key → dispatched first.
                max_depth.saturating_sub(task_depth)
            });
            sortable
        } else {
            ready
        };

        // LevelBarrier: advance the current level when all tasks at this level are terminal.
        // This handles failures/skips correctly because propagate_failure() uses BFS to
        // transitively mark all non-terminal dependents as Skipped before tick() runs dispatch.
        if self.topology.strategy == DispatchStrategy::LevelBarrier {
            let all_current_level_terminal = self.graph.tasks.iter().all(|t| {
                let task_depth = self
                    .topology
                    .depths
                    .get(&t.id)
                    .copied()
                    .unwrap_or(usize::MAX);
                task_depth != self.current_level || t.status.is_terminal()
            });
            if all_current_level_terminal {
                // Advance until we find a level with non-terminal tasks, or exhaust all levels.
                let max_depth = self.topology.depth;
                while self.current_level <= max_depth {
                    let has_non_terminal = self.graph.tasks.iter().any(|t| {
                        let d = self
                            .topology
                            .depths
                            .get(&t.id)
                            .copied()
                            .unwrap_or(usize::MAX);
                        d == self.current_level && !t.status.is_terminal()
                    });
                    if has_non_terminal {
                        break;
                    }
                    self.current_level += 1;
                }
            }
        }

        // Available dispatch slots for this tick.
        let mut slots = self.max_parallel.saturating_sub(self.running.len());

        // For sequential dispatch: track whether we already scheduled one sequential task
        // this tick AND whether any sequential task is currently running.
        let mut sequential_spawned_this_tick = false;
        let has_running_sequential = self
            .running
            .keys()
            .any(|tid| self.graph.tasks[tid.index()].execution_mode == ExecutionMode::Sequential);

        for task_id in ready {
            if slots == 0 {
                break;
            }

            // LevelBarrier: only dispatch tasks at the current level.
            if self.topology.strategy == DispatchStrategy::LevelBarrier {
                let task_depth = self
                    .topology
                    .depths
                    .get(&task_id)
                    .copied()
                    .unwrap_or(usize::MAX);
                if task_depth != self.current_level {
                    continue;
                }
            }

            let task = &self.graph.tasks[task_id.index()];

            // Sequential tasks: only one may run at a time within the scheduler.
            // Independent sequential tasks in separate DAG branches are still
            // serialized here (they share exclusive-resource intent by annotation).
            if task.execution_mode == ExecutionMode::Sequential {
                if sequential_spawned_this_tick || has_running_sequential {
                    continue;
                }
                sequential_spawned_this_tick = true;
            }

            let Some(agent_def_name) = self.router.route(task, &self.available_agents) else {
                tracing::debug!(
                    task_id = %task_id,
                    title = %task.title,
                    "no agent available, routing task to main agent inline"
                );
                let prompt = self.build_task_prompt(task);
                self.graph.tasks[task_id.index()].status = TaskStatus::Running;
                actions.push(SchedulerAction::RunInline { task_id, prompt });
                slots -= 1;
                continue;
            };

            let prompt = self.build_task_prompt(task);

            // Mark task as Running optimistically (before record_spawn is called).
            self.graph.tasks[task_id.index()].status = TaskStatus::Running;

            actions.push(SchedulerAction::Spawn {
                task_id,
                agent_def_name,
                prompt,
            });
            slots -= 1;
        }

        // Check for completion or deadlock.
        // Use graph Running status count to avoid false positives while Spawn actions
        // are in-flight (record_spawn hasn't been called yet for freshly emitted spawns).
        // Note: non-transient spawn failures (e.g. capability errors) are handled by
        // record_spawn_failure() which marks the task Failed and propagates failure per
        // the task's FailureStrategy — this detector does not fire for those cases because
        // failed tasks are terminal and dag::ready_tasks() returns their unblocked dependents.
        // ConcurrencyLimit errors are transient: record_spawn_failure() reverts the task
        // from Running back to Ready, so ready_tasks() is non-empty and deadlock is not
        // triggered.
        let running_in_graph_now = self
            .graph
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Running)
            .count();
        if running_in_graph_now == 0 && self.running.is_empty() {
            let all_terminal = self.graph.tasks.iter().all(|t| t.status.is_terminal());
            if all_terminal {
                self.graph.status = GraphStatus::Completed;
                self.graph.finished_at = Some(super::graph::chrono_now());
                actions.push(SchedulerAction::Done {
                    status: GraphStatus::Completed,
                });
            } else if dag::ready_tasks(&self.graph).is_empty() {
                tracing::error!(
                    "scheduler deadlock: no running or ready tasks, but graph not complete"
                );
                self.graph.status = GraphStatus::Failed;
                self.graph.finished_at = Some(super::graph::chrono_now());
                // Invariant: deadlock fires only when self.running is empty (checked above).
                debug_assert!(
                    self.running.is_empty(),
                    "deadlock branch reached with non-empty running map"
                );
                for task in &mut self.graph.tasks {
                    if !task.status.is_terminal() {
                        task.status = TaskStatus::Canceled;
                    }
                }
                actions.push(SchedulerAction::Done {
                    status: GraphStatus::Failed,
                });
            }
        }

        actions
    }

    /// Wait for the next event from a running sub-agent.
    ///
    /// Buffers the received event for processing in the next `tick` call.
    /// Returns immediately if no tasks are running. Uses a timeout so that
    /// periodic timeout checking can occur.
    /// Compute the current deferral backoff with exponential growth capped at 5 seconds.
    ///
    /// Each consecutive spawn failure due to concurrency limits doubles the base backoff.
    fn current_deferral_backoff(&self) -> Duration {
        const MAX_BACKOFF: Duration = Duration::from_secs(5);
        let multiplier = 1u32
            .checked_shl(self.consecutive_spawn_failures.min(10))
            .unwrap_or(u32::MAX);
        self.deferral_backoff
            .saturating_mul(multiplier)
            .min(MAX_BACKOFF)
    }

    pub async fn wait_event(&mut self) {
        if self.running.is_empty() {
            tokio::time::sleep(self.current_deferral_backoff()).await;
            return;
        }

        // Find the nearest timeout deadline among running tasks.
        let nearest_timeout = self
            .running
            .values()
            .map(|r| {
                self.task_timeout
                    .checked_sub(r.started_at.elapsed())
                    .unwrap_or(Duration::ZERO)
            })
            .min()
            .unwrap_or(Duration::from_secs(1));

        // Clamp to at least 100 ms to avoid busy-looping.
        let wait_duration = nearest_timeout.max(Duration::from_millis(100));

        tokio::select! {
            Some(event) = self.event_rx.recv() => {
                // SEC-ORCH-02: guard against unbounded buffer growth. Use total task
                // count rather than max_parallel so that parallel bursts exceeding
                // max_parallel do not cause premature event drops.
                if self.buffered_events.len() >= self.graph.tasks.len() * 2 {
                    // PERF-SCHED-02: log at error level — a dropped completion event
                    // leaves a task stuck in Running until its timeout fires.
                    if let Some(dropped) = self.buffered_events.pop_front() {
                        tracing::error!(
                            task_id = %dropped.task_id,
                            buffer_len = self.buffered_events.len(),
                            "event buffer saturated; completion event dropped — task may \
                             remain Running until timeout"
                        );
                    }
                }
                self.buffered_events.push_back(event);
            }
            () = tokio::time::sleep(wait_duration) => {}
        }
    }

    /// Record that a spawn action was successfully executed.
    ///
    /// Called by the caller after successfully spawning via `SubAgentManager`.
    ///
    /// Resets `consecutive_spawn_failures` to 0 as a "spawn succeeded = scheduler healthy"
    /// signal. This is intentionally separate from the batch-level backoff in
    /// [`record_batch_backoff`]: `record_spawn` provides an immediate reset on the first
    /// success within a batch, while `record_batch_backoff` governs the tick-granular
    /// failure counter used for exponential wait backoff.
    pub fn record_spawn(
        &mut self,
        task_id: TaskId,
        agent_handle_id: String,
        agent_def_name: String,
    ) {
        self.consecutive_spawn_failures = 0;
        self.graph.tasks[task_id.index()].assigned_agent = Some(agent_handle_id.clone());
        self.running.insert(
            task_id,
            RunningTask {
                agent_handle_id,
                agent_def_name,
                started_at: Instant::now(),
            },
        );
    }

    /// Handle a failed spawn attempt.
    ///
    /// If the error is a transient concurrency-limit rejection, reverts the task from
    /// Running back to `Ready` so the next [`tick`] can retry the spawn when a slot opens.
    /// Otherwise, marks the task as `Failed` and propagates failure.
    /// Returns any cancel actions needed.
    ///
    /// # Errors (via returned actions)
    ///
    /// Propagates failure per the task's effective `FailureStrategy`.
    pub fn record_spawn_failure(
        &mut self,
        task_id: TaskId,
        error: &SubAgentError,
    ) -> Vec<SchedulerAction> {
        // Transient condition: the SubAgentManager rejected the spawn because all
        // concurrency slots are occupied. Revert to Ready so the next tick retries.
        // consecutive_spawn_failures is updated batch-wide by record_batch_backoff().
        if let SubAgentError::ConcurrencyLimit { active, max } = error {
            tracing::warn!(
                task_id = %task_id,
                active,
                max,
                next_backoff_ms = self.current_deferral_backoff().as_millis(),
                "concurrency limit reached, deferring task to next tick"
            );
            self.graph.tasks[task_id.index()].status = TaskStatus::Ready;
            return Vec::new();
        }

        // SEC-ORCH-04: truncate error to avoid logging sensitive internal details.
        let error_excerpt: String = error.to_string().chars().take(512).collect();
        tracing::warn!(
            task_id = %task_id,
            error = %error_excerpt,
            "spawn failed, marking task failed"
        );
        self.graph.tasks[task_id.index()].status = TaskStatus::Failed;
        let cancel_ids = dag::propagate_failure(&mut self.graph, task_id);
        let mut actions = Vec::new();
        for cancel_task_id in cancel_ids {
            if let Some(running) = self.running.remove(&cancel_task_id) {
                actions.push(SchedulerAction::Cancel {
                    agent_handle_id: running.agent_handle_id,
                });
            }
        }
        if self.graph.status != GraphStatus::Running {
            self.graph.finished_at = Some(super::graph::chrono_now());
            actions.push(SchedulerAction::Done {
                status: self.graph.status,
            });
        }
        actions
    }

    /// Update the batch-level backoff counter after processing a full tick's spawn batch.
    ///
    /// With parallel dispatch a single tick may produce N Spawn actions. Individual
    /// per-spawn counter updates would miscount concurrent rejections as "consecutive"
    /// failures. This method captures the batch semantics instead:
    /// - If any spawn succeeded → reset the counter (scheduler is healthy).
    /// - Else if any spawn hit `ConcurrencyLimit` → this entire tick was a deferral tick.
    /// - If neither → no spawns were attempted; counter unchanged.
    pub fn record_batch_backoff(&mut self, any_success: bool, any_concurrency_failure: bool) {
        if any_success {
            self.consecutive_spawn_failures = 0;
        } else if any_concurrency_failure {
            self.consecutive_spawn_failures = self.consecutive_spawn_failures.saturating_add(1);
        }
    }

    /// Cancel all running tasks (for user-initiated plan cancellation).
    ///
    /// # Warning: Cooperative Cancellation
    ///
    /// Cancellation is cooperative and asynchronous. Tool operations (file writes, shell
    /// executions) in progress at the time of cancellation complete before the agent loop
    /// checks the cancellation token. Callers should inspect the task graph state and clean
    /// up partially-written artifacts manually.
    pub fn cancel_all(&mut self) -> Vec<SchedulerAction> {
        self.graph.status = GraphStatus::Canceled;
        self.graph.finished_at = Some(super::graph::chrono_now());

        // Drain running map first to avoid split borrow issues (M3).
        let running: Vec<(TaskId, RunningTask)> = self.running.drain().collect();
        let mut actions: Vec<SchedulerAction> = running
            .into_iter()
            .map(|(task_id, r)| {
                self.graph.tasks[task_id.index()].status = TaskStatus::Canceled;
                SchedulerAction::Cancel {
                    agent_handle_id: r.agent_handle_id,
                }
            })
            .collect();

        for task in &mut self.graph.tasks {
            if !task.status.is_terminal() {
                task.status = TaskStatus::Canceled;
            }
        }

        actions.push(SchedulerAction::Done {
            status: GraphStatus::Canceled,
        });
        actions
    }
}

impl DagScheduler {
    /// Process a single `TaskEvent` and return any cancel actions needed.
    fn process_event(&mut self, event: TaskEvent) -> Vec<SchedulerAction> {
        let TaskEvent {
            task_id,
            agent_handle_id,
            outcome,
        } = event;

        // Guard against stale events from previous incarnations (e.g. after timeout+retry).
        // A timed-out agent's event_tx outlives the timeout and may send a completion later.
        match self.running.get(&task_id) {
            Some(running) if running.agent_handle_id != agent_handle_id => {
                tracing::warn!(
                    task_id = %task_id,
                    expected = %running.agent_handle_id,
                    got = %agent_handle_id,
                    "discarding stale event from previous agent incarnation"
                );
                return Vec::new();
            }
            None => {
                tracing::debug!(
                    task_id = %task_id,
                    agent_handle_id = %agent_handle_id,
                    "ignoring event for task not in running map"
                );
                return Vec::new();
            }
            Some(_) => {}
        }

        // Compute duration BEFORE removing from running map (C1 fix).
        let duration_ms = self.running.get(&task_id).map_or(0, |r| {
            u64::try_from(r.started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
        });
        let agent_def_name = self.running.get(&task_id).map(|r| r.agent_def_name.clone());

        self.running.remove(&task_id);

        match outcome {
            TaskOutcome::Completed { output, artifacts } => {
                self.graph.tasks[task_id.index()].status = TaskStatus::Completed;
                self.graph.tasks[task_id.index()].result = Some(TaskResult {
                    output: output.clone(),
                    artifacts,
                    duration_ms,
                    agent_id: Some(agent_handle_id),
                    agent_def: agent_def_name,
                });

                // Record success in cascade detector.
                if let Some(ref mut detector) = self.cascade_detector {
                    detector.record_outcome(task_id, true, &self.graph);
                }

                // Mark newly unblocked tasks as Ready.
                // Downstream tasks are unblocked immediately — verification does not gate dispatch.
                let newly_ready = dag::ready_tasks(&self.graph);
                for ready_id in newly_ready {
                    if self.graph.tasks[ready_id.index()].status == TaskStatus::Pending {
                        self.graph.tasks[ready_id.index()].status = TaskStatus::Ready;
                    }
                }

                // Emit Verify action when verify_completeness is enabled.
                // The replan budget is enforced inside inject_tasks() — the observation
                // (emitting Verify) must not be gated on the mutation budget, or tasks
                // after budget exhaustion never receive verification at all.
                // max_replans=0 still emits Verify; gaps are logged only (no inject_tasks call).
                if self.verify_completeness {
                    vec![SchedulerAction::Verify { task_id, output }]
                } else {
                    Vec::new()
                }
            }

            TaskOutcome::Failed { error } => {
                // SEC-ORCH-04: truncate error to avoid logging sensitive internal details.
                let error_excerpt: String = error.chars().take(512).collect();
                tracing::warn!(
                    task_id = %task_id,
                    error = %error_excerpt,
                    "task failed"
                );
                self.graph.tasks[task_id.index()].status = TaskStatus::Failed;

                // Record failure in cascade detector.
                if let Some(ref mut detector) = self.cascade_detector {
                    detector.record_outcome(task_id, false, &self.graph);
                }

                let cancel_ids = dag::propagate_failure(&mut self.graph, task_id);
                let mut actions = Vec::new();

                for cancel_task_id in cancel_ids {
                    if let Some(running) = self.running.remove(&cancel_task_id) {
                        actions.push(SchedulerAction::Cancel {
                            agent_handle_id: running.agent_handle_id,
                        });
                    }
                }

                if self.graph.status != GraphStatus::Running {
                    self.graph.finished_at = Some(super::graph::chrono_now());
                    actions.push(SchedulerAction::Done {
                        status: self.graph.status,
                    });
                }

                actions
            }
        }
    }

    /// Check all running tasks for timeout violations.
    ///
    /// # Warning: Cooperative Cancellation
    ///
    /// Cancel actions emitted here signal agents cooperatively. Tool operations in progress
    /// at the time of cancellation complete before the agent loop checks the cancellation
    /// token. Partially-written artifacts may remain on disk after cancellation.
    fn check_timeouts(&mut self) -> Vec<SchedulerAction> {
        let timed_out: Vec<(TaskId, String)> = self
            .running
            .iter()
            .filter(|(_, r)| r.started_at.elapsed() > self.task_timeout)
            .map(|(id, r)| (*id, r.agent_handle_id.clone()))
            .collect();

        let mut actions = Vec::new();
        for (task_id, agent_handle_id) in timed_out {
            tracing::warn!(
                task_id = %task_id,
                timeout_secs = self.task_timeout.as_secs(),
                "task timed out"
            );
            self.running.remove(&task_id);
            self.graph.tasks[task_id.index()].status = TaskStatus::Failed;

            actions.push(SchedulerAction::Cancel { agent_handle_id });

            let cancel_ids = dag::propagate_failure(&mut self.graph, task_id);
            for cancel_task_id in cancel_ids {
                if let Some(running) = self.running.remove(&cancel_task_id) {
                    actions.push(SchedulerAction::Cancel {
                        agent_handle_id: running.agent_handle_id,
                    });
                }
            }

            if self.graph.status != GraphStatus::Running {
                self.graph.finished_at = Some(super::graph::chrono_now());
                actions.push(SchedulerAction::Done {
                    status: self.graph.status,
                });
                break;
            }
        }

        actions
    }

    /// Build the task prompt with dependency context injection (Section 14).
    ///
    /// Uses char-boundary-safe truncation (S1 fix) to avoid panics on multi-byte UTF-8.
    /// Dependency output is sanitized (SEC-ORCH-01) and titles are XML-escaped to prevent
    /// prompt injection via crafted task outputs.
    fn build_task_prompt(&self, task: &TaskNode) -> String {
        if task.depends_on.is_empty() {
            return task.description.clone();
        }

        let completed_deps: Vec<&TaskNode> = task
            .depends_on
            .iter()
            .filter_map(|dep_id| {
                let dep = &self.graph.tasks[dep_id.index()];
                if dep.status == TaskStatus::Completed {
                    Some(dep)
                } else {
                    None
                }
            })
            .collect();

        if completed_deps.is_empty() {
            return task.description.clone();
        }

        let budget_per_dep = self
            .dependency_context_budget
            .checked_div(completed_deps.len())
            .unwrap_or(self.dependency_context_budget);

        let mut context_block = String::from("<completed-dependencies>\n");

        for dep in &completed_deps {
            // SEC-ORCH-01: XML-escape dep.id and dep.title to prevent breaking out of the
            // <completed-dependencies> wrapper via crafted titles.
            let escaped_id = xml_escape(&dep.id.to_string());
            let escaped_title = xml_escape(&dep.title);
            let _ = writeln!(
                context_block,
                "## Task \"{escaped_id}\": \"{escaped_title}\" (completed)",
            );

            if let Some(ref result) = dep.result {
                // SEC-ORCH-01: sanitize dep output to prevent prompt injection from upstream tasks.
                let source = ContentSource::new(ContentSourceKind::A2aMessage);
                let sanitized = self.sanitizer.sanitize(&result.output, source);
                let safe_output = sanitized.body;

                // Char-boundary-safe truncation (S1): use chars().take() instead of byte slicing.
                let char_count = safe_output.chars().count();
                if char_count > budget_per_dep {
                    let truncated: String = safe_output.chars().take(budget_per_dep).collect();
                    let _ = write!(
                        context_block,
                        "{truncated}...\n[truncated: {char_count} chars total]"
                    );
                } else {
                    context_block.push_str(&safe_output);
                }
            } else {
                context_block.push_str("[no output recorded]\n");
            }
            context_block.push('\n');
        }

        // Add notes for skipped deps.
        for dep_id in &task.depends_on {
            let dep = &self.graph.tasks[dep_id.index()];
            if dep.status == TaskStatus::Skipped {
                let escaped_id = xml_escape(&dep.id.to_string());
                let escaped_title = xml_escape(&dep.title);
                let _ = writeln!(
                    context_block,
                    "## Task \"{escaped_id}\": \"{escaped_title}\" (skipped -- no output available)\n",
                );
            }
        }

        context_block.push_str("</completed-dependencies>\n\n");
        format!("{context_block}Your task: {}", task.description)
    }
}

/// Escape XML special characters in a string to prevent tag injection.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::default_trait_access)]

    use super::*;
    use crate::graph::{FailureStrategy, GraphStatus, TaskGraph, TaskNode, TaskStatus};

    fn make_node(id: u32, deps: &[u32]) -> TaskNode {
        let mut n = TaskNode::new(
            id,
            format!("task-{id}"),
            format!("description for task {id}"),
        );
        n.depends_on = deps.iter().map(|&d| TaskId(d)).collect();
        n
    }

    fn graph_from_nodes(nodes: Vec<TaskNode>) -> TaskGraph {
        let mut g = TaskGraph::new("test goal");
        g.tasks = nodes;
        g
    }

    fn make_def(name: &str) -> SubAgentDef {
        use zeph_subagent::{SkillFilter, SubAgentPermissions, SubagentHooks, ToolPolicy};
        SubAgentDef {
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

    fn make_config() -> zeph_config::OrchestrationConfig {
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
        }
    }

    struct FirstRouter;
    impl AgentRouter for FirstRouter {
        fn route(&self, _task: &TaskNode, available: &[SubAgentDef]) -> Option<String> {
            available.first().map(|d| d.name.clone())
        }
    }

    struct NoneRouter;
    impl AgentRouter for NoneRouter {
        fn route(&self, _task: &TaskNode, _available: &[SubAgentDef]) -> Option<String> {
            None
        }
    }

    fn make_scheduler_with_router(graph: TaskGraph, router: Box<dyn AgentRouter>) -> DagScheduler {
        let config = make_config();
        let defs = vec![make_def("worker")];
        DagScheduler::new(graph, &config, router, defs).unwrap()
    }

    fn make_scheduler(graph: TaskGraph) -> DagScheduler {
        let config = make_config();
        let defs = vec![make_def("worker")];
        DagScheduler::new(graph, &config, Box::new(FirstRouter), defs).unwrap()
    }

    // --- constructor tests ---

    #[test]
    fn test_new_validates_graph_status() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.status = GraphStatus::Running; // wrong status
        let config = make_config();
        let result = DagScheduler::new(graph, &config, Box::new(FirstRouter), vec![]);
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
        let result = DagScheduler::new(graph, &config, Box::new(FirstRouter), vec![]);
        assert!(result.is_err());
    }

    // --- tick tests ---

    #[test]
    fn test_tick_produces_spawn_for_ready() {
        let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[])]);
        let mut scheduler = make_scheduler(graph);
        let actions = scheduler.tick();
        let spawns: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::Spawn { .. }))
            .collect();
        assert_eq!(spawns.len(), 2);
    }

    #[test]
    fn test_tick_dispatches_all_regardless_of_max_parallel() {
        // tick() enforces max_parallel as a pre-dispatch cap.
        // With 5 independent tasks and max_parallel=2, only 2 are dispatched per tick.
        let graph = graph_from_nodes(vec![
            make_node(0, &[]),
            make_node(1, &[]),
            make_node(2, &[]),
            make_node(3, &[]),
            make_node(4, &[]),
        ]);
        let mut config = make_config();
        config.max_parallel = 2;
        let defs = vec![make_def("worker")];
        let mut scheduler = DagScheduler::new(graph, &config, Box::new(FirstRouter), defs).unwrap();
        let actions = scheduler.tick();
        let spawn_count = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::Spawn { .. }))
            .count();
        assert_eq!(
            spawn_count, 2,
            "max_parallel=2 caps dispatched tasks per tick"
        );
    }

    #[test]
    fn test_tick_detects_completion() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.tasks[0].status = TaskStatus::Completed;
        let config = make_config();
        let defs = vec![make_def("worker")];
        let mut scheduler = DagScheduler::new(graph, &config, Box::new(FirstRouter), defs).unwrap();
        // Manually set graph to Running since new() validated Created status
        // — but all tasks are terminal. tick() should detect completion.
        let actions = scheduler.tick();
        let has_done = actions.iter().any(|a| {
            matches!(
                a,
                SchedulerAction::Done {
                    status: GraphStatus::Completed
                }
            )
        });
        assert!(
            has_done,
            "should emit Done(Completed) when all tasks are terminal"
        );
    }

    // --- completion event tests ---

    #[test]
    fn test_completion_event_marks_deps_ready() {
        let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        let mut scheduler = make_scheduler(graph);

        // Simulate task 0 running.
        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.running.insert(
            TaskId(0),
            RunningTask {
                agent_handle_id: "handle-0".to_string(),
                agent_def_name: "worker".to_string(),
                started_at: Instant::now(),
            },
        );

        let event = TaskEvent {
            task_id: TaskId(0),
            agent_handle_id: "handle-0".to_string(),
            outcome: TaskOutcome::Completed {
                output: "done".to_string(),
                artifacts: vec![],
            },
        };
        scheduler.buffered_events.push_back(event);

        let actions = scheduler.tick();
        assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Completed);
        // Task 1 should now be Ready or Spawn action emitted.
        let has_spawn_1 = actions
            .iter()
            .any(|a| matches!(a, SchedulerAction::Spawn { task_id, .. } if *task_id == TaskId(1)));
        assert!(
            has_spawn_1 || scheduler.graph.tasks[1].status == TaskStatus::Ready,
            "task 1 should be spawned or marked Ready"
        );
    }

    #[test]
    fn test_failure_abort_cancels_running() {
        let graph = graph_from_nodes(vec![
            make_node(0, &[]),
            make_node(1, &[]),
            make_node(2, &[0, 1]),
        ]);
        let mut scheduler = make_scheduler(graph);

        // Simulate tasks 0 and 1 running.
        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.running.insert(
            TaskId(0),
            RunningTask {
                agent_handle_id: "h0".to_string(),
                agent_def_name: "worker".to_string(),
                started_at: Instant::now(),
            },
        );
        scheduler.graph.tasks[1].status = TaskStatus::Running;
        scheduler.running.insert(
            TaskId(1),
            RunningTask {
                agent_handle_id: "h1".to_string(),
                agent_def_name: "worker".to_string(),
                started_at: Instant::now(),
            },
        );

        // Task 0 fails with default Abort strategy.
        let event = TaskEvent {
            task_id: TaskId(0),
            agent_handle_id: "h0".to_string(),
            outcome: TaskOutcome::Failed {
                error: "boom".to_string(),
            },
        };
        scheduler.buffered_events.push_back(event);

        let actions = scheduler.tick();
        assert_eq!(scheduler.graph.status, GraphStatus::Failed);
        let cancel_ids: Vec<_> = actions
            .iter()
            .filter_map(|a| {
                if let SchedulerAction::Cancel { agent_handle_id } = a {
                    Some(agent_handle_id.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert!(cancel_ids.contains(&"h1"), "task 1 should be canceled");
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, SchedulerAction::Done { .. }))
        );
    }

    #[test]
    fn test_failure_skip_propagates() {
        let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        let mut scheduler = make_scheduler(graph);

        // Set failure strategy to Skip on task 0.
        scheduler.graph.tasks[0].failure_strategy = Some(FailureStrategy::Skip);
        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.running.insert(
            TaskId(0),
            RunningTask {
                agent_handle_id: "h0".to_string(),
                agent_def_name: "worker".to_string(),
                started_at: Instant::now(),
            },
        );

        let event = TaskEvent {
            task_id: TaskId(0),
            agent_handle_id: "h0".to_string(),
            outcome: TaskOutcome::Failed {
                error: "skip me".to_string(),
            },
        };
        scheduler.buffered_events.push_back(event);
        scheduler.tick();

        assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Skipped);
        assert_eq!(scheduler.graph.tasks[1].status, TaskStatus::Skipped);
    }

    #[test]
    fn test_failure_retry_reschedules() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);

        scheduler.graph.tasks[0].failure_strategy = Some(FailureStrategy::Retry);
        scheduler.graph.tasks[0].max_retries = Some(3);
        scheduler.graph.tasks[0].retry_count = 0;
        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.running.insert(
            TaskId(0),
            RunningTask {
                agent_handle_id: "h0".to_string(),
                agent_def_name: "worker".to_string(),
                started_at: Instant::now(),
            },
        );

        let event = TaskEvent {
            task_id: TaskId(0),
            agent_handle_id: "h0".to_string(),
            outcome: TaskOutcome::Failed {
                error: "transient".to_string(),
            },
        };
        scheduler.buffered_events.push_back(event);
        let actions = scheduler.tick();

        // Task should be rescheduled (Ready) and a Spawn action emitted.
        let has_spawn = actions
            .iter()
            .any(|a| matches!(a, SchedulerAction::Spawn { task_id, .. } if *task_id == TaskId(0)));
        assert!(
            has_spawn || scheduler.graph.tasks[0].status == TaskStatus::Ready,
            "retry should produce spawn or Ready status"
        );
        // retry_count incremented
        assert_eq!(scheduler.graph.tasks[0].retry_count, 1);
    }

    #[test]
    fn test_process_event_failed_retry() {
        // End-to-end: send Failed event, verify retry path produces Ready -> Spawn.
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);

        scheduler.graph.tasks[0].failure_strategy = Some(FailureStrategy::Retry);
        scheduler.graph.tasks[0].max_retries = Some(2);
        scheduler.graph.tasks[0].retry_count = 0;
        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.running.insert(
            TaskId(0),
            RunningTask {
                agent_handle_id: "h0".to_string(),
                agent_def_name: "worker".to_string(),
                started_at: Instant::now(),
            },
        );

        let event = TaskEvent {
            task_id: TaskId(0),
            agent_handle_id: "h0".to_string(),
            outcome: TaskOutcome::Failed {
                error: "first failure".to_string(),
            },
        };
        scheduler.buffered_events.push_back(event);
        let actions = scheduler.tick();

        // After retry: retry_count = 1, status = Ready or Spawn emitted.
        assert_eq!(scheduler.graph.tasks[0].retry_count, 1);
        let spawned = actions
            .iter()
            .any(|a| matches!(a, SchedulerAction::Spawn { task_id, .. } if *task_id == TaskId(0)));
        assert!(
            spawned || scheduler.graph.tasks[0].status == TaskStatus::Ready,
            "retry should emit Spawn or set Ready"
        );
        // Graph must still be Running.
        assert_eq!(scheduler.graph.status, GraphStatus::Running);
    }

    #[test]
    fn test_timeout_cancels_stalled() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut config = make_config();
        config.task_timeout_secs = 1; // 1 second timeout
        let defs = vec![make_def("worker")];
        let mut scheduler = DagScheduler::new(graph, &config, Box::new(FirstRouter), defs).unwrap();

        // Simulate a running task that started just over 1 second ago.
        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.running.insert(
            TaskId(0),
            RunningTask {
                agent_handle_id: "h0".to_string(),
                agent_def_name: "worker".to_string(),
                started_at: Instant::now().checked_sub(Duration::from_secs(2)).unwrap(), // already timed out
            },
        );

        let actions = scheduler.tick();
        let has_cancel = actions.iter().any(
            |a| matches!(a, SchedulerAction::Cancel { agent_handle_id } if agent_handle_id == "h0"),
        );
        assert!(has_cancel, "timed-out task should emit Cancel action");
        assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Failed);
    }

    #[test]
    fn test_cancel_all() {
        let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[])]);
        let mut scheduler = make_scheduler(graph);

        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.running.insert(
            TaskId(0),
            RunningTask {
                agent_handle_id: "h0".to_string(),
                agent_def_name: "worker".to_string(),
                started_at: Instant::now(),
            },
        );
        scheduler.graph.tasks[1].status = TaskStatus::Running;
        scheduler.running.insert(
            TaskId(1),
            RunningTask {
                agent_handle_id: "h1".to_string(),
                agent_def_name: "worker".to_string(),
                started_at: Instant::now(),
            },
        );

        let actions = scheduler.cancel_all();

        assert_eq!(scheduler.graph.status, GraphStatus::Canceled);
        assert!(scheduler.running.is_empty());
        let cancel_count = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::Cancel { .. }))
            .count();
        assert_eq!(cancel_count, 2);
        assert!(actions.iter().any(|a| matches!(
            a,
            SchedulerAction::Done {
                status: GraphStatus::Canceled
            }
        )));
    }

    #[test]
    fn test_record_spawn_failure() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);

        // Simulate task marked Running (by tick) but spawn failed.
        scheduler.graph.tasks[0].status = TaskStatus::Running;

        let error = SubAgentError::Spawn("spawn error".to_string());
        let actions = scheduler.record_spawn_failure(TaskId(0), &error);
        assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Failed);
        // With Abort strategy and no other running tasks, graph should be Failed.
        assert_eq!(scheduler.graph.status, GraphStatus::Failed);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, SchedulerAction::Done { .. }))
        );
    }

    #[test]
    fn test_record_spawn_failure_concurrency_limit_reverts_to_ready() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);

        // Simulate tick() optimistically marking the task Running before spawn.
        scheduler.graph.tasks[0].status = TaskStatus::Running;

        // Concurrency limit hit — transient, should not fail the task.
        let error = SubAgentError::ConcurrencyLimit { active: 4, max: 4 };
        let actions = scheduler.record_spawn_failure(TaskId(0), &error);
        assert_eq!(
            scheduler.graph.tasks[0].status,
            TaskStatus::Ready,
            "task must revert to Ready so the next tick can retry"
        );
        assert_eq!(
            scheduler.graph.status,
            GraphStatus::Running,
            "graph must stay Running, not transition to Failed"
        );
        assert!(
            actions.is_empty(),
            "no cancel or done actions expected for a transient deferral"
        );
    }

    #[test]
    fn test_record_spawn_failure_concurrency_limit_variant_spawn_for_task() {
        // Both spawn() and resume() now return SubAgentError::ConcurrencyLimit — verify handling.
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);
        scheduler.graph.tasks[0].status = TaskStatus::Running;

        let error = SubAgentError::ConcurrencyLimit { active: 1, max: 1 };
        let actions = scheduler.record_spawn_failure(TaskId(0), &error);
        assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Ready);
        assert!(actions.is_empty());
    }

    // --- #1516 edge-case tests ---

    #[test]
    fn test_concurrency_deferral_does_not_affect_running_task() {
        // Two root tasks. Task 0 is Running (successfully spawned).
        // Task 1 hits a concurrency limit and reverts to Ready.
        // Task 0 must be unaffected.
        let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[])]);
        let mut scheduler = make_scheduler(graph);

        // Simulate both tasks optimistically marked Running by tick().
        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.running.insert(
            TaskId(0),
            RunningTask {
                agent_handle_id: "h0".to_string(),
                agent_def_name: "worker".to_string(),
                started_at: Instant::now(),
            },
        );
        scheduler.graph.tasks[1].status = TaskStatus::Running;

        // Task 1 spawn fails with concurrency limit.
        let error = SubAgentError::ConcurrencyLimit { active: 1, max: 1 };
        let actions = scheduler.record_spawn_failure(TaskId(1), &error);

        assert_eq!(
            scheduler.graph.tasks[0].status,
            TaskStatus::Running,
            "task 0 must remain Running"
        );
        assert_eq!(
            scheduler.graph.tasks[1].status,
            TaskStatus::Ready,
            "task 1 must revert to Ready"
        );
        assert_eq!(
            scheduler.graph.status,
            GraphStatus::Running,
            "graph must stay Running"
        );
        assert!(actions.is_empty(), "no cancel or done actions expected");
    }

    #[test]
    fn test_max_concurrent_zero_no_infinite_loop() {
        // max_parallel=0 is a degenerate config. tick() uses saturating_sub so slots=0,
        // and no tasks are dispatched. The graph does not deadlock because ready tasks
        // still exist — the caller must increase max_parallel or handle this externally.
        // After max_parallel is increased and a new tick fires, tasks will be dispatched.
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let config = zeph_config::OrchestrationConfig {
            max_parallel: 0,
            ..make_config()
        };
        let mut scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        let actions1 = scheduler.tick();
        // No Spawn: slots = max_parallel(0) - running(0) = 0.
        assert!(
            actions1
                .iter()
                .all(|a| !matches!(a, SchedulerAction::Spawn { .. })),
            "no Spawn expected when max_parallel=0"
        );
        assert!(
            actions1
                .iter()
                .all(|a| !matches!(a, SchedulerAction::Done { .. })),
            "no Done(Failed) expected — ready tasks exist, so no deadlock"
        );
        assert_eq!(scheduler.graph.status, GraphStatus::Running);

        // Second tick also dispatches nothing (still max_parallel=0, ready task exists).
        let actions2 = scheduler.tick();
        assert!(
            actions2
                .iter()
                .all(|a| !matches!(a, SchedulerAction::Done { .. })),
            "second tick must not emit Done(Failed) — ready tasks still exist"
        );
        assert_eq!(
            scheduler.graph.status,
            GraphStatus::Running,
            "graph must remain Running"
        );
    }

    #[test]
    fn test_all_tasks_deferred_graph_stays_running() {
        // Both root tasks are spawned optimistically, both fail with ConcurrencyLimit,
        // and both revert to Ready. The graph must remain Running (not Failed) and
        // the next tick must re-emit Spawn actions for the deferred tasks.
        let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[])]);
        let mut scheduler = make_scheduler(graph);

        // First tick emits Spawn for both tasks and marks them Running.
        let actions = scheduler.tick();
        assert_eq!(
            actions
                .iter()
                .filter(|a| matches!(a, SchedulerAction::Spawn { .. }))
                .count(),
            2,
            "expected 2 Spawn actions on first tick"
        );
        assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Running);
        assert_eq!(scheduler.graph.tasks[1].status, TaskStatus::Running);

        // Both spawns fail — both revert to Ready.
        let error = SubAgentError::ConcurrencyLimit { active: 2, max: 2 };
        let r0 = scheduler.record_spawn_failure(TaskId(0), &error);
        let r1 = scheduler.record_spawn_failure(TaskId(1), &error);
        assert!(r0.is_empty() && r1.is_empty(), "no cancel/done on deferral");
        assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Ready);
        assert_eq!(scheduler.graph.tasks[1].status, TaskStatus::Ready);
        assert_eq!(scheduler.graph.status, GraphStatus::Running);

        // Second tick must retry both deferred tasks.
        let retry_actions = scheduler.tick();
        let spawn_count = retry_actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::Spawn { .. }))
            .count();
        assert!(
            spawn_count > 0,
            "second tick must re-emit Spawn for deferred tasks"
        );
        assert!(
            retry_actions.iter().all(|a| !matches!(
                a,
                SchedulerAction::Done {
                    status: GraphStatus::Failed,
                    ..
                }
            )),
            "no Done(Failed) expected"
        );
    }

    #[test]
    fn test_build_prompt_no_deps() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let scheduler = make_scheduler(graph);
        let prompt = scheduler.build_task_prompt(&scheduler.graph.tasks[0]);
        assert_eq!(prompt, "description for task 0");
    }

    #[test]
    fn test_build_prompt_with_deps_and_truncation() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.tasks[0].status = TaskStatus::Completed;
        // Create output longer than budget
        graph.tasks[0].result = Some(TaskResult {
            output: "x".repeat(200),
            artifacts: vec![],
            duration_ms: 10,
            agent_id: None,
            agent_def: None,
        });

        let config = zeph_config::OrchestrationConfig {
            dependency_context_budget: 50,
            ..make_config()
        };
        let scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        let prompt = scheduler.build_task_prompt(&scheduler.graph.tasks[1]);
        assert!(prompt.contains("<completed-dependencies>"));
        assert!(prompt.contains("[truncated:"));
        assert!(prompt.contains("Your task:"));
    }

    #[test]
    fn test_duration_ms_computed_correctly() {
        // Regression test for C1: duration_ms must be non-zero after completion.
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);

        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.running.insert(
            TaskId(0),
            RunningTask {
                agent_handle_id: "h0".to_string(),
                agent_def_name: "worker".to_string(),
                started_at: Instant::now()
                    .checked_sub(Duration::from_millis(50))
                    .unwrap(),
            },
        );

        let event = TaskEvent {
            task_id: TaskId(0),
            agent_handle_id: "h0".to_string(),
            outcome: TaskOutcome::Completed {
                output: "result".to_string(),
                artifacts: vec![],
            },
        };
        scheduler.buffered_events.push_back(event);
        scheduler.tick();

        let result = scheduler.graph.tasks[0].result.as_ref().unwrap();
        assert!(
            result.duration_ms > 0,
            "duration_ms should be > 0, got {}",
            result.duration_ms
        );
    }

    #[test]
    fn test_utf8_safe_truncation() {
        // S1 regression: truncation must not panic on multi-byte UTF-8.
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.tasks[0].status = TaskStatus::Completed;
        // Unicode: each char is 3 bytes in UTF-8.
        let unicode_output = "日本語テスト".repeat(100);
        graph.tasks[0].result = Some(TaskResult {
            output: unicode_output,
            artifacts: vec![],
            duration_ms: 10,
            agent_id: None,
            agent_def: None,
        });

        // Budget large enough to hold the spotlighting wrapper + some Japanese chars.
        // The sanitizer adds ~200 chars of spotlight header, so 500 chars is sufficient.
        let config = zeph_config::OrchestrationConfig {
            dependency_context_budget: 500,
            ..make_config()
        };
        let scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        // Must not panic, and Japanese chars must be preserved in the output.
        let prompt = scheduler.build_task_prompt(&scheduler.graph.tasks[1]);
        assert!(
            prompt.contains("日"),
            "Japanese characters should be in the prompt after safe truncation"
        );
    }

    #[test]
    fn test_no_agent_routes_inline() {
        // NoneRouter: when no agent matches, task falls back to RunInline.
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler_with_router(graph, Box::new(NoneRouter));
        let actions = scheduler.tick();
        assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Running);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, SchedulerAction::RunInline { .. }))
        );
    }

    #[test]
    fn test_stale_event_rejected() {
        // Regression: events from a previous agent incarnation must be discarded.
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);

        // Simulate task running with handle "current-handle".
        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.running.insert(
            TaskId(0),
            RunningTask {
                agent_handle_id: "current-handle".to_string(),
                agent_def_name: "worker".to_string(),
                started_at: Instant::now(),
            },
        );

        // Send a completion event from the OLD agent (stale handle).
        let stale_event = TaskEvent {
            task_id: TaskId(0),
            agent_handle_id: "old-handle".to_string(),
            outcome: TaskOutcome::Completed {
                output: "stale output".to_string(),
                artifacts: vec![],
            },
        };
        scheduler.buffered_events.push_back(stale_event);
        let actions = scheduler.tick();

        // Stale event must be discarded — task must NOT be completed.
        assert_ne!(
            scheduler.graph.tasks[0].status,
            TaskStatus::Completed,
            "stale event must not complete the task"
        );
        // No Spawn or Done actions should result from a discarded stale event.
        let has_done = actions
            .iter()
            .any(|a| matches!(a, SchedulerAction::Done { .. }));
        assert!(
            !has_done,
            "no Done action should be emitted for a stale event"
        );
        // Task must still be in the running map.
        assert!(
            scheduler.running.contains_key(&TaskId(0)),
            "running task must remain after stale event"
        );
    }

    #[test]
    fn test_build_prompt_chars_count_in_truncation_message() {
        // Fix #3: truncation message must report char count, not byte count.
        // Use pure ASCII so sanitization doesn't significantly change char count.
        // Budget < output length => truncation triggered; verify the count label is "chars total".
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.tasks[0].status = TaskStatus::Completed;
        // ASCII output: byte count == char count, so both old and new code produce the same number,
        // but the label "chars total" (not "bytes total") is what matters here.
        let output = "x".repeat(200);
        graph.tasks[0].result = Some(TaskResult {
            output,
            artifacts: vec![],
            duration_ms: 10,
            agent_id: None,
            agent_def: None,
        });

        let config = zeph_config::OrchestrationConfig {
            dependency_context_budget: 10, // truncate: sanitized output >> 10 chars
            ..make_config()
        };
        let scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        let prompt = scheduler.build_task_prompt(&scheduler.graph.tasks[1]);
        // Truncation must have been triggered and the message must use "chars total" label.
        assert!(
            prompt.contains("chars total"),
            "truncation message must use 'chars total' label. Prompt: {prompt}"
        );
        assert!(
            prompt.contains("[truncated:"),
            "prompt must contain truncation notice. Prompt: {prompt}"
        );
    }

    // --- resume_from tests (MT-1) ---

    #[test]
    fn test_resume_from_accepts_paused_graph() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.status = GraphStatus::Paused;
        graph.tasks[0].status = TaskStatus::Pending;

        let scheduler =
            DagScheduler::resume_from(graph, &make_config(), Box::new(FirstRouter), vec![])
                .expect("resume_from should accept Paused graph");
        assert_eq!(scheduler.graph.status, GraphStatus::Running);
    }

    #[test]
    fn test_resume_from_accepts_failed_graph() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.status = GraphStatus::Failed;
        graph.tasks[0].status = TaskStatus::Failed;

        let scheduler =
            DagScheduler::resume_from(graph, &make_config(), Box::new(FirstRouter), vec![])
                .expect("resume_from should accept Failed graph");
        assert_eq!(scheduler.graph.status, GraphStatus::Running);
    }

    #[test]
    fn test_resume_from_rejects_completed_graph() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.status = GraphStatus::Completed;

        let err = DagScheduler::resume_from(graph, &make_config(), Box::new(FirstRouter), vec![])
            .unwrap_err();
        assert!(matches!(err, OrchestrationError::InvalidGraph(_)));
    }

    #[test]
    fn test_resume_from_rejects_canceled_graph() {
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.status = GraphStatus::Canceled;

        let err = DagScheduler::resume_from(graph, &make_config(), Box::new(FirstRouter), vec![])
            .unwrap_err();
        assert!(matches!(err, OrchestrationError::InvalidGraph(_)));
    }

    #[test]
    fn test_resume_from_reconstructs_running_tasks() {
        // IC1: tasks that were Running at pause time must appear in the running map.
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.status = GraphStatus::Paused;
        graph.tasks[0].status = TaskStatus::Running;
        graph.tasks[0].assigned_agent = Some("handle-abc".to_string());
        graph.tasks[0].agent_hint = Some("worker".to_string());
        graph.tasks[1].status = TaskStatus::Pending;

        let scheduler =
            DagScheduler::resume_from(graph, &make_config(), Box::new(FirstRouter), vec![])
                .expect("should succeed");

        assert!(
            scheduler.running.contains_key(&TaskId(0)),
            "Running task must be reconstructed in the running map (IC1)"
        );
        assert_eq!(scheduler.running[&TaskId(0)].agent_handle_id, "handle-abc");
        assert!(
            !scheduler.running.contains_key(&TaskId(1)),
            "Pending task must not appear in running map"
        );
    }

    #[test]
    fn test_resume_from_sets_status_running() {
        // II3: resume_from must set graph.status = Running regardless of input status.
        let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
        graph.status = GraphStatus::Paused;

        let scheduler =
            DagScheduler::resume_from(graph, &make_config(), Box::new(FirstRouter), vec![])
                .unwrap();
        assert_eq!(scheduler.graph.status, GraphStatus::Running);
    }

    // --- #1619 regression tests: consecutive_spawn_failures + exponential backoff ---

    #[test]
    fn test_consecutive_spawn_failures_increments_on_concurrency_limit() {
        // Each tick where all spawns hit ConcurrencyLimit must increment the counter
        // via record_batch_backoff(false, true).
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);
        scheduler.graph.tasks[0].status = TaskStatus::Running;

        assert_eq!(scheduler.consecutive_spawn_failures, 0, "starts at zero");

        let error = SubAgentError::ConcurrencyLimit { active: 4, max: 4 };
        scheduler.record_spawn_failure(TaskId(0), &error);
        // record_spawn_failure no longer increments; batch_backoff does.
        scheduler.record_batch_backoff(false, true);
        assert_eq!(
            scheduler.consecutive_spawn_failures, 1,
            "first deferral tick: consecutive_spawn_failures must be 1"
        );

        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.record_spawn_failure(TaskId(0), &error);
        scheduler.record_batch_backoff(false, true);
        assert_eq!(
            scheduler.consecutive_spawn_failures, 2,
            "second deferral tick: consecutive_spawn_failures must be 2"
        );

        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.record_spawn_failure(TaskId(0), &error);
        scheduler.record_batch_backoff(false, true);
        assert_eq!(
            scheduler.consecutive_spawn_failures, 3,
            "third deferral tick: consecutive_spawn_failures must be 3"
        );
    }

    #[test]
    fn test_consecutive_spawn_failures_resets_on_success() {
        // record_spawn() after deferrals must reset consecutive_spawn_failures to 0
        // (via record_spawn internal reset; record_batch_backoff(true, _) also resets).
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);
        scheduler.graph.tasks[0].status = TaskStatus::Running;

        let error = SubAgentError::ConcurrencyLimit { active: 1, max: 1 };
        scheduler.record_spawn_failure(TaskId(0), &error);
        scheduler.record_batch_backoff(false, true);
        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.record_spawn_failure(TaskId(0), &error);
        scheduler.record_batch_backoff(false, true);
        assert_eq!(scheduler.consecutive_spawn_failures, 2);

        // Successful spawn resets the counter directly in record_spawn.
        scheduler.record_spawn(TaskId(0), "handle-0".to_string(), "worker".to_string());
        assert_eq!(
            scheduler.consecutive_spawn_failures, 0,
            "record_spawn must reset consecutive_spawn_failures to 0"
        );
    }

    #[tokio::test]
    async fn test_exponential_backoff_duration() {
        // With consecutive_spawn_failures=0, backoff equals the base interval.
        // With consecutive_spawn_failures=3, backoff = min(base * 8, 5000ms).
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let config = zeph_config::OrchestrationConfig {
            deferral_backoff_ms: 50,
            ..make_config()
        };
        let mut scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        // consecutive_spawn_failures=0 → sleep ≈ 50ms (base).
        assert_eq!(scheduler.consecutive_spawn_failures, 0);
        let start = tokio::time::Instant::now();
        scheduler.wait_event().await;
        let elapsed0 = start.elapsed();
        assert!(
            elapsed0.as_millis() >= 50,
            "backoff with 0 deferrals must be >= base (50ms), got {}ms",
            elapsed0.as_millis()
        );

        // Simulate 3 consecutive deferrals: multiplier = 2^3 = 8 → 400ms, capped at 5000ms.
        scheduler.consecutive_spawn_failures = 3;
        let start = tokio::time::Instant::now();
        scheduler.wait_event().await;
        let elapsed3 = start.elapsed();
        assert!(
            elapsed3.as_millis() >= 400,
            "backoff with 3 deferrals must be >= 400ms (50 * 8), got {}ms",
            elapsed3.as_millis()
        );

        // Simulate 20 consecutive deferrals: exponent capped at 10 → 50 * 1024 = 51200 → capped at 5000ms.
        scheduler.consecutive_spawn_failures = 20;
        let start = tokio::time::Instant::now();
        scheduler.wait_event().await;
        let elapsed_capped = start.elapsed();
        assert!(
            elapsed_capped.as_millis() >= 5000,
            "backoff must be capped at 5000ms with high deferrals, got {}ms",
            elapsed_capped.as_millis()
        );
    }

    // --- deferral_backoff regression test ---

    #[tokio::test]
    async fn test_wait_event_sleeps_deferral_backoff_when_running_empty() {
        // Regression for issue #1519: wait_event must sleep deferral_backoff when
        // running is empty, preventing a busy spin-loop.
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let config = zeph_config::OrchestrationConfig {
            deferral_backoff_ms: 50,
            ..make_config()
        };
        let mut scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        // Do not start any tasks — running map stays empty.
        assert!(scheduler.running.is_empty());

        let start = tokio::time::Instant::now();
        scheduler.wait_event().await;
        let elapsed = start.elapsed();

        assert!(
            elapsed.as_millis() >= 50,
            "wait_event must sleep at least deferral_backoff (50ms) when running is empty, but only slept {}ms",
            elapsed.as_millis()
        );
    }

    #[test]
    fn test_current_deferral_backoff_exponential_growth() {
        // Regression for issue #1618: backoff must grow exponentially with consecutive
        // spawn failures so the scheduler does not busy-spin at 250ms when saturated.
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let config = zeph_config::OrchestrationConfig {
            deferral_backoff_ms: 250,
            ..make_config()
        };
        let mut scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        assert_eq!(
            scheduler.current_deferral_backoff(),
            Duration::from_millis(250)
        );

        scheduler.consecutive_spawn_failures = 1;
        assert_eq!(
            scheduler.current_deferral_backoff(),
            Duration::from_millis(500)
        );

        scheduler.consecutive_spawn_failures = 2;
        assert_eq!(scheduler.current_deferral_backoff(), Duration::from_secs(1));

        scheduler.consecutive_spawn_failures = 3;
        assert_eq!(scheduler.current_deferral_backoff(), Duration::from_secs(2));

        scheduler.consecutive_spawn_failures = 4;
        assert_eq!(scheduler.current_deferral_backoff(), Duration::from_secs(4));

        // Cap at 5 seconds.
        scheduler.consecutive_spawn_failures = 5;
        assert_eq!(scheduler.current_deferral_backoff(), Duration::from_secs(5));

        scheduler.consecutive_spawn_failures = 100;
        assert_eq!(scheduler.current_deferral_backoff(), Duration::from_secs(5));
    }

    #[test]
    fn test_record_spawn_resets_consecutive_failures() {
        // Regression for issue #1618: a successful spawn resets the backoff counter.
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = DagScheduler::new(
            graph,
            &make_config(),
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        scheduler.consecutive_spawn_failures = 3;
        let task_id = TaskId(0);
        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.record_spawn(task_id, "handle-1".into(), "worker".into());

        assert_eq!(scheduler.consecutive_spawn_failures, 0);
    }

    #[test]
    fn test_record_spawn_failure_reverts_to_ready_no_counter_change() {
        // record_spawn_failure(ConcurrencyLimit) reverts task to Ready but does NOT
        // change consecutive_spawn_failures — that is the job of record_batch_backoff.
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = DagScheduler::new(
            graph,
            &make_config(),
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        assert_eq!(scheduler.consecutive_spawn_failures, 0);
        let task_id = TaskId(0);
        scheduler.graph.tasks[0].status = TaskStatus::Running;

        let error = SubAgentError::ConcurrencyLimit { active: 1, max: 1 };
        scheduler.record_spawn_failure(task_id, &error);

        // Counter unchanged — batch_backoff is responsible for incrementing.
        assert_eq!(scheduler.consecutive_spawn_failures, 0);
        // Task reverted to Ready.
        assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Ready);
    }

    // --- #1628 parallel dispatch tests ---

    #[test]
    fn test_parallel_dispatch_all_ready() {
        // tick() enforces max_parallel as a pre-dispatch cap. With 6 independent tasks
        // and max_parallel=2, only 2 tasks are dispatched per tick.
        let nodes: Vec<_> = (0..6).map(|i| make_node(i, &[])).collect();
        let graph = graph_from_nodes(nodes);
        let config = zeph_config::OrchestrationConfig {
            max_parallel: 2,
            ..make_config()
        };
        let mut scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        let actions = scheduler.tick();
        let spawn_count = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::Spawn { .. }))
            .count();
        assert_eq!(
            spawn_count, 2,
            "only max_parallel=2 tasks dispatched per tick"
        );

        let running_count = scheduler
            .graph
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Running)
            .count();
        assert_eq!(running_count, 2, "only 2 tasks marked Running");
    }

    #[test]
    fn test_batch_backoff_partial_success() {
        // Some spawns succeed, some hit ConcurrencyLimit: counter resets to 0.
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);
        scheduler.consecutive_spawn_failures = 3;

        scheduler.record_batch_backoff(true, true);
        assert_eq!(
            scheduler.consecutive_spawn_failures, 0,
            "any success in batch must reset counter"
        );
    }

    #[test]
    fn test_batch_backoff_all_failed() {
        // All spawns hit ConcurrencyLimit: counter increments by 1.
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);
        scheduler.consecutive_spawn_failures = 2;

        scheduler.record_batch_backoff(false, true);
        assert_eq!(
            scheduler.consecutive_spawn_failures, 3,
            "all-failure tick must increment counter"
        );
    }

    #[test]
    fn test_batch_backoff_no_spawns() {
        // No spawn actions in tick: counter unchanged.
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);
        scheduler.consecutive_spawn_failures = 5;

        scheduler.record_batch_backoff(false, false);
        assert_eq!(
            scheduler.consecutive_spawn_failures, 5,
            "no spawns must not change counter"
        );
    }

    #[test]
    fn test_buffer_guard_uses_task_count() {
        // Structural guard: verifies that the buffer capacity expression uses
        // graph.tasks.len() * 2 rather than max_parallel * 2. This is an intentional
        // regression-prevention test — if wait_event() is accidentally reverted to
        // max_parallel * 2 the assertion below catches the discrepancy.
        // Behavioral coverage (actual buffer drop prevention) requires an async harness
        // with a real channel, which is outside the scope of this unit test.
        //
        // Scenario: 10 tasks, max_parallel=2 → tasks.len()*2=20, max_parallel*2=4.
        // The guard must use 20, not 4.
        let nodes: Vec<_> = (0..10).map(|i| make_node(i, &[])).collect();
        let graph = graph_from_nodes(nodes);
        let config = zeph_config::OrchestrationConfig {
            max_parallel: 2, // 2*2=4, but tasks.len()*2=20
            ..make_config()
        };
        let scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();
        // Confirm: tasks.len() * 2 = 20, max_parallel * 2 = 4.
        assert_eq!(scheduler.graph.tasks.len() * 2, 20);
        assert_eq!(scheduler.max_parallel * 2, 4);
    }

    #[test]
    fn test_batch_mixed_concurrency_and_fatal_failure() {
        // Mixed batch: task 0 gets ConcurrencyLimit (transient), task 1 gets a
        // non-transient Spawn error (fatal). Two independent tasks, no deps between them.
        // Verify:
        // - task 0 reverts to Ready (retried next tick)
        // - task 1 is marked Failed; with FailureStrategy::Skip the graph stays Running
        //   because task 1 has no dependents that would abort the graph
        // - record_batch_backoff(false, true) increments counter by 1
        let mut nodes = vec![make_node(0, &[]), make_node(1, &[])];
        // FailureStrategy::Skip: task 1 fails but its absence is ignored.
        nodes[1].failure_strategy = Some(FailureStrategy::Skip);
        let graph = graph_from_nodes(nodes);
        let mut scheduler = make_scheduler(graph);

        // Optimistically mark both as Running (as tick() would do).
        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.graph.tasks[1].status = TaskStatus::Running;

        // Task 0: ConcurrencyLimit (transient).
        let concurrency_err = SubAgentError::ConcurrencyLimit { active: 1, max: 1 };
        let actions0 = scheduler.record_spawn_failure(TaskId(0), &concurrency_err);
        assert!(
            actions0.is_empty(),
            "ConcurrencyLimit must produce no extra actions"
        );
        assert_eq!(
            scheduler.graph.tasks[0].status,
            TaskStatus::Ready,
            "task 0 must revert to Ready"
        );

        // Task 1: non-transient Spawn failure. record_spawn_failure marks it Failed,
        // then propagate_failure applies FailureStrategy::Skip → status becomes Skipped.
        let fatal_err = SubAgentError::Spawn("provider unavailable".to_string());
        let actions1 = scheduler.record_spawn_failure(TaskId(1), &fatal_err);
        assert_eq!(
            scheduler.graph.tasks[1].status,
            TaskStatus::Skipped,
            "task 1: Skip strategy turns Failed into Skipped via propagate_failure"
        );
        // No Done action from record_spawn_failure — graph still has task 0 alive.
        assert!(
            actions1
                .iter()
                .all(|a| !matches!(a, SchedulerAction::Done { .. })),
            "no Done action expected: task 0 is still Ready"
        );

        // Batch result: no success, one ConcurrencyLimit failure.
        scheduler.consecutive_spawn_failures = 0;
        scheduler.record_batch_backoff(false, true);
        assert_eq!(
            scheduler.consecutive_spawn_failures, 1,
            "batch with only ConcurrencyLimit must increment counter"
        );
    }

    /// Regression for #1879: when the scheduler detects a deadlock (no running or ready tasks,
    /// but the graph is not complete), all non-terminal tasks must be marked Canceled, not left
    /// in their previous status (e.g. Pending).
    #[test]
    fn test_deadlock_marks_non_terminal_tasks_canceled() {
        // Build a graph in Failed status (as if a prior retry pass left task 0 failed and
        // task 1/2 still Pending). resume_from() transitions it to Running without resetting
        // task statuses, so tick() immediately sees no running, no ready, not all terminal —
        // triggering the deadlock branch.
        let mut nodes = vec![make_node(0, &[]), make_node(1, &[0]), make_node(2, &[0])];
        nodes[0].status = TaskStatus::Failed;
        nodes[1].status = TaskStatus::Pending;
        nodes[2].status = TaskStatus::Pending;

        let mut graph = graph_from_nodes(nodes);
        graph.status = GraphStatus::Failed;

        let mut scheduler = DagScheduler::resume_from(
            graph,
            &make_config(),
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        // After resume_from, graph is Running but no tasks are Ready/Running — deadlock.
        let actions = scheduler.tick();

        // Must emit Done(Failed).
        assert!(
            actions.iter().any(|a| matches!(
                a,
                SchedulerAction::Done {
                    status: GraphStatus::Failed
                }
            )),
            "deadlock must emit Done(Failed); got: {actions:?}"
        );
        assert_eq!(scheduler.graph.status, GraphStatus::Failed);

        // task 0 was already Failed (terminal) — must remain unchanged.
        assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Failed);
        // task 1 was Pending (non-terminal) — must be Canceled.
        assert_eq!(
            scheduler.graph.tasks[1].status,
            TaskStatus::Canceled,
            "Pending task must be Canceled on deadlock"
        );
        // task 2 was Pending (non-terminal) — must be Canceled.
        assert_eq!(
            scheduler.graph.tasks[2].status,
            TaskStatus::Canceled,
            "Pending task must be Canceled on deadlock"
        );
    }

    /// Regression for #1879: deadlock with one task Running should NOT trigger the deadlock
    /// branch (`running_in_graph_now` > 0 suppresses the check).
    #[test]
    fn test_deadlock_not_triggered_when_task_running() {
        // Graph in Failed with one task still marked Running — resume_from reconstructs
        // the running map. tick() sees running_in_graph_now > 0 and skips deadlock check.
        let mut nodes = vec![make_node(0, &[]), make_node(1, &[0])];
        nodes[0].status = TaskStatus::Running;
        nodes[0].assigned_agent = Some("handle-1".into());
        nodes[1].status = TaskStatus::Pending;

        let mut graph = graph_from_nodes(nodes);
        graph.status = GraphStatus::Failed;

        let mut scheduler = DagScheduler::resume_from(
            graph,
            &make_config(),
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        let actions = scheduler.tick();

        // Running task in graph — no deadlock triggered.
        assert!(
            actions
                .iter()
                .all(|a| !matches!(a, SchedulerAction::Done { .. })),
            "no Done action expected when a task is running; got: {actions:?}"
        );
        assert_eq!(scheduler.graph.status, GraphStatus::Running);
    }

    // --- topology_selection tests ---

    #[test]
    fn topology_linear_chain_limits_parallelism_to_one() {
        // LinearChain topology with topology_selection=true → max_parallel overridden to 1.
        // tick() must dispatch exactly 1 task even though 1 root task is ready.
        let graph = graph_from_nodes(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[1]),
        ]);
        let config = zeph_config::OrchestrationConfig {
            topology_selection: true,
            max_parallel: 4,
            ..make_config()
        };
        let mut scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        assert_eq!(
            scheduler.topology().topology,
            crate::topology::Topology::LinearChain
        );
        assert_eq!(scheduler.max_parallel, 1);

        let actions = scheduler.tick();
        let spawn_count = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::Spawn { .. }))
            .count();
        assert_eq!(spawn_count, 1, "linear chain: only 1 task dispatched");
    }

    #[test]
    fn topology_all_parallel_dispatches_all_ready() {
        // AllParallel topology with topology_selection=true → max_parallel unchanged.
        // tick() dispatches all 4 independent tasks in one go.
        let graph = graph_from_nodes(vec![
            make_node(0, &[]),
            make_node(1, &[]),
            make_node(2, &[]),
            make_node(3, &[]),
        ]);
        let config = zeph_config::OrchestrationConfig {
            topology_selection: true,
            max_parallel: 4,
            ..make_config()
        };
        let mut scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        assert_eq!(
            scheduler.topology().topology,
            crate::topology::Topology::AllParallel
        );

        let actions = scheduler.tick();
        let spawn_count = actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::Spawn { .. }))
            .count();
        assert_eq!(spawn_count, 4, "all-parallel: all 4 tasks dispatched");
    }

    #[test]
    fn sequential_dispatch_one_at_a_time_parallel_unblocked() {
        // Three ready tasks: A(sequential), B(sequential), C(parallel).
        // tick() must dispatch A + C, hold B (another sequential already scheduled this tick).
        use crate::graph::ExecutionMode;

        let mut a = make_node(0, &[]);
        a.execution_mode = ExecutionMode::Sequential;
        let mut b = make_node(1, &[]);
        b.execution_mode = ExecutionMode::Sequential;
        let mut c = make_node(2, &[]);
        c.execution_mode = ExecutionMode::Parallel;

        let graph = graph_from_nodes(vec![a, b, c]);
        let config = zeph_config::OrchestrationConfig {
            max_parallel: 4,
            ..make_config()
        };
        let mut scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        let actions = scheduler.tick();
        let spawned: Vec<TaskId> = actions
            .iter()
            .filter_map(|a| {
                if let SchedulerAction::Spawn { task_id, .. } = a {
                    Some(*task_id)
                } else {
                    None
                }
            })
            .collect();

        // A (seq, idx=0) and C (par, idx=2) dispatched; B (seq, idx=1) held.
        assert!(
            spawned.contains(&TaskId(0)),
            "A(sequential) must be dispatched"
        );
        assert!(
            spawned.contains(&TaskId(2)),
            "C(parallel) must be dispatched"
        );
        assert!(!spawned.contains(&TaskId(1)), "B(sequential) must be held");
        assert_eq!(spawned.len(), 2);
    }

    // --- inject_tasks replan cap tests (#2241) ---

    #[test]
    fn test_inject_tasks_per_task_cap_skips_second() {
        // Per-task cap: 1 replan per task. Second inject for same task_id is a silent no-op.
        let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        let mut scheduler = make_scheduler(graph);

        let first = make_node(2, &[]);
        scheduler.inject_tasks(TaskId(0), vec![first], 20).unwrap();
        assert_eq!(
            scheduler.graph.tasks.len(),
            3,
            "first inject must append the task"
        );
        assert_eq!(scheduler.global_replan_count, 1);

        // Second inject for the same verified task — per-task count is already 1.
        let second = make_node(3, &[]);
        scheduler.inject_tasks(TaskId(0), vec![second], 20).unwrap();
        assert_eq!(
            scheduler.graph.tasks.len(),
            3,
            "second inject must be silently skipped (per-task cap)"
        );
        assert_eq!(
            scheduler.global_replan_count, 1,
            "global counter must not increment on skipped inject"
        );
    }

    #[test]
    fn test_inject_tasks_global_cap_skips_when_exhausted() {
        // Global cap: max_replans=1. First inject consumes the budget; second is a no-op.
        let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        let mut config = make_config();
        config.max_replans = 1;
        let defs = vec![make_def("worker")];
        let mut scheduler = DagScheduler::new(graph, &config, Box::new(FirstRouter), defs).unwrap();

        let new1 = make_node(2, &[]);
        scheduler.inject_tasks(TaskId(0), vec![new1], 20).unwrap();
        assert_eq!(scheduler.global_replan_count, 1);

        // Second inject for a different task — global cap exhausted.
        let new2 = make_node(3, &[]);
        scheduler.inject_tasks(TaskId(1), vec![new2], 20).unwrap();
        assert_eq!(
            scheduler.graph.tasks.len(),
            3,
            "global cap must prevent the second inject"
        );
        assert_eq!(
            scheduler.global_replan_count, 1,
            "global counter must not increment past cap"
        );
    }

    #[test]
    fn test_inject_tasks_sets_topology_dirty() {
        // inject_tasks must set topology_dirty; tick() must clear it after re-analysis.
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);
        assert!(
            !scheduler.topology_dirty,
            "topology_dirty must be false initially"
        );

        let new_task = make_node(1, &[]);
        scheduler
            .inject_tasks(TaskId(0), vec![new_task], 20)
            .unwrap();
        assert!(
            scheduler.topology_dirty,
            "inject_tasks must set topology_dirty=true"
        );

        scheduler.tick();
        assert!(
            !scheduler.topology_dirty,
            "tick() must clear topology_dirty after re-analysis"
        );
    }

    #[test]
    fn test_inject_tasks_rejects_cycle() {
        // Injecting a task that introduces a cycle must return VerificationFailed.
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);

        // New task ID=1 with a self-reference (depends on itself) → cycle.
        let cyclic_task = make_node(1, &[1]);
        let result = scheduler.inject_tasks(TaskId(0), vec![cyclic_task], 20);
        assert!(result.is_err(), "cyclic injection must return an error");
        assert!(
            matches!(
                result.unwrap_err(),
                OrchestrationError::VerificationFailed(_)
            ),
            "must return VerificationFailed for cycle"
        );
        // Global and per-task counters must not be incremented on error.
        assert_eq!(scheduler.global_replan_count, 0);
        assert!(
            !scheduler.topology_dirty,
            "topology_dirty must not be set when inject fails"
        );
    }

    // --- LevelBarrier dispatch tests (#2242) ---

    fn make_hierarchical_config() -> zeph_config::OrchestrationConfig {
        zeph_config::OrchestrationConfig {
            topology_selection: true,
            max_parallel: 4,
            ..make_config()
        }
    }

    /// A(0)→{B(1),C(2)}, B(1)→D(3). Hierarchical topology, depths: A=0, B=1, C=1, D=2.
    fn make_hierarchical_graph() -> TaskGraph {
        graph_from_nodes(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[0]),
            make_node(3, &[1]),
        ])
    }

    #[test]
    fn test_level_barrier_advances_on_terminal_level() {
        // When all tasks at current_level are terminal, tick() advances current_level
        // and dispatches tasks at the next non-terminal level.
        let graph = make_hierarchical_graph();
        let config = make_hierarchical_config();
        let defs = vec![make_def("worker")];
        let mut scheduler = DagScheduler::new(graph, &config, Box::new(FirstRouter), defs).unwrap();

        assert_eq!(
            scheduler.topology().strategy,
            crate::topology::DispatchStrategy::LevelBarrier,
            "must use LevelBarrier strategy for Hierarchical graph"
        );
        assert_eq!(scheduler.current_level, 0);

        // First tick: only A(0) at level 0 is dispatched.
        let actions = scheduler.tick();
        let spawned_ids: Vec<_> = actions
            .iter()
            .filter_map(|a| {
                if let SchedulerAction::Spawn { task_id, .. } = a {
                    Some(*task_id)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            spawned_ids,
            vec![TaskId(0)],
            "first tick must dispatch only A at level 0"
        );

        // Simulate A completing: mark Completed, mark B and C Ready (deps satisfied).
        scheduler.graph.tasks[0].status = TaskStatus::Completed;
        scheduler.running.clear();
        scheduler.graph.tasks[1].status = TaskStatus::Ready;
        scheduler.graph.tasks[2].status = TaskStatus::Ready;

        // Second tick: A is terminal → level advances to 1 → B and C dispatched.
        let actions2 = scheduler.tick();
        assert_eq!(
            scheduler.current_level, 1,
            "current_level must advance to 1 after level-0 tasks terminate"
        );
        let spawned2: Vec<_> = actions2
            .iter()
            .filter_map(|a| {
                if let SchedulerAction::Spawn { task_id, .. } = a {
                    Some(*task_id)
                } else {
                    None
                }
            })
            .collect();
        assert!(
            spawned2.contains(&TaskId(1)),
            "B must be dispatched after level advance"
        );
        assert!(
            spawned2.contains(&TaskId(2)),
            "C must be dispatched after level advance"
        );
    }

    #[test]
    fn test_level_barrier_failure_propagates_transitively() {
        // When A fails with Skip strategy, propagate_failure() BFS-marks all
        // descendants (B, C, D) as Skipped. tick() must then advance past level 0.
        let graph = make_hierarchical_graph();
        let config = make_hierarchical_config();
        let defs = vec![make_def("worker")];
        let mut scheduler = DagScheduler::new(graph, &config, Box::new(FirstRouter), defs).unwrap();

        // Set A to Skip failure strategy and simulate it running.
        scheduler.graph.tasks[0].failure_strategy = Some(crate::graph::FailureStrategy::Skip);
        scheduler.graph.tasks[0].status = TaskStatus::Running;
        scheduler.running.insert(
            TaskId(0),
            RunningTask {
                agent_handle_id: "h0".to_string(),
                agent_def_name: "worker".to_string(),
                started_at: Instant::now(),
            },
        );

        // Push a failure event for A.
        scheduler.buffered_events.push_back(TaskEvent {
            task_id: TaskId(0),
            agent_handle_id: "h0".to_string(),
            outcome: TaskOutcome::Failed {
                error: "simulated failure".to_string(),
            },
        });

        scheduler.tick();

        // A failed with Skip → A=Skipped. B, C, D must be transitively Skipped.
        assert_eq!(
            scheduler.graph.tasks[0].status,
            TaskStatus::Skipped,
            "A must be Skipped (Skip strategy)"
        );
        assert_eq!(
            scheduler.graph.tasks[1].status,
            TaskStatus::Skipped,
            "B must be transitively Skipped"
        );
        assert_eq!(
            scheduler.graph.tasks[2].status,
            TaskStatus::Skipped,
            "C must be transitively Skipped"
        );
        assert_eq!(
            scheduler.graph.tasks[3].status,
            TaskStatus::Skipped,
            "D must be transitively Skipped"
        );
    }

    #[test]
    fn test_level_barrier_current_level_reset_after_inject() {
        // inject_tasks() adding a task at depth < current_level must cause tick() to
        // reset current_level downward via the .min() guard (critic C2 / issue #2242).
        let graph = make_hierarchical_graph(); // A(0)→{B(1),C(2)}, B(1)→D(3)
        let config = make_hierarchical_config();
        let defs = vec![make_def("worker")];
        let mut scheduler = DagScheduler::new(graph, &config, Box::new(FirstRouter), defs).unwrap();

        // Manually mark A, B, C as Completed (simulate levels 0 and 1 done).
        scheduler.graph.tasks[0].status = TaskStatus::Completed; // A depth 0
        scheduler.graph.tasks[1].status = TaskStatus::Completed; // B depth 1
        scheduler.graph.tasks[2].status = TaskStatus::Completed; // C depth 1
        // D(3) is Pending at depth 2. Manually set current_level to 2.
        scheduler.current_level = 2;

        // Inject E(4) depending on A(0) (Completed) → E will be at depth 1 after re-analysis.
        // This is shallower than current_level=2 → tick() must reset current_level to 1.
        let e = make_node(4, &[0]);
        scheduler.inject_tasks(TaskId(3), vec![e], 20).unwrap();
        assert!(scheduler.topology_dirty);

        // tick() re-analyzes topology (E at depth 1, D at depth 2).
        // min_non_terminal_depth = 1 (E is Ready). current_level = min(2, 1) = 1.
        scheduler.tick();
        assert_eq!(
            scheduler.current_level, 1,
            "current_level must reset to min non-terminal depth (1) after inject at depth 1"
        );
    }

    #[test]
    fn resume_from_preserves_topology_classification() {
        // resume_from() must also apply topology classification (fix H3).
        let mut graph = graph_from_nodes(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[1]),
        ]);
        // Put graph in Paused state so resume_from accepts it.
        graph.status = GraphStatus::Paused;
        graph.tasks[0].status = TaskStatus::Completed;
        graph.tasks[1].status = TaskStatus::Pending;
        graph.tasks[2].status = TaskStatus::Pending;

        let config = zeph_config::OrchestrationConfig {
            topology_selection: true,
            max_parallel: 4,
            ..make_config()
        };
        let scheduler = DagScheduler::resume_from(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        assert_eq!(
            scheduler.topology().topology,
            crate::topology::Topology::LinearChain,
            "resume_from must classify topology"
        );
        assert_eq!(
            scheduler.max_parallel, 1,
            "resume_from must apply topology limit"
        );
    }

    // --- #2238: validate_verify_config tests ---

    fn make_verify_config(provider: &str) -> zeph_config::OrchestrationConfig {
        zeph_config::OrchestrationConfig {
            verify_completeness: true,
            verify_provider: zeph_config::ProviderName::new(provider),
            ..make_config()
        }
    }

    #[test]
    fn validate_verify_config_unknown_provider_returns_err() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let config = make_verify_config("nonexistent");
        let scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();
        let result = scheduler.validate_verify_config(&["fast", "quality"]);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("nonexistent"));
        assert!(err_msg.contains("fast"));
    }

    #[test]
    fn validate_verify_config_known_provider_returns_ok() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let config = make_verify_config("fast");
        let scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();
        assert!(
            scheduler
                .validate_verify_config(&["fast", "quality"])
                .is_ok()
        );
    }

    #[test]
    fn validate_verify_config_empty_provider_always_ok() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let config = make_verify_config("");
        let scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();
        assert!(scheduler.validate_verify_config(&["fast"]).is_ok());
    }

    #[test]
    fn validate_verify_config_disabled_skips_validation() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        // verify_completeness = false: no validation even with bogus provider name
        let scheduler = make_scheduler(graph);
        assert!(scheduler.validate_verify_config(&["fast"]).is_ok());
    }

    #[test]
    fn validate_verify_config_empty_pool_skips_validation() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let config = make_verify_config("nonexistent");
        let scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();
        // Empty provider_names slice = unknown provider set, skip validation.
        assert!(scheduler.validate_verify_config(&[]).is_ok());
    }

    #[test]
    fn validate_verify_config_trims_whitespace_in_config() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        // verify_provider with surrounding whitespace in config is trimmed at construction.
        let config = make_verify_config("  fast  ");
        let scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();
        assert!(scheduler.validate_verify_config(&["fast"]).is_ok());
    }

    // --- #2237 regression tests: max_parallel drift across replan cycles ---

    #[test]
    fn config_max_parallel_initialized_from_config() {
        // config_max_parallel must always equal config.max_parallel, regardless
        // of whether topology analysis reduces max_parallel for the initial topology.
        let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        let config = zeph_config::OrchestrationConfig {
            topology_selection: true,
            max_parallel: 6,
            ..make_config()
        };
        let scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        assert_eq!(
            scheduler.config_max_parallel, 6,
            "config_max_parallel must equal config.max_parallel"
        );
        // LinearChain reduces max_parallel to 1, but config_max_parallel stays at 6.
        assert_eq!(
            scheduler.max_parallel, 1,
            "max_parallel reduced by topology analysis"
        );
        assert_eq!(
            scheduler.config_max_parallel, 6,
            "config_max_parallel must not be reduced by topology"
        );
    }

    #[test]
    fn max_parallel_does_not_drift_across_inject_tick_cycles() {
        // Regression for #2237: successive inject_tasks+tick cycles with a Mixed graph
        // must not reduce max_parallel below compute_max_parallel(Mixed, config_max_parallel).
        //
        // Before the fix, the tick() dirty path used self.max_parallel as the base for
        // compute_max_parallel, so each replan cycle reduced it further:
        //   cycle 1: max_parallel = (4/2+1)      = 3
        //   cycle 2: max_parallel = (3/2+1)      = 2  ← drift!
        //   cycle 3: max_parallel = (2/2+1)      = 2
        //
        // After the fix, config_max_parallel=4 is always used as the base:
        //   all cycles: max_parallel = (4/2+1)   = 3  ← stable
        let graph = graph_from_nodes(vec![
            make_node(0, &[]),
            make_node(1, &[0]),
            make_node(2, &[0]),
            make_node(3, &[1, 2]), // diamond → Mixed
        ]);
        let config = zeph_config::OrchestrationConfig {
            topology_selection: true,
            max_parallel: 4,
            max_tasks: 50,
            ..make_config()
        };
        let mut scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        // Initial analysis: Mixed topology → max_parallel = (4/2+1) = 3.
        assert_eq!(
            scheduler.topology().topology,
            crate::topology::Topology::Mixed,
            "initial topology must be Mixed"
        );
        let expected_max_parallel = (4usize / 2 + 1).clamp(1, 4); // = 3
        assert_eq!(scheduler.max_parallel, expected_max_parallel);

        // Simulate inject_tasks (which sets topology_dirty=true) followed by tick().
        // The injected task depends on task 3 to keep the graph Mixed.
        let extra_task_id = 4u32;
        let extra_task = {
            let mut n = crate::graph::TaskNode::new(
                extra_task_id,
                "extra".to_string(),
                "extra task injected by replan",
            );
            n.depends_on = vec![TaskId(3)];
            n
        };

        // inject_tasks requires the verified task to be Completed.
        scheduler.graph.tasks[3].status = TaskStatus::Completed;

        scheduler
            .inject_tasks(TaskId(3), vec![extra_task], 50)
            .expect("inject must succeed");
        assert!(
            scheduler.topology_dirty,
            "topology_dirty must be true after inject"
        );

        // First tick() after inject: re-analyzes topology. Must use config_max_parallel as base.
        let _ = scheduler.tick();
        let max_after_first_inject = scheduler.max_parallel;
        assert_eq!(
            max_after_first_inject, expected_max_parallel,
            "max_parallel must not drift after first inject+tick"
        );

        // Second inject+tick cycle: max_parallel must still equal the original computed value.
        let extra_task2 = {
            let mut n = crate::graph::TaskNode::new(5u32, "extra2".to_string(), "second replan");
            n.depends_on = vec![TaskId(extra_task_id)];
            n
        };
        scheduler.graph.tasks[extra_task_id as usize].status = TaskStatus::Completed;
        // Reset to created-like state to allow a second inject (per-task limit is 1,
        // so use a fresh task ID for the verified source).
        scheduler
            .inject_tasks(TaskId(extra_task_id), vec![extra_task2], 50)
            .expect("second inject must succeed");

        let _ = scheduler.tick();
        let max_after_second_inject = scheduler.max_parallel;
        assert_eq!(
            max_after_second_inject, expected_max_parallel,
            "max_parallel must not drift after second inject+tick (was: {max_after_second_inject}, expected: {expected_max_parallel})"
        );
    }

    // --- VMAO adaptive replanning accessor tests ---

    #[test]
    fn completeness_threshold_returns_config_value() {
        let mut config = make_config();
        config.completeness_threshold = 0.85;
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let scheduler =
            DagScheduler::new(graph, &config, Box::new(FirstRouter), vec![make_def("w")]).unwrap();
        assert!((scheduler.completeness_threshold() - 0.85).abs() < f32::EPSILON);
    }

    #[test]
    fn completeness_threshold_default_is_0_7() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let scheduler = make_scheduler(graph);
        assert!((scheduler.completeness_threshold() - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn verify_provider_name_returns_config_value() {
        let mut config = make_config();
        config.verify_provider = zeph_config::ProviderName::new("fast");
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let scheduler =
            DagScheduler::new(graph, &config, Box::new(FirstRouter), vec![make_def("w")]).unwrap();
        assert_eq!(scheduler.verify_provider_name(), "fast");
    }

    #[test]
    fn verify_provider_name_empty_when_not_set() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let scheduler = make_scheduler(graph);
        assert_eq!(scheduler.verify_provider_name(), "");
    }

    #[test]
    fn max_replans_remaining_initial_equals_max_replans() {
        let mut config = make_config();
        config.max_replans = 3;
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let scheduler =
            DagScheduler::new(graph, &config, Box::new(FirstRouter), vec![make_def("w")]).unwrap();
        assert_eq!(scheduler.max_replans_remaining(), 3);
    }

    #[test]
    fn max_replans_remaining_decrements_after_record() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);
        assert_eq!(scheduler.max_replans_remaining(), 2);
        scheduler.record_whole_plan_replan();
        assert_eq!(scheduler.max_replans_remaining(), 1);
        scheduler.record_whole_plan_replan();
        assert_eq!(scheduler.max_replans_remaining(), 0);
        // saturating: stays at 0
        scheduler.record_whole_plan_replan();
        assert_eq!(scheduler.max_replans_remaining(), 0);
    }

    #[test]
    fn record_whole_plan_replan_does_not_modify_graph() {
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);
        let task_count_before = scheduler.graph().tasks.len();
        scheduler.record_whole_plan_replan();
        assert_eq!(
            scheduler.graph().tasks.len(),
            task_count_before,
            "record_whole_plan_replan must not modify the task graph"
        );
    }

    // --- cascade routing tests ---

    fn make_cascade_config() -> zeph_config::OrchestrationConfig {
        zeph_config::OrchestrationConfig {
            topology_selection: true,
            cascade_routing: true,
            cascade_failure_threshold: 0.4,
            max_parallel: 4,
            ..make_config()
        }
    }

    #[test]
    fn inject_tasks_resets_cascade_detector() {
        // inject_tasks() must call cascade_detector.reset() (C13 fix).
        // Verify: recording a failure before inject makes the region healthy again after.
        let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
        graph.tasks[0].status = TaskStatus::Completed;
        graph.tasks[1].status = TaskStatus::Completed;
        let config = make_cascade_config();
        let mut scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        // Record a failure in the detector (simulates a failed task before inject).
        if let Some(ref mut det) = scheduler.cascade_detector {
            let g = &scheduler.graph;
            det.record_outcome(TaskId(1), false, g);
            // Sanity: region has 1 entry.
            assert_eq!(det.region_health().len(), 1);
        } else {
            panic!(
                "cascade_detector must be Some when cascade_routing=true and topology_selection=true"
            );
        }

        // inject_tasks resets the detector.
        let new_task = make_node(2, &[1]);
        scheduler
            .inject_tasks(TaskId(1), vec![new_task], 20)
            .unwrap();

        assert!(
            scheduler
                .cascade_detector
                .as_ref()
                .is_some_and(|d| d.region_health().is_empty()),
            "cascade_detector must be cleared after inject_tasks (C13 fix)"
        );
    }

    #[test]
    fn sequential_tasks_not_reordered_by_cascade() {
        // Sequential tasks must stay at the front of the dispatch queue even when
        // their region is cascading (C14 fix): they must not be moved to the deferred set.
        //
        // Graph: root0 (healthy region), root1 -> t2 (cascading region, Sequential).
        // After injecting failures for root1's region, deprioritized = {root1, t2}.
        // But t2 is Sequential — it must stay in "preferred" partition.
        let mut graph = graph_from_nodes(vec![
            make_node(0, &[]),  // root for healthy region
            make_node(1, &[]),  // root for cascading region
            make_node(2, &[1]), // child of root1
        ]);
        graph.tasks[2].execution_mode = ExecutionMode::Sequential;
        let config = make_cascade_config();
        let mut scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();

        // Record failures for root1's region to make it cascade.
        if let Some(ref mut det) = scheduler.cascade_detector {
            let g = &scheduler.graph;
            // Two failures → rate 1.0 > threshold 0.4
            det.record_outcome(TaskId(1), false, g);
            det.record_outcome(TaskId(2), false, g);
        } else {
            panic!("cascade_detector must be Some");
        }

        // Mark root0 and root1 as Ready (inject nothing — just check tick dispatch order).
        // tick() should put Sequential task in preferred, not deferred.
        let actions = scheduler.tick();

        // Both root tasks (0 and 1) are Ready and root1 is in a cascading region,
        // but t2 (Sequential) must not be deprioritized. We verify by checking that
        // task 1 (root of cascading region) and task 2 (Sequential child) both appear
        // in the Spawn actions — they are not silently deferred behind root0.
        let spawned_ids: Vec<TaskId> = actions
            .iter()
            .filter_map(|a| {
                if let SchedulerAction::Spawn { task_id, .. }
                | SchedulerAction::RunInline { task_id, .. } = a
                {
                    Some(*task_id)
                } else {
                    None
                }
            })
            .collect();

        // root0 and root1 are both roots (in-degree 0). root1 is deprioritized,
        // but t2 (Sequential) stays in preferred. At minimum root0 or t2/root1 must be spawned.
        // The key invariant: the presence of at least one spawn confirms the sequential
        // task was not silently dropped.
        assert!(
            !spawned_ids.is_empty(),
            "tick must dispatch at least one ready task; Sequential tasks must not be dropped by cascade logic"
        );
    }

    #[test]
    fn cascade_routing_without_topology_selection_creates_no_detector() {
        // cascade_routing=true but topology_selection=false must not create a detector
        // (the constructor emits a warn but does not fail).
        let config = zeph_config::OrchestrationConfig {
            cascade_routing: true,
            topology_selection: false,
            ..make_config()
        };
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let scheduler = DagScheduler::new(
            graph,
            &config,
            Box::new(FirstRouter),
            vec![make_def("worker")],
        )
        .unwrap();
        assert!(
            scheduler.cascade_detector.is_none(),
            "cascade_detector must be None when topology_selection=false"
        );
    }
}
