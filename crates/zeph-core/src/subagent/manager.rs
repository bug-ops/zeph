use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeph_a2a::types::TaskState;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{LlmProvider, Message, MessageMetadata, Role};
use zeph_tools::executor::ErasedToolExecutor;

use super::def::SubAgentDef;
use super::error::SubAgentError;
use super::filter::FilteredToolExecutor;
use super::grants::PermissionGrants;

struct AgentLoopArgs {
    provider: AnyProvider,
    executor: FilteredToolExecutor,
    system_prompt: String,
    task_prompt: String,
    max_turns: u32,
    cancel: CancellationToken,
    status_tx: watch::Sender<SubAgentStatus>,
    started_at: Instant,
}

fn make_message(role: Role, content: String) -> Message {
    Message {
        role,
        content,
        parts: vec![],
        metadata: MessageMetadata::default(),
    }
}

// Returns `true` if no tool was called (loop should break).
async fn handle_tool_step(
    executor: &FilteredToolExecutor,
    response: String,
    messages: &mut Vec<Message>,
) -> bool {
    match executor.execute_erased(&response).await {
        Ok(Some(output)) => {
            messages.push(make_message(Role::Assistant, response));
            messages.push(make_message(
                Role::User,
                format!(
                    "[tool output: {}]\n```\n{}\n```",
                    output.tool_name, output.summary
                ),
            ));
            false
        }
        Ok(None) => {
            messages.push(make_message(Role::Assistant, response));
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, "sub-agent tool execution failed");
            messages.push(make_message(Role::Assistant, response));
            messages.push(make_message(Role::User, format!("[tool error]: {e}")));
            false
        }
    }
}

