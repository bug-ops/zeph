// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;

use zeph_config::SubAgentConfig;

use super::SubAgentManager;
use super::SubAgentStatus;
use crate::def::SubAgentDef;
use crate::error::SubAgentError;
use crate::fleet::FleetSessionStatus;
use crate::hooks::fire_hooks;
use crate::manager::secrets::make_hook_env;
use crate::state::SubAgentState;
use crate::transcript::{TranscriptMeta, TranscriptWriter, sweep_old_transcripts};

impl SubAgentManager {
    /// Collect the result from a completed sub-agent, removing it from the active set.
    ///
    /// Writes a final `TranscriptMeta` sidecar with the terminal state and turn count.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::NotFound`] if the task ID is unknown,
    /// [`SubAgentError::Spawn`] if the task panicked.
    #[tracing::instrument(name = "subagent.manager.collect", skip_all, fields(task_id = task_id))]
    pub async fn collect(&mut self, task_id: &str) -> Result<String, SubAgentError> {
        let mut handle = self
            .agents
            .remove(task_id)
            .ok_or_else(|| SubAgentError::NotFound(task_id.to_owned()))?;

        if !self.stop_hooks.is_empty() {
            let stop_hooks = self.stop_hooks.clone();
            let stop_env = make_hook_env(task_id, &handle.def.name, "");
            self.spawn_hook_task(async move {
                if let Err(e) = fire_hooks(&stop_hooks, &stop_env, None, None).await {
                    tracing::warn!(error = %e, "SubagentStop hook failed");
                }
            });
        }

        handle.grants.revoke_all();

        // Flatten the outer `BlockingError` (panic/abort of the supervised task itself)
        // into `result` rather than propagating it with `?`: an early return here would
        // skip the fleet `mark_terminal` call and final `TranscriptMeta` write below,
        // leaving both stuck showing the agent as still active even though it has just
        // been removed from `self.agents` (issue #6408).
        let result: Result<String, SubAgentError> = if let Some(jh) = handle.join_handle.take() {
            match jh.join().await {
                Ok(inner) => inner,
                Err(e) => Err(SubAgentError::Spawn(e.to_string())),
            }
        } else {
            Ok(String::new())
        };

        let final_state = {
            let status = handle.status_rx.borrow();
            if result.is_err() {
                SubAgentState::Failed
            } else if status.state == SubAgentState::Canceled {
                SubAgentState::Canceled
            } else {
                SubAgentState::Completed
            }
        };

        if let Some(ref registry) = self.fleet_registry {
            let registry = std::sync::Arc::clone(registry);
            let tid = task_id.to_owned();
            let fleet_status = match final_state {
                SubAgentState::Failed => FleetSessionStatus::Failed,
                SubAgentState::Canceled => FleetSessionStatus::Cancelled,
                _ => FleetSessionStatus::Completed,
            };
            self.spawn_hook_task(async move {
                if let Err(e) = registry.mark_terminal(&tid, fleet_status).await {
                    tracing::warn!(error = %e, task_id = %tid, "fleet: mark_terminal failed");
                }
            });
        }

        if let Some(ref dir) = handle.transcript_dir.clone() {
            let turns_used = handle.status_rx.borrow().turns_used;
            let meta = TranscriptMeta {
                agent_id: task_id.to_owned(),
                agent_name: handle.def.name.clone(),
                def_name: handle.def.name.clone(),
                status: final_state,
                started_at: handle.started_at_str.clone(),
                finished_at: Some(crate::transcript::utc_now()),
                resumed_from: None,
                turns_used,
                mcp_tool_names: handle.mcp_tool_names.clone(),
            };
            if let Err(e) = TranscriptWriter::write_meta_async(dir, task_id, &meta).await {
                tracing::warn!(error = %e, task_id, "failed to write final transcript meta");
            }
        }

        result
    }

    /// Resolve the effective transcript directory from config or default.
    pub(crate) fn effective_transcript_dir(&self, config: &SubAgentConfig) -> PathBuf {
        if let Some(ref dir) = self.transcript_dir {
            dir.clone()
        } else if let Some(ref dir) = config.transcript_dir {
            dir.clone()
        } else {
            PathBuf::from(".zeph/subagents")
        }
    }

    /// Look up the definition name for a resumable transcript without spawning.
    ///
    /// Used by callers that need to resolve skills before calling `resume()`.
    /// Offloads the blocking FS reads to a `spawn_blocking` thread.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`crate::transcript::TranscriptReader::find_by_prefix`] and
    /// [`crate::transcript::TranscriptReader::load_meta`].
    pub async fn def_name_for_resume(
        &self,
        id_prefix: &str,
        config: &SubAgentConfig,
    ) -> Result<String, SubAgentError> {
        let dir = self.effective_transcript_dir(config);
        let id_prefix = id_prefix.to_owned();
        tokio::task::spawn_blocking(move || {
            let original_id =
                crate::transcript::TranscriptReader::find_by_prefix(&dir, &id_prefix)?;
            let meta = crate::transcript::TranscriptReader::load_meta(&dir, &original_id)?;
            Ok(meta.def_name)
        })
        .await
        .map_err(|e| SubAgentError::Spawn(format!("spawn_blocking panicked: {e}")))?
    }

