// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! DAG execution scheduler: drives task graph execution by emitting `SchedulerAction` commands.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use super::dag;
use super::error::OrchestrationError;
use super::graph::{GraphStatus, TaskGraph, TaskId, TaskNode, TaskResult, TaskStatus};
use super::router::AgentRouter;
use crate::config::OrchestrationConfig;
use crate::sanitizer::{
    ContentIsolationConfig, ContentSanitizer, ContentSource, ContentSourceKind,
};
use crate::subagent::SubAgentDef;

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
///                     Err(e) => { for a in scheduler.record_spawn_failure(task_id, &e.to_string()) { /* exec */ } }
///                 }
///             }
///             Cancel { agent_handle_id } => { manager.cancel(&agent_handle_id); }
///             Done { .. } => break,
///         }
///     }
///     scheduler.wait_event().await;
/// }
/// ```
pub struct DagScheduler {
    graph: TaskGraph,
    max_parallel: usize,
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
}

impl std::fmt::Debug for DagScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DagScheduler")
            .field("graph_id", &self.graph.id)
            .field("graph_status", &self.graph.status)
            .field("running_count", &self.running.len())
            .field("max_parallel", &self.max_parallel)
            .field("task_timeout_secs", &self.task_timeout.as_secs())
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

        Ok(Self {
            graph,
            max_parallel: config.max_parallel as usize,
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

        Ok(Self {
            graph,
            max_parallel: config.max_parallel as usize,
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
        })
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
    pub fn tick(&mut self) -> Vec<SchedulerAction> {
        if self.graph.status != GraphStatus::Running {
            return vec![SchedulerAction::Done {
                status: self.graph.status,
            }];
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

        // Find ready tasks and schedule them up to max_parallel.
        let ready = dag::ready_tasks(&self.graph);
        // Count tasks that are Running in the graph (includes optimistically-marked ones
        // that haven't been added to self.running yet via record_spawn). This prevents
        // the false-deadlock detection from firing while Spawn actions are in-flight.
        let running_in_graph = self
            .graph
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Running)
            .count();
        let slots_available = self.max_parallel.saturating_sub(running_in_graph);

        for task_id in ready.into_iter().take(slots_available) {
            let task = &self.graph.tasks[task_id.index()];

            let Some(agent_def_name) = self.router.route(task, &self.available_agents) else {
                tracing::debug!(
                    task_id = %task_id,
                    title = %task.title,
                    "no agent available, routing task to main agent inline"
                );
                let prompt = self.build_task_prompt(task);
                self.graph.tasks[task_id.index()].status = TaskStatus::Running;
                actions.push(SchedulerAction::RunInline { task_id, prompt });
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
        }

        // Check for completion or deadlock.
        // Use graph Running status count to avoid false positives while Spawn actions
        // are in-flight (record_spawn hasn't been called yet for freshly emitted spawns).
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
    pub async fn wait_event(&mut self) {
        if self.running.is_empty() {
            tokio::time::sleep(self.deferral_backoff).await;
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
                // SEC-ORCH-02: guard against unbounded buffer growth.
                if self.buffered_events.len() >= self.max_parallel * 2 {
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
    pub fn record_spawn(
        &mut self,
        task_id: TaskId,
        agent_handle_id: String,
        agent_def_name: String,
    ) {
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
    pub fn record_spawn_failure(&mut self, task_id: TaskId, error: &str) -> Vec<SchedulerAction> {
        // Transient condition: the SubAgentManager rejected the spawn because all
        // concurrency slots are occupied. Revert to Ready so the next tick retries.
        if error.contains("concurrency limit") {
            tracing::debug!(
                task_id = %task_id,
                "concurrency limit reached, deferring task to next tick"
            );
            self.graph.tasks[task_id.index()].status = TaskStatus::Ready;
            return Vec::new();
        }

        // SEC-ORCH-04: truncate error to avoid logging sensitive internal details.
        let error_excerpt: String = error.chars().take(512).collect();
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
                    output,
                    artifacts,
                    duration_ms,
                    agent_id: Some(agent_handle_id),
                    agent_def: agent_def_name,
                });

                // Mark newly unblocked tasks as Ready.
                let newly_ready = dag::ready_tasks(&self.graph);
                for ready_id in newly_ready {
                    if self.graph.tasks[ready_id.index()].status == TaskStatus::Pending {
                        self.graph.tasks[ready_id.index()].status = TaskStatus::Ready;
                    }
                }

                Vec::new()
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
    use crate::orchestration::graph::{
        FailureStrategy, GraphStatus, TaskGraph, TaskNode, TaskStatus,
    };

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
        use crate::subagent::def::{SkillFilter, SubAgentPermissions, ToolPolicy};
        SubAgentDef {
            name: name.to_string(),
            description: format!("{name} agent"),
            model: None,
            tools: ToolPolicy::InheritAll,
            disallowed_tools: vec![],
            permissions: SubAgentPermissions::default(),
            skills: SkillFilter::default(),
            system_prompt: String::new(),
            hooks: Default::default(),
            memory: None,
            source: None,
            file_path: None,
        }
    }

    fn make_config() -> crate::config::OrchestrationConfig {
        crate::config::OrchestrationConfig {
            enabled: true,
            max_tasks: 20,
            max_parallel: 4,
            default_failure_strategy: "abort".to_string(),
            default_max_retries: 3,
            task_timeout_secs: 300,
            planner_model: None,
            planner_max_tokens: 4096,
            dependency_context_budget: 16384,
            confirm_before_execute: true,
            aggregator_max_tokens: 4096,
            deferral_backoff_ms: 250,
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
    fn test_tick_respects_max_parallel() {
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
        assert_eq!(spawn_count, 2);
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

        let actions = scheduler.record_spawn_failure(TaskId(0), "spawn error");
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
        let actions = scheduler.record_spawn_failure(TaskId(0), "concurrency limit 4 reached");
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
        // spawn_for_task() uses a slightly different format string — verify both are handled.
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let mut scheduler = make_scheduler(graph);
        scheduler.graph.tasks[0].status = TaskStatus::Running;

        let actions = scheduler.record_spawn_failure(TaskId(0), "concurrency limit reached");
        assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Ready);
        assert!(actions.is_empty());
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

        let config = crate::config::OrchestrationConfig {
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
        let config = crate::config::OrchestrationConfig {
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

        let config = crate::config::OrchestrationConfig {
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

    // --- deferral_backoff regression test ---

    #[tokio::test]
    async fn test_wait_event_sleeps_deferral_backoff_when_running_empty() {
        // Regression for issue #1519: wait_event must sleep deferral_backoff when
        // running is empty, preventing a busy spin-loop.
        let graph = graph_from_nodes(vec![make_node(0, &[])]);
        let config = crate::config::OrchestrationConfig {
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
}