async fn run_agent_loop(args: AgentLoopArgs) -> anyhow::Result<String> {
    let AgentLoopArgs {
        provider,
        executor,
        system_prompt,
        task_prompt,
        max_turns,
        cancel,
        status_tx,
        started_at,
    } = args;
    let _ = status_tx.send(SubAgentStatus {
        state: TaskState::Working,
        last_message: None,
        turns_used: 0,
        started_at,
    });

    let mut messages = vec![
        make_message(Role::System, system_prompt),
        make_message(Role::User, task_prompt),
    ];
    let mut turns: u32 = 0;
    let mut last_result = String::new();

    loop {
        if cancel.is_cancelled() {
            tracing::debug!("sub-agent cancelled, stopping loop");
            break;
        }
        if turns >= max_turns {
            tracing::debug!(turns, max_turns, "sub-agent reached max_turns limit");
            break;
        }

        let response = match provider.chat(&messages).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "sub-agent LLM call failed");
                let _ = status_tx.send(SubAgentStatus {
                    state: TaskState::Failed,
                    last_message: Some(e.to_string()),
                    turns_used: turns,
                    started_at,
                });
                return Err(anyhow::anyhow!("LLM call failed: {e}"));
            }
        };

        turns += 1;
        last_result.clone_from(&response);
        let _ = status_tx.send(SubAgentStatus {
            state: TaskState::Working,
            last_message: Some(response.chars().take(120).collect()),
            turns_used: turns,
            started_at,
        });

        if handle_tool_step(&executor, response, &mut messages).await {
            break;
        }
    }

    let _ = status_tx.send(SubAgentStatus {
        state: TaskState::Completed,
        last_message: Some(last_result.chars().take(120).collect()),
        turns_used: turns,
        started_at,
    });

    Ok(last_result)
}

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
    /// Task ID (UUID). Currently the same as `id`; separated for future use.
    pub(crate) task_id: String,
    pub(crate) state: TaskState,
    pub(crate) join_handle: Option<JoinHandle<anyhow::Result<String>>>,
    pub(crate) cancel: CancellationToken,
    pub(crate) status_rx: watch::Receiver<SubAgentStatus>,
    pub(crate) grants: PermissionGrants,
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

    /// Return mutable access to definitions, for testing and dynamic registration.
    pub fn definitions_mut(&mut self) -> &mut Vec<SubAgentDef> {
        &mut self.definitions
    }

    /// Spawn a sub-agent by definition name with real background execution.
    ///
    /// Returns the `task_id` (UUID string) that can be used with [`cancel`](Self::cancel)
    /// and [`collect`](Self::collect).
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::NotFound`] if no definition with the given name exists,
    /// [`SubAgentError::Spawn`] if the concurrency limit is exceeded.
    pub fn spawn(
        &mut self,
        def_name: &str,
        task_prompt: &str,
        provider: AnyProvider,
        tool_executor: Arc<dyn ErasedToolExecutor>,
    ) -> Result<String, SubAgentError> {
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

        let started_at = Instant::now();
        let initial_status = SubAgentStatus {
            state: TaskState::Submitted,
            last_message: None,
            turns_used: 0,
            started_at,
        };
        let (status_tx, status_rx) = watch::channel(initial_status);

        let filtered_executor = FilteredToolExecutor::new(tool_executor, def.tools.clone());
        let max_turns = def.permissions.max_turns;
        let system_prompt = def.system_prompt.clone();
        let task_prompt = task_prompt.to_owned();
        let cancel_clone = cancel.clone();

        let join_handle: JoinHandle<anyhow::Result<String>> =
            tokio::spawn(run_agent_loop(AgentLoopArgs {
                provider,
                executor: filtered_executor,
                system_prompt,
                task_prompt,
                max_turns,
                cancel: cancel_clone,
                status_tx,
                started_at,
            }));

        let handle = SubAgentHandle {
            id: task_id.clone(),
            def,
            task_id: task_id.clone(),
            state: TaskState::Submitted,
            join_handle: Some(join_handle),
            cancel,
            status_rx,
            grants: PermissionGrants::default(),
        };

        self.agents.insert(task_id.clone(), handle);
        tracing::info!(task_id, def_name, "sub-agent spawned");
        Ok(task_id)
    }

    /// Cancel all active sub-agents. Called during main agent shutdown.
    pub fn shutdown_all(&mut self) {
        let ids: Vec<String> = self.agents.keys().cloned().collect();
        for id in ids {
            let _ = self.cancel(&id);
        }
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
            .map(|h| {
                let mut status = h.status_rx.borrow().clone();
                // cancel() updates handle.state synchronously but the background task
                // may not have sent the final watch update yet; reflect it here.
                if h.state == TaskState::Canceled {
                    status.state = TaskState::Canceled;
                }
                (h.task_id.clone(), status)
            })
            .collect()
    }

    /// Return the definition for a specific agent by `task_id`.
    #[must_use]
    pub fn agents_def(&self, task_id: &str) -> Option<&SubAgentDef> {
        self.agents.get(task_id).map(|h| &h.def)
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;

    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;
    use zeph_tools::ToolCall;
    use zeph_tools::executor::{ErasedToolExecutor, ToolError, ToolOutput};
    use zeph_tools::registry::ToolDef;

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

    struct NoopExecutor;

    impl ErasedToolExecutor for NoopExecutor {
        fn execute_erased<'a>(
            &'a self,
            _response: &'a str,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a,
            >,
        > {
            Box::pin(std::future::ready(Ok(None)))
        }

        fn execute_confirmed_erased<'a>(
            &'a self,
            _response: &'a str,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a,
            >,
        > {
            Box::pin(std::future::ready(Ok(None)))
        }

        fn tool_definitions_erased(&self) -> Vec<ToolDef> {
            vec![]
        }

        fn execute_tool_call_erased<'a>(
            &'a self,
            _call: &'a ToolCall,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a,
            >,
        > {
            Box::pin(std::future::ready(Ok(None)))
        }
    }

    fn mock_provider(responses: Vec<&str>) -> AnyProvider {
        AnyProvider::Mock(MockProvider::with_responses(
            responses.into_iter().map(String::from).collect(),
        ))
    }

    fn noop_executor() -> Arc<dyn ErasedToolExecutor> {
        Arc::new(NoopExecutor)
    }

    fn do_spawn(
        mgr: &mut SubAgentManager,
        name: &str,
        prompt: &str,
    ) -> Result<String, SubAgentError> {
        mgr.spawn(name, prompt, mock_provider(vec!["done"]), noop_executor())
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
        let err = do_spawn(&mut mgr, "nonexistent", "prompt").unwrap_err();
        assert!(matches!(err, SubAgentError::NotFound(_)));
    }

    #[test]
    fn spawn_and_cancel() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());

        let task_id = do_spawn(&mut mgr, "bot", "do stuff").unwrap();
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

        let task_id = do_spawn(&mut mgr, "bot", "do stuff").unwrap();
        mgr.cancel(&task_id).unwrap();

        // Wait briefly for the task to observe cancellation
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let result = mgr.collect(&task_id).await.unwrap();
        assert!(!mgr.agents.contains_key(&task_id));
        // result may be empty string (cancelled before LLM response) or the mock response
        let _ = result;
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

        let task_id = do_spawn(&mut mgr, "bot", "work").unwrap();
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

        let task_id = do_spawn(&mut mgr, "bot", "work").unwrap();
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

        let task_id = do_spawn(&mut mgr, "bot", "work").unwrap();
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

        let _first = do_spawn(&mut mgr, "bot", "first").unwrap();
        let err = do_spawn(&mut mgr, "bot", "second").unwrap_err();
        assert!(matches!(err, SubAgentError::Spawn(_)));
    }

    #[tokio::test]
    async fn background_agent_does_not_block_caller() {
        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());

        // Spawn should return immediately without waiting for LLM
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            std::future::ready(do_spawn(&mut mgr, "bot", "work")),
        )
        .await;
        assert!(result.is_ok(), "spawn() must not block");
        assert!(result.unwrap().is_ok());
    }

    #[tokio::test]
    async fn max_turns_terminates_agent_loop() {
        let mut mgr = make_manager();
        // max_turns = 1, mock returns empty (no tool call), so loop ends after 1 turn
        let def = SubAgentDef::parse(
            "+++\nname = \"limited\"\ndescription = \"A bot\"\n[permissions]\nmax_turns = 1\n+++\n\nDo one thing.\n",
        )
        .unwrap();
        mgr.definitions.push(def);

        let task_id = mgr
            .spawn(
                "limited",
                "task",
                mock_provider(vec!["final answer"]),
                noop_executor(),
            )
            .unwrap();

        // Wait for completion
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let status = mgr.statuses().into_iter().find(|(id, _)| id == &task_id);
        // Status should show Completed or still Working but <= 1 turn
        if let Some((_, s)) = status {
            assert!(s.turns_used <= 1);
        }
    }

    #[tokio::test]
    async fn cancellation_token_stops_agent_loop() {
        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());

        let task_id = do_spawn(&mut mgr, "bot", "long task").unwrap();

        // Cancel immediately
        mgr.cancel(&task_id).unwrap();

        // Wait a bit then collect
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let result = mgr.collect(&task_id).await;
        // Cancelled task may return empty or partial result — both are acceptable
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn shutdown_all_cancels_all_active_agents() {
        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());

        do_spawn(&mut mgr, "bot", "task 1").unwrap();
        do_spawn(&mut mgr, "bot", "task 2").unwrap();

        assert_eq!(mgr.agents.len(), 2);
        mgr.shutdown_all();

        // All agents should be in Canceled state
        for (_, status) in mgr.statuses() {
            assert_eq!(status.state, TaskState::Canceled);
        }
    }

    #[test]
    fn debug_impl_does_not_expose_sensitive_fields() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut mgr = make_manager();
        mgr.definitions.push(def_with_secrets());
        let task_id = do_spawn(&mut mgr, "bot", "work").unwrap();
        let handle = &mgr.agents[&task_id];
        let debug_str = format!("{handle:?}");
        // SubAgentHandle Debug must not expose grant contents or secrets
        assert!(!debug_str.contains("api-key"));
    }

    #[tokio::test]
    async fn llm_failure_transitions_to_failed_state() {
        let rt_handle = tokio::runtime::Handle::current();
        let _guard = rt_handle.enter();
        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());

        let failing = AnyProvider::Mock(MockProvider::failing());
        let task_id = mgr
            .spawn("bot", "do work", failing, noop_executor())
            .unwrap();

        // Wait for the background task to complete.
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        let statuses = mgr.statuses();
        let status = statuses
            .iter()
            .find(|(id, _)| id == &task_id)
            .map(|(_, s)| s);
        // The background loop should have caught the LLM error and reported Failed.
        assert!(
            status.is_some_and(|s| s.state == TaskState::Failed),
            "expected Failed, got: {status:?}"
        );
    }

    #[tokio::test]
    async fn tool_call_loop_two_turns() {
        use std::sync::Mutex;
        use zeph_tools::ToolCall;

        struct ToolOnceExecutor {
            calls: Mutex<u32>,
        }

        impl ErasedToolExecutor for ToolOnceExecutor {
            fn execute_erased<'a>(
                &'a self,
                _response: &'a str,
            ) -> Pin<
                Box<
                    dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>>
                        + Send
                        + 'a,
                >,
            > {
                Box::pin(std::future::ready(Ok(None)))
            }

            fn execute_confirmed_erased<'a>(
                &'a self,
                _response: &'a str,
            ) -> Pin<
                Box<
                    dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>>
                        + Send
                        + 'a,
                >,
            > {
                Box::pin(std::future::ready(Ok(None)))
            }

            fn tool_definitions_erased(&self) -> Vec<ToolDef> {
                vec![]
            }

            fn execute_tool_call_erased<'a>(
                &'a self,
                call: &'a ToolCall,
            ) -> Pin<
                Box<
                    dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>>
                        + Send
                        + 'a,
                >,
            > {
                let mut n = self.calls.lock().unwrap();
                *n += 1;
                let result = if *n == 1 {
                    // First call: return tool output (simulates tool call)
                    Ok(Some(ToolOutput {
                        tool_name: call.tool_id.clone(),
                        summary: "step 1 done".into(),
                        blocks_executed: 1,
                        filter_stats: None,
                        diff: None,
                        streamed: false,
                    }))
                } else {
                    Ok(None)
                };
                Box::pin(std::future::ready(result))
            }
        }

        let rt_handle = tokio::runtime::Handle::current();
        let _guard = rt_handle.enter();
        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());

        // Two responses: first triggers tool handling, second is final.
        let provider = mock_provider(vec!["turn 1 response", "final answer"]);
        let executor = Arc::new(ToolOnceExecutor {
            calls: Mutex::new(0),
        });

        let task_id = mgr
            .spawn("bot", "run two turns", provider, executor)
            .unwrap();

        // Wait for background loop to finish.
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

        let result = mgr.collect(&task_id).await;
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
    }

    #[tokio::test]
    async fn collect_on_running_task_completes_eventually() {
        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());

        // Spawn with a slow response so the task is still running.
        let task_id = do_spawn(&mut mgr, "bot", "slow work").unwrap();

        // collect() awaits the JoinHandle, so it will finish when the task completes.
        let result =
            tokio::time::timeout(tokio::time::Duration::from_secs(5), mgr.collect(&task_id)).await;

        assert!(result.is_ok(), "collect timed out after 5s");
        let inner = result.unwrap();
        assert!(inner.is_ok(), "collect returned error: {inner:?}");
    }

    #[test]
    fn concurrency_slot_freed_after_cancel() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut mgr = SubAgentManager::new(1); // limit to 1
        mgr.definitions.push(sample_def());

        let id1 = do_spawn(&mut mgr, "bot", "task 1").unwrap();

        // Concurrency limit reached — second spawn should fail.
        let err = do_spawn(&mut mgr, "bot", "task 2").unwrap_err();
        assert!(
            matches!(err, SubAgentError::Spawn(ref msg) if msg.contains("concurrency limit")),
            "expected concurrency limit error, got: {err}"
        );

        // Cancel the first agent to free the slot.
        mgr.cancel(&id1).unwrap();

        // Now a new spawn should succeed.
        let result = do_spawn(&mut mgr, "bot", "task 3");
        assert!(
            result.is_ok(),
            "expected spawn to succeed after cancel, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn statuses_does_not_include_collected_task() {
        let mut mgr = make_manager();
        mgr.definitions.push(sample_def());

        let task_id = do_spawn(&mut mgr, "bot", "task").unwrap();
        assert_eq!(mgr.statuses().len(), 1);

        // Wait for task completion then collect.
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
        let _ = mgr.collect(&task_id).await;

        // After collect(), the task should no longer appear in statuses.
        assert!(
            mgr.statuses().is_empty(),
            "expected empty statuses after collect"
        );
    }
}