    /// Return a snapshot of all active sub-agent statuses.
    #[must_use]
    pub fn statuses(&self) -> Vec<(String, SubAgentStatus)> {
        self.agents
            .values()
            .map(|h| {
                let mut status = h.status_rx.borrow().clone();
                if h.state == SubAgentState::Canceled {
                    status.state = SubAgentState::Canceled;
                }
                (h.task_id.clone(), status)
            })
            .collect()
    }

    /// Returns whether the background task backing `task_id` has finished at the runtime
    /// level, independent of whether its `status_rx` channel ever published a terminal
    /// [`SubAgentState`].
    ///
    /// A code path that exits `run_agent_loop` without sending a terminal status first —
    /// most notably a panic — leaves `status_rx` stuck on the last observed state (typically
    /// `Working`) forever. Callers such as `collect_finished_subagents` in `zeph-core` use
    /// this as a defense-in-depth reap signal for that case (issue #6408).
    ///
    /// Returns `false` for an unknown `task_id` or a handle with no `join_handle` (already
    /// collected, or a test-constructed handle).
    #[must_use]
    pub fn is_task_finished(&self, task_id: &str) -> bool {
        self.agents
            .get(task_id)
            .and_then(|h| h.join_handle.as_ref())
            .is_some_and(zeph_common::task_supervisor::BlockingHandle::is_finished)
    }

    /// Return the definition for a specific agent by `task_id`.
    #[must_use]
    pub fn agents_def(&self, task_id: &str) -> Option<&SubAgentDef> {
        self.agents.get(task_id).map(|h| &h.def)
    }

    /// Return the transcript directory for a specific agent by `task_id`.
    #[must_use]
    pub fn agent_transcript_dir(&self, task_id: &str) -> Option<&std::path::Path> {
        self.agents
            .get(task_id)
            .and_then(|h| h.transcript_dir.as_deref())
    }

    /// Resolve the transcript file path for `agent_id` from `config`, independent of whether the
    /// agent's handle is still resident in this manager.
    ///
    /// Unlike [`Self::agent_transcript_dir`] (which only returns a path for agents still tracked
    /// in `self.agents`), this is safe to call after [`Self::collect`] has already removed the
    /// handle — the path is fully determined by `config` and `agent_id`, matching exactly what
    /// `handle.transcript_dir` held at spawn time (see the `handle_transcript_dir` construction
    /// in `manager/spawn.rs`).
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_config::SubAgentConfig;
    /// use zeph_subagent::SubAgentManager;
    ///
    /// let mgr = SubAgentManager::new(4);
    /// let config = SubAgentConfig::default();
    /// let path = mgr.transcript_path_for(&config, "task-123");
    /// assert!(path.ends_with("task-123.jsonl"));
    /// ```
    #[must_use]
    pub fn transcript_path_for(&self, config: &SubAgentConfig, agent_id: &str) -> PathBuf {
        self.effective_transcript_dir(config)
            .join(format!("{agent_id}.jsonl"))
    }

    /// Create a transcript writer if transcripts are enabled.
    ///
    /// All three blocking FS operations (sweep, file open, meta write) are offloaded via
    /// [`tokio::task::block_in_place`] on multi-thread runtimes so the Tokio executor
    /// thread is not stalled. Falls back to a direct call on `current_thread` runtimes
    /// (e.g. single-threaded unit tests) where `block_in_place` would panic.
    pub(crate) fn create_transcript_writer(
        &mut self,
        config: &SubAgentConfig,
        task_id: &str,
        agent_name: &str,
        resumed_from: Option<&str>,
    ) -> Option<TranscriptWriter> {
        if !config.transcript_enabled {
            return None;
        }
        let dir = self.effective_transcript_dir(config);
        let max_files = self.transcript_max_files;
        let path = dir.join(format!("{task_id}.jsonl"));
        let meta = TranscriptMeta {
            agent_id: task_id.to_owned(),
            agent_name: agent_name.to_owned(),
            def_name: agent_name.to_owned(),
            status: SubAgentState::Submitted,
            started_at: crate::transcript::utc_now(),
            finished_at: None,
            resumed_from: resumed_from.map(str::to_owned),
            turns_used: 0,
            mcp_tool_names: Vec::new(),
        };
        let task_id = task_id.to_owned();
        run_blocking(move || {
            if max_files > 0
                && let Err(e) = sweep_old_transcripts(&dir, max_files)
            {
                tracing::warn!(error = %e, "transcript sweep failed");
            }
            match TranscriptWriter::new(&path) {
                Ok(w) => {
                    if let Err(e) = TranscriptWriter::write_meta(&dir, &task_id, &meta) {
                        tracing::warn!(error = %e, "failed to write initial transcript meta");
                    }
                    Some(w)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to create transcript writer");
                    None
                }
            }
        })
    }
}

/// Run a blocking closure without stalling the Tokio executor.
///
/// On a multi-thread runtime, delegates to [`tokio::task::block_in_place`] so other
/// tasks can continue running while the blocking work executes. On a `current_thread`
/// runtime (unit tests, single-threaded entry points) calls the closure directly,
/// since there is no thread pool to offload to and `block_in_place` would panic.
fn run_blocking<T>(f: impl FnOnce() -> T) -> T {
    if tokio::runtime::Handle::try_current()
        .is_ok_and(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
    {
        tokio::task::block_in_place(f)
    } else {
        f()
    }
}
