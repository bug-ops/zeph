// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Sub-agent lifecycle management: spawn, cancel, collect, and resume.

mod collect;
mod secrets;
mod spawn;
mod worktree;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use zeph_common::task_supervisor::BlockingHandle;
use zeph_common::{SkillTrustLevel, TaskSupervisor};
use zeph_config::{ContentIsolationConfig, McpServerConfig};
use zeph_llm::provider::Message;

use crate::def::{PermissionMode, SubAgentDef};
use crate::durable::DurableResolverSeat;
use crate::error::SubAgentError;
use crate::fleet::SharedFleetRegistry;
use crate::grants::{GrantedSecret, PermissionGrants, SecretRequest};
use crate::state::SubAgentState;

/// Parent-derived state propagated to a spawned sub-agent at spawn time.
///
/// All fields default to empty/`None`, preserving existing behavior when callers
/// pass `SpawnContext::default()`.
///
/// # Constraint propagation
///
/// [`max_trust_level`][Self::max_trust_level] and
/// [`inherited_tool_allowlist`][Self::inherited_tool_allowlist] implement transitive
/// constraint propagation: safety constraints set at orchestration time are enforced on
/// every sub-agent in the spawn chain, regardless of nesting depth.
///
/// When a sub-agent spawns its own sub-agents it must forward these fields downward so
/// that grandchild agents cannot silently receive more privileges than the original
/// orchestration policy allowed.
///
/// # Examples
///
/// ```rust
/// use zeph_subagent::manager::SpawnContext;
///
/// // Minimal context — all fields use their defaults.
/// let ctx = SpawnContext::default();
/// assert!(ctx.parent_messages.is_empty());
/// assert_eq!(ctx.spawn_depth, 0);
/// assert!(ctx.max_trust_level.is_none());
/// assert!(ctx.inherited_tool_allowlist.is_none());
/// ```
#[derive(Default)]
pub struct SpawnContext {
    /// Recent parent conversation messages (last N turns).
    pub parent_messages: Vec<Message>,
    /// Parent's cancellation token for linked cancellation (foreground spawns).
    pub parent_cancel: Option<CancellationToken>,
    /// Parent's active provider name (for context propagation).
    pub parent_provider_name: Option<String>,
    /// Current spawn depth (0 = top-level agent).
    pub spawn_depth: u32,
    /// MCP tool names available in the parent's tool executor (for diagnostics).
    pub mcp_tool_names: Vec<String>,
    /// Seeded trajectory risk score from the parent sentinel (spec 050 §4).
    ///
    /// When `Some`, the subagent's `TrajectorySentinel` starts with this pre-seeded score
    /// rather than `0.0`, preventing a subagent spawn from acting as a free risk reset.
    /// The subagent loop applies this via `TrajectorySentinel::seed_score` after build.
    pub seed_trajectory_score: Option<f32>,
    /// Parent's content isolation config, propagated so the subagent loop can run the
    /// same sanitizer settings on hook-replaced tool output.
    pub content_isolation: ContentIsolationConfig,
    /// Name of the orchestrator that spawned this subagent.
    ///
    /// When set, the subagent's system prompt includes an identity header naming the
    /// orchestrator, so the subagent can validate that instructions are consistent with
    /// the expected authority.
    pub orchestrator_name: Option<String>,
    /// Role or task label of the orchestrating agent (e.g., `"planner"`, `"tool-router"`).
    ///
    /// Injected alongside [`orchestrator_name`][Self::orchestrator_name] when both are set.
    /// Omitted from the identity header when only `orchestrator_name` is provided.
    pub orchestrator_role: Option<String>,
    /// Per-session MCP servers to inject into this subagent's tool name annotations.
    ///
    /// The parent is responsible for connecting these servers and including them in the
    /// `tool_executor` passed to [`SubAgentManager::spawn`]. This field only carries the
    /// server metadata so the subagent's system prompt lists the additional tool names.
    pub session_mcp_servers: Vec<McpServerConfig>,
    /// Maximum trust level cap inherited from the parent agent or orchestration policy.
    ///
    /// When `Some(cap)`, the spawned sub-agent's effective trust level is clamped to
    /// `min(own_trust, cap)` so that sub-agents can never receive higher privileges than
    /// the orchestration policy originally allowed.
    ///
    /// # Caller responsibility for nested spawns
    ///
    /// This field does **not** propagate automatically. When a sub-agent itself spawns a
    /// grandchild, it must copy this field from its own received `SpawnContext` into the
    /// grandchild's `SpawnContext`. Passing `None` (the default) at that point means the
    /// grandchild receives **no cap**, which is a privilege escalation if the parent was
    /// constrained. Only the top-level session (spawned by `build_spawn_context`) correctly
    /// leaves this `None` — that represents an unconstrained top-level entry point.
    ///
    /// `None` means no cap is imposed by the parent (the sub-agent's own definition
    /// determines its trust level).
    pub max_trust_level: Option<SkillTrustLevel>,
    /// Tool names that this sub-agent is allowed to invoke, inherited from the parent.
    ///
    /// When `Some(set)`, the effective tool allowlist for the spawned agent is the
    /// intersection of `set` and the agent's own definition policy. This prevents a
    /// sub-agent from accessing tools that the parent is itself not allowed to use.
    ///
    /// # Caller responsibility for nested spawns
    ///
    /// Like [`max_trust_level`][Self::max_trust_level], this field does **not** propagate
    /// automatically. When a constrained sub-agent spawns its own children, it must copy
    /// this field from its received `SpawnContext` into the child's `SpawnContext`.
    /// Passing `None` at that point would grant the grandchild unrestricted tool access,
    /// defeating the original orchestration policy.
    ///
    /// `None` means no additional allowlist restriction is imposed by the parent
    /// (the agent's definition policy applies without narrowing).
    pub inherited_tool_allowlist: Option<HashSet<String>>,

