use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeph_a2a::types::TaskState;

use super::channel::{AgentHalf, OrchestratorHalf, new_channel};
use super::def::SubAgentDef;
use super::error::SubAgentError;
use super::grants::PermissionGrants;

const CHANNEL_BUFFER: usize = 32;

/// Live status of a running sub-agent.
#[derive(Debug, Clone)]
pub struct SubAgentStatus {
    pub state: TaskState,
    pub last_message: Option<String>,
    pub turns_used: u32,
    pub started_at: Instant,
}

/// Handle to a spawned sub-agent task.
///
/// Fields are `pub(crate)` to prevent external code from bypassing the manager's
/// audit trail by mutating grants or cancellation state directly.
pub struct SubAgentHandle {
    pub(crate) id: String,
    pub(crate) def: SubAgentDef,
    /// A2A task ID — currently the same UUID as `id`; separated for future
    /// compatibility when sub-agents may have distinct internal/external IDs.
    pub(crate) task_id: String,
    pub(crate) state: TaskState,
    pub(crate) join_handle: Option<JoinHandle<anyhow::Result<String>>>,
    pub(crate) cancel: CancellationToken,
    pub(crate) status_rx: watch::Receiver<SubAgentStatus>,
    pub(crate) grants: PermissionGrants,
    /// Orchestrator-side channel half for communicating with the sub-agent.
    #[allow(dead_code)]
    pub(crate) channel: OrchestratorHalf,
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
pub struct SubAgentManager {
    definitions: Vec<SubAgentDef>,
    agents: HashMap<String, SubAgentHandle>,
    max_concurrent: usize,
}

impl std::fmt::Debug for SubAgentManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubAgentManager")
            .field("definitions_count", &self.definitions.len())
            .field("active_agents", &self.agents.len())
            .field("max_concurrent", &self.max_concurrent)
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
        }
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
        self.definitions = SubAgentDef::load_all(dirs)?;
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

    /// Spawn a sub-agent by definition name.
    ///
    /// Returns the `task_id` (UUID string) that can be used with [`cancel`](Self::cancel)
    /// and [`collect`](Self::collect).
    ///
    /// # Note
    ///
    /// The actual agent execution loop is a stub that waits for cancellation.
    /// It will be wired to the real provider + tools in a future phase (M28-E).
    /// The intended signature at that point is:
    /// `spawn(&mut self, def_name, task_prompt, provider: AnyProvider, tools: Box<dyn ErasedToolExecutor>)`
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::NotFound`] if no definition with the given name exists,
    /// [`SubAgentError::Spawn`] if the concurrency limit is exceeded.
    pub fn spawn(&mut self, def_name: &str, _task_prompt: &str) -> Result<String, SubAgentError> {
        let def = self
            .definitions
            .iter()
            .find(|d| d.name == def_name)
            .cloned()
            .ok_or_else(|| SubAgentError::NotFound(def_name.to_owned()))?;

        let active = self
            .agents
            .values()
            .filter(|h| matches!(h.state, TaskState::Working | TaskState::Submitted))
            .count();

        if active >= self.max_concurrent {
            return Err(SubAgentError::Spawn(format!(
                "concurrency limit {max} reached",
                max = self.max_concurrent
            )));
        }

        let task_id = Uuid::new_v4().to_string();
        let cancel = CancellationToken::new();
        let (orch_half, agent_half): (OrchestratorHalf, AgentHalf) = new_channel(CHANNEL_BUFFER);

        let started_at = Instant::now();
        let initial_status = SubAgentStatus {
            state: TaskState::Submitted,
            last_message: None,
            turns_used: 0,
            started_at,
        };
        let (status_tx, status_rx) = watch::channel(initial_status);

        // Stub execution task — real agent loop wired in future phase.
        // `agent_half` is passed into the closure so it is available when the
        // real loop is implemented; dropping it here would silently break comms.
        let cancel_clone = cancel.clone();
        let join_handle: JoinHandle<anyhow::Result<String>> = tokio::spawn(async move {
            // Keep agent_half alive for the duration of the task.
            let _agent_channel = agent_half;
            let _ = status_tx.send(SubAgentStatus {
                state: TaskState::Working,
                last_message: None,
                turns_used: 0,
                started_at, // reuse the original started_at — no drift
            });
            cancel_clone.cancelled().await;
            Ok(String::new())
        });

        let handle = SubAgentHandle {
            id: task_id.clone(),
            def,
            task_id: task_id.clone(),
            state: TaskState::Submitted,
            join_handle: Some(join_handle),
            cancel,
            status_rx,
            grants: PermissionGrants::default(),
            channel: orch_half,
        };

        self.agents.insert(task_id.clone(), handle);
        tracing::info!(task_id, def_name, "sub-agent spawned");
        Ok(task_id)
    }

    /// Cancel a running sub-agent by task ID.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::NotFound`] if the task ID is unknown.
    pub fn cancel(&mut self, task_id: &str) -> Result<(), SubAgentError> {
        let handle = self
            .agents
            .get_mut(task_id)
            .ok_or_else(|| SubAgentError::NotFound(task_id.to_owned()))?;
        handle.cancel.cancel();
        handle.state = TaskState::Canceled;
        handle.grants.revoke_all();
        tracing::info!(task_id, "sub-agent cancelled");
        Ok(())
    }

    /// Approve a secret request for a running sub-agent.
    ///
    /// Called after the user approves a vault secret access prompt. The secret
    /// key must appear in the sub-agent definition's allowed `secrets` list;
    /// otherwise the request is auto-denied.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::NotFound`] if the task ID is unknown,
    /// [`SubAgentError::Invalid`] if the key is not in the definition's allowed list.
    pub fn approve_secret(
        &mut self,
        task_id: &str,
        secret_key: &str,
        ttl: std::time::Duration,
    ) -> Result<(), SubAgentError> {
        let handle = self
            .agents
            .get_mut(task_id)
            .ok_or_else(|| SubAgentError::NotFound(task_id.to_owned()))?;

        // Sweep stale grants before adding a new one for consistent housekeeping.
        handle.grants.sweep_expired();

        if !handle
            .def
            .permissions
            .secrets
            .iter()
            .any(|k| k == secret_key)
        {
            // Do not log the key name at warn level — only log that a request was denied.
            tracing::warn!(task_id, "secret request denied: key not in allowed list");
            return Err(SubAgentError::Invalid(format!(
                "secret is not in the allowed secrets list for '{}'",
                handle.def.name
            )));
        }

        handle.grants.grant_secret(secret_key, ttl);
        Ok(())
    }

    /// Collect the result from a completed sub-agent, removing it from the active set.
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::NotFound`] if the task ID is unknown,
    /// [`SubAgentError::Spawn`] if the task panicked.
    pub async fn collect(&mut self, task_id: &str) -> Result<String, SubAgentError> {
        let mut handle = self
            .agents
            .remove(task_id)
            .ok_or_else(|| SubAgentError::NotFound(task_id.to_owned()))?;

        handle.grants.revoke_all();

        if let Some(jh) = handle.join_handle.take() {
            let result = jh.await.map_err(|e| SubAgentError::Spawn(e.to_string()))?;
            result.map_err(|e| SubAgentError::Spawn(e.to_string()))
        } else {
            Ok(String::new())
        }
    }

    /// Return a snapshot of all active sub-agent statuses.
    #[must_use]
    pub fn statuses(&self) -> Vec<(String, SubAgentStatus)> {
        self.agents
            .values()
            .map(|h| (h.task_id.clone(), h.status_rx.borrow().clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_manager() -> SubAgentManager {
        SubAgentManager::new(4)
    }

    fn sample_def() -> SubAgentDef {
        SubAgentDef::parse("+++\nname = \"bot\"\ndescription = \"A bot\"\n+++\n\nDo things.\n")
            .unwrap()
    }

    fn def_with_secrets() -> SubAgentDef {
        SubAgentDef::parse(
            "+++\nname = \"bot\"\ndescription = \"A bot\"\n[permissions]\nsecrets = [\"api-key\"]\n+++\n\nDo things.\n",
        )
        .unwrap()
    }

    #[test]
    fn load_definitions_populates_vec() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let content = "+++\nname = \"helper\"\ndescription = \"A helper\"\n+++\n\nHelp.\n";
        let mut f = std::fs::File::create(dir.path().join("helper.md")).unwrap();
        f.write_all(content.as_bytes()).unwrap();

        let mut mgr = make_manager();
        mgr.load_definitions(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(mgr.definitions().len(), 1);
        assert_eq!(mgr.definitions()[0].name, "helper");
    }

    #[test]
    fn spawn_not_found_error() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut mgr = make_manager();
        let err = mgr.spawn("nonexistent", "prompt").unwrap_err();
        assert!(matches!(err, SubAgentError::NotFound(_)));
    }

    #[test]
    fn spawn_and_cancel() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());

        let task_id = mgr.spawn("bot", "do stuff").unwrap();
        assert!(!task_id.is_empty());

        mgr.cancel(&task_id).unwrap();
        assert_eq!(mgr.agents[&task_id].state, TaskState::Canceled);
    }

    #[test]
    fn cancel_unknown_task_id_returns_not_found() {
        let mut mgr = make_manager();
        let err = mgr.cancel("unknown-id").unwrap_err();
        assert!(matches!(err, SubAgentError::NotFound(_)));
    }

    #[tokio::test]
    async fn collect_removes_agent() {
        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());

        let task_id = mgr.spawn("bot", "do stuff").unwrap();
        mgr.cancel(&task_id).unwrap();

        let result = mgr.collect(&task_id).await.unwrap();
        assert!(result.is_empty());
        assert!(!mgr.agents.contains_key(&task_id));
    }

    #[tokio::test]
    async fn collect_unknown_task_id_returns_not_found() {
        let mut mgr = make_manager();
        let err = mgr.collect("unknown-id").await.unwrap_err();
        assert!(matches!(err, SubAgentError::NotFound(_)));
    }

    #[test]
    fn approve_secret_grants_access() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut mgr = make_manager();
        mgr.definitions.push(def_with_secrets());

        let task_id = mgr.spawn("bot", "work").unwrap();
        mgr.approve_secret(&task_id, "api-key", std::time::Duration::from_secs(60))
            .unwrap();

        let handle = mgr.agents.get_mut(&task_id).unwrap();
        assert!(
            handle
                .grants
                .is_active(&crate::subagent::GrantKind::Secret("api-key".into()))
        );
    }

    #[test]
    fn approve_secret_denied_for_unlisted_key() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut mgr = make_manager();
        mgr.definitions.push(sample_def()); // no secrets in allowed list

        let task_id = mgr.spawn("bot", "work").unwrap();
        let err = mgr
            .approve_secret(&task_id, "not-allowed", std::time::Duration::from_secs(60))
            .unwrap_err();
        assert!(matches!(err, SubAgentError::Invalid(_)));
    }

    #[test]
    fn approve_secret_unknown_task_id_returns_not_found() {
        let mut mgr = make_manager();
        let err = mgr
            .approve_secret("unknown", "key", std::time::Duration::from_secs(60))
            .unwrap_err();
        assert!(matches!(err, SubAgentError::NotFound(_)));
    }

    #[test]
    fn statuses_returns_active_agents() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());

        let task_id = mgr.spawn("bot", "work").unwrap();
        let statuses = mgr.statuses();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].0, task_id);
    }

    #[test]
    fn concurrency_limit_enforced() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut mgr = SubAgentManager::new(1);
        mgr.definitions.push(sample_def());

        let _first = mgr.spawn("bot", "first").unwrap();
        let err = mgr.spawn("bot", "second").unwrap_err();
        assert!(matches!(err, SubAgentError::Spawn(_)));
    }

    #[test]
    fn debug_impl_does_not_expose_sensitive_fields() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut mgr = make_manager();
        mgr.definitions.push(def_with_secrets());
        let task_id = mgr.spawn("bot", "work").unwrap();
        let handle = &mgr.agents[&task_id];
        let debug_str = format!("{handle:?}");
        // SubAgentHandle Debug must not expose grant contents or secrets
        assert!(!debug_str.contains("api-key"));
    }
}