    /// Durable resolver seat for promise-based subagent spawn/await (spec-064 §P4, INV-9).
    ///
    /// When `Some`, the spawned background task resolves the parent's durable promise after the
    /// agent loop terminates. The seat carries the resolver token and MUST NOT be forwarded to
    /// the child's tool executor or LLM surface — only the background task wrapper consumes it.
    ///
    /// `None` when `durable.enabled && durable.subagent` is false (plain spawn/collect path).
    pub durable_resolver: Option<DurableResolverSeat>,

    /// Deny network egress for this sub-agent's `bash` tool calls.
    ///
    /// Set by the orchestration layer when the spawning `TaskNode` carries
    /// `network_scope: NetworkScope::Deny` (spec `069-threat-model` OQ-1). When `true`,
    /// `build_filtered_executor` wraps the tool executor with
    /// [`NetworkDenyToolExecutor`](crate::NetworkDenyToolExecutor), which blocks `bash`
    /// invocations of `curl`, `wget`, `nc`, `ncat`, and `netcat` for this spawn only —
    /// sibling tasks and the parent agent's own executor are unaffected.
    ///
    /// # Caller responsibility for nested spawns
    ///
    /// Like [`max_trust_level`][Self::max_trust_level], this field does **not** propagate
    /// automatically. A sub-agent that itself spawns a grandchild must copy this field
    /// from its own received `SpawnContext` into the grandchild's `SpawnContext`, or the
    /// grandchild spawns with network access regardless of the original task's scope.
    ///
    /// `false` (the default) imposes no restriction beyond the executor/global
    /// `allow_network` default.
    pub network_denied: bool,

    /// Shared progress heartbeat for idle-timeout detection (issue #6245).
    ///
    /// Set by the orchestration driver (`handle_scheduler_spawn_action` in `zeph-core`'s
    /// `scheduler_loop.rs`) alongside [`network_denied`][Self::network_denied] — same
    /// post-construction assignment pattern, not part of `build_spawn_context`'s base
    /// literal. The driver creates the `Arc`, clones it in here, and keeps the original for
    /// `zeph_orchestration::DagScheduler::record_spawn`'s `last_progress_at` parameter so
    /// both the running loop and the scheduler observe the same counter.
    ///
    /// `None` (the default) for spawns not tracked by a `DagScheduler` — e.g. the standalone
    /// `/agent run` command — which are never idle-tracked.
    pub progress_at: Option<Arc<std::sync::atomic::AtomicU64>>,
}

/// Live status snapshot of a running sub-agent.
///
/// Values are updated by the background agent loop via a [`tokio::sync::watch`] channel.
/// Callers receive snapshots via [`SubAgentManager::statuses`].
#[derive(Debug, Clone)]
pub struct SubAgentStatus {
    /// Current lifecycle state of the agent task.
    pub state: SubAgentState,
    /// Last message content from the agent (trimmed for display).
    pub last_message: Option<String>,
    /// Number of LLM turns consumed so far.
    pub turns_used: u32,
    /// Monotonic timestamp recorded at spawn time.
    pub started_at: Instant,
}

/// Handle to a spawned sub-agent task, owned by [`SubAgentManager`].
///
/// Fields are public to allow test harnesses in downstream crates to construct handles
/// without going through the full spawn lifecycle. Production code must not mutate
/// grants or the cancellation state directly — use the [`SubAgentManager`] API instead.
///
/// The `Drop` implementation cancels the task and revokes all grants as a safety net.
pub struct SubAgentHandle {
    /// Short display ID (same as `task_id` for non-resumed sessions).
    pub id: String,
    /// The definition that was used to spawn this agent.
    pub def: SubAgentDef,
    /// UUID assigned at spawn time (currently identical to `id`; separated for future use).
    pub task_id: String,
    /// Cached state — may lag the background task by one watch broadcast.
    pub state: SubAgentState,
    /// Supervised handle for the background agent loop task.
    pub join_handle: Option<BlockingHandle<Result<String, SubAgentError>>>,
    /// Cancellation token; cancelled on [`SubAgentManager::cancel`] or drop.
    pub cancel: CancellationToken,
    /// Watch receiver for live status updates from the agent loop.
    pub status_rx: watch::Receiver<SubAgentStatus>,
    /// Zero-trust TTL-bounded grants for this agent session.
    pub grants: PermissionGrants,
    /// Receives secret requests from the sub-agent loop.
    pub pending_secret_rx: mpsc::Receiver<SecretRequest>,
    /// Delivers the approval outcome to the sub-agent loop: `None` = denied,
    /// `Some(value)` = approved, carrying the resolved vault secret value and its
    /// grant expiry so the loop can re-validate the TTL locally on every tool call.
    pub secret_tx: mpsc::Sender<Option<GrantedSecret>>,
    /// ISO 8601 UTC timestamp recorded when the agent was spawned or resumed.
    pub started_at_str: String,
    /// Resolved transcript directory at spawn time; `None` if transcripts were disabled.
    pub transcript_dir: Option<PathBuf>,
    /// MCP tool names available at spawn time, persisted for transcript meta on collect.
    pub mcp_tool_names: Vec<String>,
}

impl SubAgentHandle {
    /// Construct a minimal [`SubAgentHandle`] for use in unit tests.
    ///
    /// The returned handle has a no-op cancel token, closed channels, and no grants.
    /// It must not be spawned or collected — it is only valid for inspection logic
    /// that operates on the handle's metadata fields (id, def, state, etc.).
    #[cfg(test)]
    pub fn for_test(id: impl Into<String>, def: SubAgentDef) -> Self {
        let initial_status = SubAgentStatus {
            state: SubAgentState::Working,
            last_message: None,
            turns_used: 0,
            started_at: Instant::now(),
        };
        let (status_tx, status_rx) = watch::channel(initial_status);
        drop(status_tx);
        let (pending_secret_rx_tx, pending_secret_rx) = mpsc::channel(1);
        drop(pending_secret_rx_tx);
        let (secret_tx, _) = mpsc::channel(1);
        let id_str = id.into();
        Self {
            task_id: id_str.clone(),
            id: id_str,
            def,
            state: SubAgentState::Working,
            join_handle: None,
            cancel: CancellationToken::new(),
            status_rx,
            grants: PermissionGrants::default(),
            pending_secret_rx,
            secret_tx,
            started_at_str: String::new(),
            transcript_dir: None,
            mcp_tool_names: Vec::new(),
        }
    }
}

impl std::fmt::Debug for SubAgentHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubAgentHandle")
            .field("id", &self.id)
            .field("task_id", &self.task_id)
            .field("state", &self.state)
            .field("def_name", &self.def.name)
            .finish_non_exhaustive()
    }
}

impl Drop for SubAgentHandle {
    fn drop(&mut self) {
        // Defense-in-depth: cancel the task and revoke grants on drop even if
        // cancel() or collect() was not called (e.g., on panic or early return).
        self.cancel.cancel();
        if !self.grants.is_empty_grants() {
            tracing::warn!(
                id = %self.id,
                "SubAgentHandle dropped without explicit cleanup — revoking grants"
            );
        }
        self.grants.revoke_all();
    }
}

/// Manages sub-agent lifecycle: definitions, spawning, cancellation, and result collection.
///
/// `SubAgentManager` is the central coordinator for all sub-agent tasks. It tracks active
/// [`SubAgentHandle`]s, enforces the global concurrency limit, and stores loaded
/// [`SubAgentDef`]s.
///
/// # Concurrency model
///
/// The concurrency limit counts agents whose [`SubAgentState`] is `Submitted` or `Working`.
/// Reserved slots (via [`reserve_slots`][Self::reserve_slots]) also count against this limit
/// to allow orchestration schedulers to guarantee capacity before spawning.
///
/// # Examples
///
/// ```rust
/// use zeph_subagent::SubAgentManager;
///
/// let manager = SubAgentManager::new(4);
/// assert_eq!(manager.definitions().len(), 0);
/// ```
pub struct SubAgentManager {
    definitions: Vec<SubAgentDef>,
    agents: HashMap<String, SubAgentHandle>,
    max_concurrent: usize,
    /// Number of slots soft-reserved by the orchestration scheduler.
    ///
    /// Reserved slots count against the concurrency limit so that the scheduler can
    /// guarantee capacity for tasks it is about to spawn, preventing a planning-phase
    /// sub-agent from exhausting the pool and causing a deadlock.
    reserved_slots: usize,
    /// Config-level `SubagentStop` hooks, cached so `cancel()` and `collect()` can fire them.
    stop_hooks: Vec<super::hooks::HookDef>,
    /// Directory for JSONL transcripts and meta sidecars.
    transcript_dir: Option<PathBuf>,
    /// Maximum number of transcript files to keep (0 = unlimited).
    transcript_max_files: usize,
    /// Optional fleet registry for registering sub-agents in the fleet dashboard.
    ///
    /// When `None`, fleet registration is skipped silently. Inject via
    /// [`set_fleet_registry`][Self::set_fleet_registry].
    fleet_registry: Option<SharedFleetRegistry>,
    /// Tracks fire-and-forget hook and fleet-registry tasks to prevent silent panic swallowing.
    ///
    /// Completed and panicked tasks are drained before each new spawn. On graceful shutdown,
    /// [`shutdown_all`][Self::shutdown_all] aborts all outstanding tasks via
    /// [`JoinSet::shutdown`].
    hook_tasks: JoinSet<()>,
    /// Maximum number of concurrent hook tasks allowed in [`hook_tasks`][Self::hook_tasks].
    ///
    /// When the limit is reached, new fire-and-forget tasks are dropped with a warning instead
    /// of growing the set unboundedly under high-throughput spawning.
    max_hook_tasks: usize,
    /// Optional worktree manager; `Some` iff `worktree.enabled = true` in config.
    ///
    /// When set, every [`spawn`][Self::spawn] acquires [`cwd_lock`][Self::cwd_lock] for
    /// its full run so that plain agents cannot observe a stale cwd mutated by a worktree
    /// agent (INV-1). Only agents with `permissions.worktree = true` and a non-`None`
    /// `bg_isolation` actually get a dedicated worktree.
    ///
    /// This is the single live instance shared by the running agent's `/worktree`
    /// slash command (see [`worktree_manager`][Self::worktree_manager]) — distinct from
    /// the CLI's `zeph worktree list`/`clean`, which constructs its own fresh manager
    /// per invocation (`src/commands/worktree.rs`).
    worktree_manager: Option<Arc<zeph_worktree::DefaultWorktreeManager>>,
    /// Process-level serialisation mutex for working-directory mutations (INV-1).
    ///
    /// Acquired by every spawned task when `worktree_manager.is_some()`.  The
    /// `OwnedMutexGuard` is held for the full duration of `run_agent_loop` via the
    /// `CwdRestoreGuard` RAII wrapper.
    cwd_lock: Arc<tokio::sync::Mutex<()>>,
    /// Optional supervisor for subagent lifecycle tasks.
    ///
    /// When set, each spawned agent loop task is registered under its task ID so it is
    /// visible to TUI status panels and shutdown is coordinated through the supervisor.
    task_supervisor: Option<TaskSupervisor>,
}

impl std::fmt::Debug for SubAgentManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubAgentManager")
            .field("definitions_count", &self.definitions.len())
            .field("active_agents", &self.agents.len())
            .field("max_concurrent", &self.max_concurrent)
            .field("reserved_slots", &self.reserved_slots)
            .field("stop_hooks_count", &self.stop_hooks.len())
            .field("transcript_dir", &self.transcript_dir)
            .field("transcript_max_files", &self.transcript_max_files)
            .field("fleet_registry", &self.fleet_registry.is_some())
            .field("hook_tasks_len", &self.hook_tasks.len())
            .field("max_hook_tasks", &self.max_hook_tasks)
            .field("worktree_manager", &self.worktree_manager.is_some())
            .field("cwd_lock", &"<Mutex>")
            .field("task_supervisor", &self.task_supervisor.is_some())
            .finish()
    }
}

impl SubAgentManager {
    /// Create a new manager with the given concurrency limit.
    #[must_use]
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            definitions: Vec::new(),
            agents: HashMap::new(),
            max_concurrent,
            reserved_slots: 0,
            stop_hooks: Vec::new(),
            transcript_dir: None,
            transcript_max_files: 50,
            fleet_registry: None,
            hook_tasks: JoinSet::new(),
            max_hook_tasks: 64,
            worktree_manager: None,
            cwd_lock: Arc::new(tokio::sync::Mutex::new(())),
            task_supervisor: None,
        }
    }

    /// Inject a [`TaskSupervisor`] so subagent lifecycle tasks are registered and visible.
    ///
    /// Must be called before the first [`spawn`][Self::spawn]. When set, each spawned agent
    /// loop task is registered under its task ID and is observable in TUI status panels and
    /// [`TaskSupervisor::snapshot`].
    pub fn set_task_supervisor(&mut self, supervisor: TaskSupervisor) {
        self.task_supervisor = Some(supervisor);
    }

    /// Inject a [`DefaultWorktreeManager`][zeph_worktree::DefaultWorktreeManager] into the
    /// manager.
    ///
    /// Must be called at most once, before the first [`spawn`][Self::spawn].  When set,
    /// every spawned task acquires the process-level cwd mutex (INV-1) and agents with
    /// `permissions.worktree = true` receive a dedicated git worktree.
    pub fn set_worktree_manager(&mut self, wm: Arc<zeph_worktree::DefaultWorktreeManager>) {
        self.worktree_manager = Some(wm);
    }

    /// Returns the live worktree manager, if the worktree subsystem is enabled for this
    /// session.
    ///
    /// This is the same instance [`spawn`][Self::spawn] uses to create per-subagent
    /// worktrees, so callers (e.g. the `/worktree` slash command) observe this session's
    /// actual live state rather than a fresh disk scan. Its own
    /// `prune_branch_on_remove()` reflects `WorktreeConfig::prune_branch_on_remove` — no
    /// need to retain a separate copy on `SubAgentManager`.
    #[must_use]
    pub fn worktree_manager(&self) -> Option<&Arc<zeph_worktree::DefaultWorktreeManager>> {
        self.worktree_manager.as_ref()
    }

    /// Drain completed hook tasks and spawn a new one if below the limit.
    ///
    /// Polls [`hook_tasks`][Self::hook_tasks] for finished entries so the set does not
    /// accumulate stale handles. When the set is at capacity, logs a warning and skips
    /// the spawn rather than growing unboundedly.
    fn spawn_hook_task<F>(&mut self, future: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        // Drain completed/panicked tasks before checking capacity.
        while self.hook_tasks.try_join_next().is_some() {}
        if self.hook_tasks.len() >= self.max_hook_tasks {
            tracing::warn!(
                limit = self.max_hook_tasks,
                "hook task limit reached — dropping fire-and-forget task"
            );
            return;
        }
        self.hook_tasks.spawn(future);
    }

    /// Spawns a named subagent task under the session [`TaskSupervisor`] if one is configured,
    /// making the task visible in TUI status and abortable on shutdown via
    /// [`TaskSupervisor::shutdown_all`].
    ///
    /// Falls back to a transient local supervisor when no session supervisor has been wired via
    /// [`SubAgentManager::set_task_supervisor`] — the task runs but is not tracked globally.
    /// The returned [`BlockingHandle`] type is identical in both cases so call sites are uniform.
    ///
    /// Every agent loop task's future resolves to a `Result<T, E>` (in practice always
    /// `Result<String, SubAgentError>`), so this classifies via
    /// [`TaskSupervisor::spawn_oneshot_classified`] rather than plain `spawn_oneshot` — an
    /// `Err` produced by a genuinely completed task (e.g. a worktree-quota or cwd-guard setup
    /// failure returned before the agent loop ever starts) is thus classified and logged as a
    /// supervisor-level failure instead of a normal completion (#6257).
    pub(crate) fn spawn_agent_task<F, Fut, T, E>(
        &self,
        name: Arc<str>,
        factory: F,
    ) -> BlockingHandle<Result<T, E>>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<T, E>> + Send + 'static,
        T: Send + 'static,
        E: Send + 'static,
    {
        if let Some(ref sup) = self.task_supervisor {
            sup.spawn_oneshot_classified(name, factory, Result::is_ok)
        } else {
            let local = TaskSupervisor::new(CancellationToken::new());
            local.spawn_oneshot_classified(name, factory, Result::is_ok)
        }
    }

    /// Reserve `n` concurrency slots for the orchestration scheduler.
    ///
    /// Reserved slots count against the concurrency limit in [`spawn`](Self::spawn) so that
    /// the scheduler can guarantee capacity for tasks it is about to launch. Call
    /// [`release_reservation`](Self::release_reservation) when the scheduler finishes.
    pub fn reserve_slots(&mut self, n: usize) {
        self.reserved_slots = self.reserved_slots.saturating_add(n);
    }

    /// Release `n` previously reserved concurrency slots.
    pub fn release_reservation(&mut self, n: usize) {
        self.reserved_slots = self.reserved_slots.saturating_sub(n);
    }

    /// Configure transcript storage settings.
    pub fn set_transcript_config(&mut self, dir: Option<PathBuf>, max_files: usize) {
        self.transcript_dir = dir;
        self.transcript_max_files = max_files;
    }

    /// Set config-level lifecycle stop hooks (fired when any agent finishes or is cancelled).
    pub fn set_stop_hooks(&mut self, hooks: Vec<super::hooks::HookDef>) {
        self.stop_hooks = hooks;
    }

    /// Inject a fleet registry so spawned sub-agents appear in the fleet dashboard.
    ///
    /// When set, [`spawn`][Self::spawn] registers the session as `Active` and
    /// [`collect`][Self::collect] / [`cancel`][Self::cancel] mark it terminal.
    /// Errors from the registry are logged at `warn` level and never propagate to callers.
    pub fn set_fleet_registry(&mut self, registry: SharedFleetRegistry) {
        self.fleet_registry = Some(registry);
    }

    /// Load sub-agent definitions from the given directories.
    ///
    /// Higher-priority directories should appear first. Name conflicts are resolved
    /// by keeping the first occurrence. Non-existent directories are silently skipped.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError`] if any definition file fails to parse.
    pub fn load_definitions(&mut self, dirs: &[PathBuf]) -> Result<(), SubAgentError> {
        let defs = SubAgentDef::load_all(dirs)?;

        // Security gate: non-Default permission_mode is forbidden when the user-level
        // agents directory (~/.zeph/agents/) is one of the load sources. This prevents
        // a crafted agent file from escalating its own privileges.
        // Validation happens here (in the manager) because this is the only place
        // that has full context about which directories were searched.
        //
        // FIX-5: fail-closed — if user_agents_dir is in dirs and a definition has
        // non-Default permission_mode, we cannot verify it did not originate from the
        // user-level dir (SubAgentDef no longer stores source_path), so we reject it.
        let user_agents_dir = dirs::home_dir().map(|h| h.join(".zeph").join("agents"));
        let loads_user_dir = user_agents_dir.as_ref().is_some_and(|user_dir| {
            // FIX-8: log and treat as non-user-level if canonicalize fails.
            match std::fs::canonicalize(user_dir) {
                Ok(canonical_user) => dirs
                    .iter()
                    .filter_map(|d| std::fs::canonicalize(d).ok())
                    .any(|d| d == canonical_user),
                Err(e) => {
                    tracing::warn!(
                        dir = %user_dir.display(),
                        error = %e,
                        "could not canonicalize user agents dir, treating as non-user-level"
                    );
                    false
                }
            }
        });

        if loads_user_dir {
            for def in &defs {
                if def.permissions.permission_mode != PermissionMode::Default {
                    return Err(SubAgentError::Invalid(format!(
                        "sub-agent '{}': non-default permission_mode is not allowed for \
                         user-level definitions (~/.zeph/agents/)",
                        def.name
                    )));
                }
            }
        }

        self.definitions = defs;
        tracing::info!(
            count = self.definitions.len(),
            "sub-agent definitions loaded"
        );
        Ok(())
    }

    /// Load definitions with full scope context for source tracking and security checks.
    ///
    /// The blocking filesystem scan runs on a dedicated thread via
    /// `tokio::task::spawn_blocking` so the tokio worker thread is not stalled (#5108).
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError`] if a CLI-sourced definition file fails to parse.
    #[tracing::instrument(name = "subagent.manager.load_definitions_with_sources", skip_all)]
    pub async fn load_definitions_with_sources(
        &mut self,
        ordered_paths: &[PathBuf],
        cli_agents: &[PathBuf],
        config_user_dir: Option<&PathBuf>,
        extra_dirs: &[PathBuf],
    ) -> Result<(), SubAgentError> {
        // Clone inputs so they can be moved into spawn_blocking ('static bound).
        let ordered = ordered_paths.to_vec();
        let cli = cli_agents.to_vec();
        let user_dir = config_user_dir.cloned();
        let extra = extra_dirs.to_vec();

        let defs = tokio::task::spawn_blocking(move || {
            SubAgentDef::load_all_with_sources(&ordered, &cli, user_dir.as_ref(), &extra)
        })
        .await
        .map_err(|e| SubAgentError::TaskPanic(format!("load_definitions_with_sources: {e}")))?;

        self.definitions = defs?;
        tracing::info!(
            count = self.definitions.len(),
            "sub-agent definitions loaded"
        );
        Ok(())
    }

    /// Return all loaded definitions.
    #[must_use]
    pub fn definitions(&self) -> &[SubAgentDef] {
        &self.definitions
    }

    /// Return mutable access to the loaded definitions list.
    ///
    /// Intended for test harnesses and dynamic definition registration. Production code
    /// should prefer [`load_definitions`][Self::load_definitions].
    pub fn definitions_mut(&mut self) -> &mut Vec<SubAgentDef> {
        &mut self.definitions
    }

    /// Insert a pre-built handle directly into the active agents map.
    ///
    /// Used in tests to simulate an agent that has already run and left a pending secret
    /// request in its channel without going through the full spawn lifecycle.
    pub fn insert_handle_for_test(&mut self, id: String, handle: SubAgentHandle) {
        self.agents.insert(id, handle);
    }
}

#[cfg(test)]
mod tests;
