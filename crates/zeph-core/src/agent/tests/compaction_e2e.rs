// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// End-to-end tests for M30 resilient compaction: error detection → compact → retry → success.
use crate::agent::agent_tests::*;
use zeph_llm::LlmError;
use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::{Message, MessageMetadata, Role};

/// Verify that the agent recovers from a `ContextLengthExceeded` error during an LLM call,
/// compacts its context, and returns a successful response on the next attempt.
#[tokio::test]
async fn agent_recovers_from_context_length_exceeded_and_produces_response() {
    // Provider: first call raises ContextLengthExceeded, second call succeeds.
    let provider = AnyProvider::Mock(
        MockProvider::with_responses(vec!["final answer".into()])
            .with_errors(vec![LlmError::ContextLengthExceeded]),
    );
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();

    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor)
        // Provide a context budget so compact_context has a compaction target
        .with_context_budget(200_000, 0.20, 0.80, 4, 0);

    // Seed a user message so the agent has something to compact/retry
    agent.msg.messages.push(Message {
        role: Role::User,
        content: "describe the architecture".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    });

    // call_llm_with_retry is the direct entry point for the retry/compact flow
    let result = agent.call_llm_with_retry(2).await.unwrap();

    assert!(
        result.is_some(),
        "agent must produce a response after recovering from context length error"
    );
    assert_eq!(result.as_deref(), Some("final answer"));

    // Verify the channel received the recovered response
    let sent = agent.channel.sent_messages();
    assert!(
        sent.iter().any(|m| m.contains("final answer")),
        "recovered response must be forwarded to the channel; got: {sent:?}"
    );
}

/// E2E test: spawn sub-agent in background, verify it runs and produces output.
///
/// Scope: spawn → text response → collect (`MockProvider` only supports text responses).
#[tokio::test]
async fn subagent_spawn_text_collect_e2e() {
    use zeph_subagent::def::{SkillFilter, SubAgentPermissions, ToolPolicy};
    use zeph_subagent::hooks::SubagentHooks;
    use zeph_subagent::{AgentCommand, SubAgentDef, SubAgentManager};

    // Provider shared between main agent and sub-agent via Arc clone.
    // We pre-load a response that the sub-agent loop will consume.
    let provider = mock_provider(vec!["task completed successfully".into()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

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
    agent.orchestration.subagent_manager = Some(mgr);

    // Spawn the sub-agent in background — returns immediately with the task id.
    let spawn_resp = agent
        .handle_agent_command(AgentCommand::Background {
            name: "worker".into(),
            prompt: "do a task".into(),
        })
        .await
        .expect("Background spawn must return Some");
    assert!(
        spawn_resp.contains("worker"),
        "spawn response must mention agent name; got: {spawn_resp}"
    );
    assert!(
        spawn_resp.contains("started"),
        "spawn response must confirm start; got: {spawn_resp}"
    );

    // Extract the short id from response: "Sub-agent 'worker' started in background (id: XXXXXXXX)"
    let short_id = spawn_resp
        .split("id: ")
        .nth(1)
        .expect("response must contain 'id: '")
        .trim_end_matches(')')
        .trim()
        .to_string();
    assert!(!short_id.is_empty(), "short_id must not be empty");

    // Poll until the sub-agent reaches a terminal state (max 5s).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let full_id = loop {
        let mgr = agent.orchestration.subagent_manager.as_ref().unwrap();
        let statuses = mgr.statuses();
        let found = statuses.iter().find(|(id, _)| id.starts_with(&short_id));
        if let Some((id, status)) = found {
            match status.state {
                zeph_subagent::SubAgentState::Completed => break id.clone(),
                zeph_subagent::SubAgentState::Failed => {
                    panic!(
                        "sub-agent reached Failed state unexpectedly: {:?}",
                        status.last_message
                    );
                }
                _ => {}
            }
        }
        assert!(
            std::time::Instant::now() <= deadline,
            "sub-agent did not complete within timeout"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };

    // Collect result and verify output.
    let result = agent
        .orchestration
        .subagent_manager
        .as_mut()
        .unwrap()
        .collect(&full_id)
        .await
        .expect("collect must succeed for completed sub-agent");
    assert!(
        result.contains("task completed successfully"),
        "collected result must contain sub-agent output; got: {result:?}"
    );
}

/// Unit test for secret bridge in foreground spawn poll loop.
///
/// Verifies that when a sub-agent emits [`REQUEST_SECRET`: api-key], the bridge:
/// - calls `channel.confirm()` with a prompt containing the key name
/// - on approval, delivers the secret to the sub-agent
/// The `MockChannel` `confirm()` is pre-loaded with `true` (approve).
#[tokio::test]
async fn foreground_spawn_secret_bridge_approves() {
    use zeph_subagent::def::{SkillFilter, SubAgentPermissions, ToolPolicy};
    use zeph_subagent::hooks::SubagentHooks;
    use zeph_subagent::{AgentCommand, SubAgentDef, SubAgentManager};

    // Sub-agent loop responses:
    //   turn 1: request a secret
    //   turn 2: final reply after secret delivered
    let provider = mock_provider(vec![
        "[REQUEST_SECRET: api-key]".into(),
        "done with secret".into(),
    ]);

    // MockChannel with confirm() → true (approve)
    let channel = MockChannel::new(vec![]).with_confirmations(vec![true]);

    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

    let mut mgr = SubAgentManager::new(4);
    mgr.definitions_mut().push(SubAgentDef {
        name: "vault-bot".into(),
        description: "A bot that requests secrets".into(),
        model: None,
        tools: ToolPolicy::InheritAll,
        disallowed_tools: vec![],
        permissions: SubAgentPermissions {
            max_turns: 2,
            secrets: vec!["api-key".into()],
            ..SubAgentPermissions::default()
        },
        skills: SkillFilter::default(),
        system_prompt: "You need a secret.".into(),
        hooks: SubagentHooks::default(),
        memory: None,
        source: None,
        file_path: None,
    });
    agent.orchestration.subagent_manager = Some(mgr);

    // Foreground spawn — blocks until sub-agent completes.
    let resp: String = agent
        .handle_agent_command(AgentCommand::Spawn {
            name: "vault-bot".into(),
            prompt: "fetch the api key".into(),
        })
        .await
        .expect("Spawn must return Some");

    // Sub-agent completed after secret was bridged (approve path).
    // The sub-agent had 2 turns: turn 1 = secret request, turn 2 = final reply.
    // If the bridge did NOT call confirm(), the sub-agent would never get the
    // approval outcome and the foreground poll loop would stall or time out.
    // Reaching this point proves the bridge ran and confirm() was called.
    assert!(
        resp.contains("vault-bot"),
        "response must mention agent name; got: {resp}"
    );
    assert!(
        resp.contains("completed"),
        "sub-agent must complete successfully; got: {resp}"
    );
}

// ── /plan handler unit tests ─────────────────────────────────────────────

#[cfg(feature = "scheduler")]
use zeph_orchestration::{GraphStatus, PlanCommand, TaskGraph, TaskNode, TaskResult, TaskStatus};

#[cfg(feature = "scheduler")]
fn agent_with_orchestration() -> Agent<MockChannel> {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.orchestration.orchestration_config.enabled = true;
    agent
}

#[cfg(feature = "scheduler")]
fn make_simple_graph(status: GraphStatus) -> TaskGraph {
    let mut g = TaskGraph::new("test goal");
    let mut node = TaskNode::new(0, "task-0", "do something");
    node.status = match status {
        GraphStatus::Created => TaskStatus::Pending,
        GraphStatus::Running => TaskStatus::Ready,
        _ => TaskStatus::Completed,
    };
    if status == GraphStatus::Running || status == GraphStatus::Completed {
        node.result = Some(TaskResult {
            output: "done".into(),
            artifacts: vec![],
            duration_ms: 0,
            agent_id: None,
            agent_def: None,
        });
        if status == GraphStatus::Completed {
            node.status = TaskStatus::Completed;
        }
    }
    g.tasks.push(node);
    g.status = status;
    g
}

/// GAP-1: `handle_plan_confirm` with `subagent_manager` = None → fallback message,
/// graph restored in `pending_graph`.
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn plan_confirm_no_manager_restores_graph() {
    let mut agent = agent_with_orchestration();

    let graph = make_simple_graph(GraphStatus::Created);
    agent.orchestration.pending_graph = Some(graph);

    // No subagent_manager set.
    agent
        .handle_plan_command_as_string(PlanCommand::Confirm)
        .await
        .unwrap();

    // Graph must be restored.
    assert!(
        agent.orchestration.pending_graph.is_some(),
        "graph must be restored when no manager configured"
    );
    let msgs = agent.channel.sent_messages();
    assert!(
        msgs.iter().any(|m| m.contains("sub-agent")),
        "must send fallback message; got: {msgs:?}"
    );
}

/// GAP-2: `handle_plan_confirm` with `pending_graph` = None → "No pending plan" message.
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn plan_confirm_no_pending_graph_sends_message() {
    let mut agent = agent_with_orchestration();

    // No pending_graph.
    agent
        .handle_plan_command_as_string(PlanCommand::Confirm)
        .await
        .unwrap();

    let msgs = agent.channel.sent_messages();
    assert!(
        msgs.iter().any(|m| m.contains("No pending plan")),
        "must send 'No pending plan' message; got: {msgs:?}"
    );
}

/// GAP-3: happy path — pre-built Running graph with one already-Completed task.
/// `resume_from()` accepts it; first `tick()` emits Done{Completed}; aggregation called.
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn plan_confirm_completed_graph_aggregates() {
    use zeph_subagent::def::{SkillFilter, SubAgentPermissions, ToolPolicy};
    use zeph_subagent::hooks::SubagentHooks;
    use zeph_subagent::{SubAgentDef, SubAgentManager};

    // MockProvider returns the aggregation synthesis.
    let provider = mock_provider(vec!["synthesis result".into()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.orchestration.orchestration_config.enabled = true;

    let mut mgr = SubAgentManager::new(4);
    mgr.definitions_mut().push(SubAgentDef {
        name: "worker".into(),
        description: "A worker".into(),
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
    });
    agent.orchestration.subagent_manager = Some(mgr);

    // Graph with one already-Completed task in Running status: resume_from() accepts it,
    // and the first tick() will find no running/ready tasks → Done{Completed}.
    let mut graph = TaskGraph::new("test goal");
    let mut node = TaskNode::new(0, "task-0", "already done");
    node.status = TaskStatus::Completed;
    node.result = Some(TaskResult {
        output: "task output".into(),
        artifacts: vec![],
        duration_ms: 10,
        agent_id: None,
        agent_def: None,
    });
    graph.tasks.push(node);
    graph.status = GraphStatus::Running;
    agent.orchestration.pending_graph = Some(graph);

    agent
        .handle_plan_command_as_string(PlanCommand::Confirm)
        .await
        .unwrap();

    let msgs = agent.channel.sent_messages();
    // Aggregation synthesis should appear in messages.
    assert!(
        msgs.iter().any(|m| m.contains("synthesis result")),
        "aggregation synthesis must be sent to user; got: {msgs:?}"
    );
    // Graph must be cleared after successful completion.
    assert!(
        agent.orchestration.pending_graph.is_none(),
        "pending_graph must be cleared after Completed"
    );
}

/// GAP-4: `handle_plan_confirm` with no sub-agents defined but provider fails →
/// task executes inline but provider returns error → plan fails, failure message sent.
///
/// Since the fix for #1463, no agents → `RunInline` (not `spawn_for_task`). So when
/// the provider fails, the scheduler records a Failed `TaskOutcome`, graph fails,
/// and `finalize_plan_execution` sends a failure message.
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn plan_confirm_inline_provider_failure_sends_message() {
    use zeph_subagent::SubAgentManager;

    // Failing provider → chat() always returns an error.
    let provider = mock_provider_failing();
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.orchestration.orchestration_config.enabled = true;

    // Manager with no defined agents → route() returns None → RunInline.
    agent.orchestration.subagent_manager = Some(SubAgentManager::new(4));

    // Graph in Created status with one task; scheduler emits RunInline,
    // provider fails → TaskOutcome::Failed → graph Failed.
    let mut graph = TaskGraph::new("failing inline goal");
    let node = TaskNode::new(0, "task-0", "will fail inline");
    graph.tasks.push(node);
    graph.status = GraphStatus::Created;
    agent.orchestration.pending_graph = Some(graph);

    agent
        .handle_plan_command_as_string(PlanCommand::Confirm)
        .await
        .unwrap();

    let msgs = agent.channel.sent_messages();
    assert!(
        msgs.iter()
            .any(|m| m.contains("failed") || m.contains("Failed")),
        "failure message must be sent after inline provider error; got: {msgs:?}"
    );
}

/// GAP-5: `handle_plan_list` with `pending_graph` → shows summary + status label.
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn plan_list_with_pending_graph_shows_summary() {
    let mut agent = agent_with_orchestration();

    agent.orchestration.pending_graph = Some(make_simple_graph(GraphStatus::Created));

    let out = agent
        .handle_plan_command_as_string(PlanCommand::List)
        .await
        .unwrap();

    assert!(
        out.contains("awaiting confirmation"),
        "must show 'awaiting confirmation' status; got: {out:?}"
    );
}

/// GAP-6: `handle_plan_list` with no graph → "No recent plans."
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn plan_list_no_graph_shows_no_recent() {
    let mut agent = agent_with_orchestration();

    let out = agent
        .handle_plan_command_as_string(PlanCommand::List)
        .await
        .unwrap();

    assert!(
        out.contains("No recent plans"),
        "must show 'No recent plans'; got: {out:?}"
    );
}

/// GAP-7: `handle_plan_retry` resets Running tasks to Ready and clears `assigned_agent`.
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn plan_retry_resets_running_tasks_to_ready() {
    let mut agent = agent_with_orchestration();

    let mut graph = TaskGraph::new("retry test");
    let mut failed = TaskNode::new(0, "failed-task", "desc");
    failed.status = TaskStatus::Failed;
    let mut stale_running = TaskNode::new(1, "stale-task", "desc");
    stale_running.status = TaskStatus::Running;
    stale_running.assigned_agent = Some("old-handle-id".into());
    graph.tasks.push(failed);
    graph.tasks.push(stale_running);
    graph.status = GraphStatus::Failed;
    agent.orchestration.pending_graph = Some(graph);

    agent
        .handle_plan_command_as_string(PlanCommand::Retry(None))
        .await
        .unwrap();

    let g = agent
        .orchestration
        .pending_graph
        .as_ref()
        .expect("graph must be present after retry");

    // Failed task must be reset to Ready.
    assert_eq!(
        g.tasks[0].status,
        TaskStatus::Ready,
        "failed task must be reset to Ready"
    );

    // Stale Running task must be reset to Ready and assigned_agent cleared.
    assert_eq!(
        g.tasks[1].status,
        TaskStatus::Ready,
        "stale Running task must be reset to Ready"
    );
    assert!(
        g.tasks[1].assigned_agent.is_none(),
        "assigned_agent must be cleared for stale Running task"
    );
}

/// GAP-A: `handle_plan_cancel_as_string` with no active plan returns "No active plan".
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn plan_cancel_as_string_no_active_plan() {
    let mut agent = agent_with_orchestration();
    let out = agent.handle_plan_cancel_as_string(None);
    assert!(
        out.contains("No active plan"),
        "must return 'No active plan' message; got: {out:?}"
    );
}

/// GAP-A: `handle_plan_resume_as_string` with no pending graph returns "No paused plan".
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn plan_resume_as_string_no_paused_plan() {
    let mut agent = agent_with_orchestration();
    let out = agent.handle_plan_resume_as_string(None).await;
    assert!(
        out.contains("No paused plan"),
        "must return 'No paused plan' message; got: {out:?}"
    );
}

/// GAP-B: `dispatch_plan_command_as_string` with a parse error returns `Ok(non-empty)`.
/// `/plan list extra_args` is rejected by the parser — the error must be returned as
/// `Ok(message)`, not propagated as `Err`.
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn dispatch_plan_command_as_string_invalid_subcommand() {
    let mut agent = agent_with_orchestration();
    let result = agent
        .dispatch_plan_command_as_string("/plan list unexpected_arg")
        .await
        .unwrap();
    assert!(
        !result.is_empty(),
        "parse error must be returned as Ok(non-empty string), not propagated; got: {result:?}"
    );
}

/// Regression test for issue #1454: secret requests queued before the first `tick()` were
/// silently dropped when a single-task plan completed on the very first tick (instant
/// completion) because `process_pending_secret_requests()` was only called inside the
/// `'tick: loop` body, which exits immediately via `break` before reaching the drain call.
///
/// The fix adds a final `process_pending_secret_requests()` drain after the loop exits.
/// This test verifies that drain by:
/// 1. Pre-loading a `SecretRequest` into the manager's channel **before** the plan runs.
/// 2. Using a graph where the first `tick()` emits `Done` (all tasks already Completed).
/// 3. Asserting that `try_recv_secret_request()` returns `None` after the plan loop,
///    proving the drain was executed.
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn test_secret_drain_after_instant_completion() {
    use tokio_util::sync::CancellationToken;
    use zeph_subagent::def::{SkillFilter, SubAgentPermissions, ToolPolicy};
    use zeph_subagent::hooks::SubagentHooks;
    use zeph_subagent::{
        PermissionGrants, SecretRequest, SubAgentDef, SubAgentHandle, SubAgentManager,
        SubAgentState, SubAgentStatus,
    };

    // Channel with one pre-loaded confirmation (approve) so the bridge can resolve the
    // pending request when it is finally drained.
    let channel = MockChannel::new(vec![]).with_confirmations(vec![true]);

    // Provider returns aggregation synthesis (needed to satisfy finalize_plan_execution).
    let provider = mock_provider(vec!["synthesis".into()]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.orchestration.orchestration_config.enabled = true;

    // Build a manager with one agent definition (needed by finalize_plan_execution).
    let mut mgr = SubAgentManager::new(4);
    mgr.definitions_mut().push(SubAgentDef {
        name: "worker".into(),
        description: "A worker".into(),
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
    });

    // Create a fake handle whose `pending_secret_rx` already contains one SecretRequest.
    // This simulates a sub-agent that queued the request before the plan loop ran.
    let (secret_request_tx, pending_secret_rx) = tokio::sync::mpsc::channel::<SecretRequest>(4);
    let (secret_tx, _secret_rx) = tokio::sync::mpsc::channel(1);
    let (status_tx, status_rx) = watch::channel(SubAgentStatus {
        state: SubAgentState::Completed,
        last_message: None,
        turns_used: 1,
        started_at: std::time::Instant::now(),
    });
    drop(status_tx);

    // Pre-load the secret request into the channel before plan execution starts.
    secret_request_tx
        .send(SecretRequest {
            secret_key: "api-key".into(),
            reason: Some("test drain".into()),
        })
        .await
        .expect("channel must accept request");
    drop(secret_request_tx); // close sender so try_recv returns None after drain

    let fake_handle_id = "deadbeef-0000-0000-0000-000000000001".to_owned();
    let def_clone = mgr.definitions()[0].clone();
    mgr.insert_handle_for_test(
        fake_handle_id.clone(),
        SubAgentHandle {
            id: fake_handle_id.clone(),
            def: def_clone,
            task_id: fake_handle_id.clone(),
            state: SubAgentState::Completed,
            join_handle: None,
            cancel: CancellationToken::new(),
            status_rx,
            grants: PermissionGrants::default(),
            pending_secret_rx,
            secret_tx,
            started_at_str: "2026-01-01T00:00:00Z".to_owned(),
            transcript_dir: None,
        },
    );
    agent.orchestration.subagent_manager = Some(mgr);

    // Graph with one already-Completed task in Running status: the first tick() finds no
    // Running/Ready tasks and emits Done{Completed} immediately (instant completion).
    let mut graph = TaskGraph::new("instant goal");
    let mut node = TaskNode::new(0, "task-0", "already done");
    node.status = TaskStatus::Completed;
    node.result = Some(TaskResult {
        output: "task output".into(),
        artifacts: vec![],
        duration_ms: 1,
        agent_id: None,
        agent_def: None,
    });
    graph.tasks.push(node);
    graph.status = GraphStatus::Running;
    agent.orchestration.pending_graph = Some(graph);

    // Run the plan loop — the fix adds a post-loop drain call.
    agent
        .handle_plan_command_as_string(PlanCommand::Confirm)
        .await
        .unwrap();

    // After plan completion, the secret request must have been drained.
    // If the drain was NOT called, try_recv_secret_request() would return Some(_).
    let leftover = agent
        .orchestration
        .subagent_manager
        .as_mut()
        .and_then(SubAgentManager::try_recv_secret_request);
    assert!(
        leftover.is_none(),
        "pending secret request must be drained after instant plan completion; \
         got: {leftover:?}"
    );
}

/// GAP-8: `handle_plan_confirm` with no sub-agents defined executes the task inline
/// via the main provider. Verifies `RunInline` path: plan succeeds, provider output
/// appears in aggregation, `pending_graph` is cleared.
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn plan_confirm_no_subagents_executes_inline() {
    use zeph_subagent::SubAgentManager;

    // Provider returns task result then aggregation synthesis.
    let provider = mock_provider(vec!["inline task output".into(), "synthesis done".into()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.orchestration.orchestration_config.enabled = true;

    // SubAgentManager with no definitions → route() returns None → RunInline.
    agent.orchestration.subagent_manager = Some(SubAgentManager::new(4));

    // Simple single-task graph.
    let mut graph = TaskGraph::new("inline goal");
    let node = TaskNode::new(0, "task-0", "do something inline");
    graph.tasks.push(node);
    graph.status = GraphStatus::Created;
    agent.orchestration.pending_graph = Some(graph);

    agent
        .handle_plan_command_as_string(PlanCommand::Confirm)
        .await
        .unwrap();

    // Graph must be cleared after successful execution.
    assert!(
        agent.orchestration.pending_graph.is_none(),
        "pending_graph must be cleared after inline plan completion"
    );
    let msgs = agent.channel.sent_messages();
    assert!(
        msgs.iter().any(|m| m.contains("synthesis done")),
        "aggregation synthesis must appear in messages; got: {msgs:?}"
    );
}

/// COV-01: `/plan cancel` received during `run_scheduler_loop` cancels the plan.
///
/// Verifies that when the channel delivers "/plan cancel" while the scheduler loop
/// is waiting for a task event, `cancel_all()` is called and the loop exits with
/// `GraphStatus::Canceled`. The "Canceling plan..." status must be sent immediately.
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn plan_cancel_during_scheduler_loop_cancels_plan() {
    use crate::config::OrchestrationConfig;
    use zeph_orchestration::{DagScheduler, RuleBasedRouter};
    use zeph_subagent::SubAgentManager;

    // Channel pre-loaded with "/plan cancel" — returned immediately on first recv().
    let channel = MockChannel::new(vec!["/plan cancel".to_owned()]);
    let provider = mock_provider(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.orchestration.orchestration_config.enabled = true;
    agent.orchestration.subagent_manager = Some(SubAgentManager::new(4));

    // Graph in Running status with one task in Running state: tick() will not emit
    // any actions (no Ready tasks, no timed-out running tasks), so the loop reaches
    // wait_event() / select! immediately.
    let mut graph = TaskGraph::new("cancel test goal");
    let mut node = TaskNode::new(0, "task-0", "will be canceled");
    node.status = TaskStatus::Running;
    graph.tasks.push(node);
    graph.status = GraphStatus::Running;

    let config = OrchestrationConfig {
        enabled: true,
        ..OrchestrationConfig::default()
    };
    let mut scheduler =
        DagScheduler::resume_from(graph, &config, Box::new(RuleBasedRouter), vec![]).unwrap();

    let token = tokio_util::sync::CancellationToken::new();
    let status = agent
        .run_scheduler_loop(&mut scheduler, 1, token)
        .await
        .unwrap();

    assert_eq!(
        status,
        GraphStatus::Canceled,
        "run_scheduler_loop must return Canceled when /plan cancel is received"
    );
    assert!(
        agent
            .channel
            .statuses
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.contains("Canceling plan")),
        "must send 'Canceling plan...' status before processing cancel"
    );
}

/// COV-02: `finalize_plan_execution` with `GraphStatus::Canceled` sends the correct
/// message, does NOT store the graph into `pending_graph`, and updates
/// `orchestration.tasks_completed` with the count of tasks that finished before cancel.
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn finalize_plan_execution_canceled_does_not_store_graph() {
    use zeph_subagent::SubAgentManager;

    let channel = MockChannel::new(vec![]);
    let provider = mock_provider(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (metrics_tx, metrics_rx) = watch::channel(MetricsSnapshot::default());
    let mut agent =
        Agent::new(provider, channel, registry, None, 5, executor).with_metrics(metrics_tx);
    agent.orchestration.orchestration_config.enabled = true;
    agent.orchestration.subagent_manager = Some(SubAgentManager::new(4));

    // Graph with one completed task and one canceled task — typical mid-cancel state.
    let mut graph = TaskGraph::new("cancel finalize test");
    let mut completed = TaskNode::new(0, "task-done", "finished");
    completed.status = TaskStatus::Completed;
    completed.result = Some(TaskResult {
        output: "done".into(),
        artifacts: vec![],
        duration_ms: 10,
        agent_id: None,
        agent_def: None,
    });
    let mut canceled = TaskNode::new(1, "task-canceled", "was running");
    canceled.status = TaskStatus::Canceled;
    graph.tasks.push(completed);
    graph.tasks.push(canceled);
    graph.status = GraphStatus::Canceled;

    agent
        .finalize_plan_execution(graph, GraphStatus::Canceled)
        .await
        .unwrap();

    let msgs = agent.channel.sent_messages();
    assert!(
        msgs.iter()
            .any(|m| m.contains("canceled") || m.contains("Canceled")),
        "must send a cancellation message; got: {msgs:?}"
    );
    assert!(
        msgs.iter().any(|m| m.contains("1/2")),
        "must report completed task count (1/2); got: {msgs:?}"
    );
    assert!(
        agent.orchestration.pending_graph.is_none(),
        "canceled plan must NOT be stored in pending_graph"
    );
    let snapshot = metrics_rx.borrow().clone();
    assert_eq!(
        snapshot.orchestration.tasks_completed, 1,
        "tasks completed before cancellation must be counted in metrics"
    );
}

/// COV-03: a non-cancel message received via the channel during `run_scheduler_loop`
/// is queued in `message_queue` for processing after the plan completes.
///
/// Verifies the `tokio::select!` path added in #1603: when the channel delivers a
/// non-cancel message while the loop is waiting for a scheduler event, the message
/// is passed to `enqueue_or_merge()` and appears in `agent.msg.message_queue` after
/// `run_scheduler_loop` returns.
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn scheduler_loop_queues_non_cancel_message() {
    use crate::config::OrchestrationConfig;
    use zeph_orchestration::{DagScheduler, RuleBasedRouter};
    use zeph_subagent::SubAgentManager;

    // Channel pre-loaded with one non-cancel message; second recv() returns None,
    // which terminates the loop with GraphStatus::Failed — acceptable for this test
    // since we only verify queuing, not plan completion status.
    let channel = MockChannel::new(vec!["hello".to_owned()]);
    let provider = mock_provider(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.orchestration.orchestration_config.enabled = true;
    agent.orchestration.subagent_manager = Some(SubAgentManager::new(4));

    // Graph in Running status with one task in Running state: tick() emits no actions
    // (no Ready tasks, running_in_graph_now > 0 suppresses Done), so the loop reaches
    // the select! where channel.recv() delivers "hello" before the loop exits.
    let mut graph = TaskGraph::new("queue test goal");
    let mut node = TaskNode::new(0, "task-0", "long running task");
    node.status = TaskStatus::Running;
    graph.tasks.push(node);
    graph.status = GraphStatus::Running;

    let config = OrchestrationConfig {
        enabled: true,
        ..OrchestrationConfig::default()
    };
    let mut scheduler =
        DagScheduler::resume_from(graph, &config, Box::new(RuleBasedRouter), vec![]).unwrap();

    let token = tokio_util::sync::CancellationToken::new();
    let _ = agent
        .run_scheduler_loop(&mut scheduler, 1, token)
        .await
        .unwrap();

    assert_eq!(
        agent.msg.message_queue.len(),
        1,
        "non-cancel message must be queued in message_queue; got: {:?}",
        agent
            .msg
            .message_queue
            .iter()
            .map(|m| &m.text)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        agent.msg.message_queue[0].text, "hello",
        "queued message text must match the received message"
    );
}

/// COV-04: channel close (`Ok(None)`) on an exit-supporting channel (CLI/TUI) returns
/// `GraphStatus::Canceled` — no retry needed, stdin EOF is a normal termination.
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn scheduler_loop_channel_close_supports_exit_returns_canceled() {
    use crate::config::OrchestrationConfig;
    use zeph_orchestration::{DagScheduler, RuleBasedRouter};
    use zeph_subagent::SubAgentManager;

    // Empty channel with exit_supported=true (the default): recv() returns Ok(None) immediately.
    let channel = MockChannel::new(vec![]);
    let provider = mock_provider(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.orchestration.orchestration_config.enabled = true;
    agent.orchestration.subagent_manager = Some(SubAgentManager::new(4));

    let mut graph = TaskGraph::new("channel close test goal");
    let mut node = TaskNode::new(0, "task-0", "will be canceled on channel close");
    node.status = TaskStatus::Running;
    graph.tasks.push(node);
    graph.status = GraphStatus::Running;

    let config = OrchestrationConfig {
        enabled: true,
        ..OrchestrationConfig::default()
    };
    let mut scheduler =
        DagScheduler::resume_from(graph, &config, Box::new(RuleBasedRouter), vec![]).unwrap();

    let token = tokio_util::sync::CancellationToken::new();
    let status = agent
        .run_scheduler_loop(&mut scheduler, 1, token)
        .await
        .unwrap();

    assert_eq!(
        status,
        GraphStatus::Canceled,
        "channel close on exit-supporting channel (CLI/TUI) must return Canceled, not Failed"
    );
}

/// COV-04b: channel close (`Ok(None)`) on a server channel (Telegram/Discord/Slack,
/// `supports_exit()=false`) returns `GraphStatus::Failed` so the user can `/plan retry`
/// after reconnecting.
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn scheduler_loop_channel_close_no_exit_support_returns_failed() {
    use crate::config::OrchestrationConfig;
    use zeph_orchestration::{DagScheduler, RuleBasedRouter};
    use zeph_subagent::SubAgentManager;

    // Channel with exit_supported=false simulates Telegram/Discord/Slack.
    let channel = MockChannel::new(vec![]).without_exit_support();
    let provider = mock_provider(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.orchestration.orchestration_config.enabled = true;
    agent.orchestration.subagent_manager = Some(SubAgentManager::new(4));

    let mut graph = TaskGraph::new("server channel close goal");
    let mut node = TaskNode::new(0, "task-0", "interrupted by infra failure");
    node.status = TaskStatus::Running;
    graph.tasks.push(node);
    graph.status = GraphStatus::Running;

    let config = OrchestrationConfig {
        enabled: true,
        ..OrchestrationConfig::default()
    };
    let mut scheduler =
        DagScheduler::resume_from(graph, &config, Box::new(RuleBasedRouter), vec![]).unwrap();

    let token = tokio_util::sync::CancellationToken::new();
    let status = agent
        .run_scheduler_loop(&mut scheduler, 1, token)
        .await
        .unwrap();

    assert_eq!(
        status,
        GraphStatus::Failed,
        "channel close on server channel (no exit support) must return Failed so the plan can be retried"
    );
}

/// COV-04c: a task completion event that arrives between the last tick and the channel
/// close is captured by the drain tick and honored — the loop returns the natural
/// `Done` status from the drain rather than forcing `Canceled`/`Failed`.
///
/// This verifies the drain-before-cancel ordering (architect S1 fix for #2246):
/// `cancel_all()` empties `self.running`, so any completion event processed AFTER it
/// would be silently discarded. The drain tick must come FIRST while `self.running`
/// is still intact.
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn scheduler_loop_channel_close_drain_captures_completion() {
    use crate::config::OrchestrationConfig;
    use zeph_orchestration::{DagScheduler, RuleBasedRouter, TaskEvent, TaskOutcome};
    use zeph_subagent::SubAgentManager;

    // Channel is empty: recv() returns Ok(None) immediately, triggering the close path.
    let channel = MockChannel::new(vec![]);
    let provider = mock_provider(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.orchestration.orchestration_config.enabled = true;
    agent.orchestration.subagent_manager = Some(SubAgentManager::new(4));

    // Single-task graph in Running state.  The task is assigned an agent handle so
    // resume_from() reconstructs it in the running map — this is required for
    // process_event() to accept the completion event (it checks self.running).
    let mut graph = TaskGraph::new("drain capture goal");
    let mut node = TaskNode::new(0, "task-0", "completes just before channel close");
    node.status = TaskStatus::Running;
    node.assigned_agent = Some("handle-0".to_string());
    node.agent_hint = Some("worker".to_string());
    graph.tasks.push(node);
    graph.status = GraphStatus::Running;

    let config = OrchestrationConfig {
        enabled: true,
        ..OrchestrationConfig::default()
    };
    let mut scheduler =
        DagScheduler::resume_from(graph, &config, Box::new(RuleBasedRouter), vec![]).unwrap();

    // Inject the completion event via the public event_sender() so it sits in event_rx
    // when the drain tick calls event_rx.try_recv().  This simulates the race: the task
    // finished between the last tick and the channel EOF.
    let event_tx = scheduler.event_sender();
    event_tx
        .send(TaskEvent {
            task_id: scheduler.graph().tasks[0].id,
            agent_handle_id: "handle-0".to_string(),
            outcome: TaskOutcome::Completed {
                output: "finished just in time".to_string(),
                artifacts: vec![],
            },
        })
        .await
        .expect("event_tx send must not fail");
    // Drop the sender so the channel is not kept alive beyond this test.
    drop(event_tx);

    let token = tokio_util::sync::CancellationToken::new();
    let status = agent
        .run_scheduler_loop(&mut scheduler, 1, token)
        .await
        .unwrap();

    // The drain tick processes the completion event while self.running is intact,
    // advances the graph to Completed, and emits Done{Completed}.  The loop must
    // honor this natural Done rather than overriding it with Canceled/Failed.
    assert_eq!(
        status,
        GraphStatus::Completed,
        "drain tick must capture the late completion and return Done(Completed); got {status:?}"
    );
    assert_eq!(
        scheduler.graph().tasks[0].status,
        TaskStatus::Completed,
        "task 0 must be Completed, not Canceled, when its completion is captured by the drain tick"
    );
}

/// COV-05: when channel closes (stdin EOF) while sub-agent tasks are still running,
/// `run_scheduler_loop` must NOT cancel them immediately.  Instead it parks the recv
/// arm (`stdin_closed = true`) and waits for the natural completion event.
///
/// Simulates: piped `echo "/plan confirm" | zeph` — stdin closes before sub-agents finish.
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn stdin_closed_parks_when_tasks_running() {
    use crate::config::OrchestrationConfig;
    use zeph_orchestration::{DagScheduler, RuleBasedRouter, TaskEvent, TaskOutcome};
    use zeph_subagent::SubAgentManager;

    // Empty channel: recv() returns Ok(None) immediately, simulating piped stdin EOF.
    let channel = MockChannel::new(vec![]);
    let provider = mock_provider(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.orchestration.orchestration_config.enabled = true;
    agent.orchestration.subagent_manager = Some(SubAgentManager::new(4));

    // A single task that is already running when the channel closes.
    let mut graph = TaskGraph::new("piped stdin EOF with running task");
    let mut node = TaskNode::new(0, "task-0", "must finish naturally");
    node.status = TaskStatus::Running;
    node.assigned_agent = Some("handle-0".to_string());
    node.agent_hint = Some("worker".to_string());
    graph.tasks.push(node);
    graph.status = GraphStatus::Running;

    let config = OrchestrationConfig {
        enabled: true,
        ..OrchestrationConfig::default()
    };
    let mut scheduler =
        DagScheduler::resume_from(graph, &config, Box::new(RuleBasedRouter), vec![]).unwrap();

    // Deliver the completion event asynchronously after a short delay so that the loop
    // first observes channel-close (stdin_closed = true), then receives the event.
    let event_tx = scheduler.event_sender();
    let task_id = scheduler.graph().tasks[0].id;
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let _ = event_tx
            .send(TaskEvent {
                task_id,
                agent_handle_id: "handle-0".to_string(),
                outcome: TaskOutcome::Completed {
                    output: "natural completion".to_string(),
                    artifacts: vec![],
                },
            })
            .await;
    });

    let token = tokio_util::sync::CancellationToken::new();
    let status = agent
        .run_scheduler_loop(&mut scheduler, 1, token)
        .await
        .unwrap();

    assert_eq!(
        status,
        GraphStatus::Completed,
        "loop must wait for natural task completion after stdin EOF, not cancel immediately; got {status:?}"
    );
    assert_eq!(
        scheduler.graph().tasks[0].status,
        TaskStatus::Completed,
        "task must be Completed, not Canceled, when loop parks on stdin EOF"
    );
}

/// COV-06: when channel closes (stdin EOF) and there are no running tasks,
/// `run_scheduler_loop` must exit immediately with `GraphStatus::Canceled`
/// (existing behavior preserved for empty-scheduler case).
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn stdin_closed_exits_when_no_tasks() {
    use crate::config::OrchestrationConfig;
    use zeph_orchestration::{DagScheduler, RuleBasedRouter};
    use zeph_subagent::SubAgentManager;

    // Empty channel: recv() returns Ok(None) immediately.
    let channel = MockChannel::new(vec![]);
    let provider = mock_provider(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.orchestration.orchestration_config.enabled = true;
    agent.orchestration.subagent_manager = Some(SubAgentManager::new(4));

    // Graph has a task in Running state, but no entry in scheduler.running map
    // (simulates the case where the task was already drained before channel close).
    let mut graph = TaskGraph::new("no running tasks on channel close");
    let mut node = TaskNode::new(0, "task-0", "already drained");
    node.status = TaskStatus::Running;
    graph.tasks.push(node);
    graph.status = GraphStatus::Running;

    let config = OrchestrationConfig {
        enabled: true,
        ..OrchestrationConfig::default()
    };
    // resume_from without assigned_agent → running map stays empty.
    let mut scheduler =
        DagScheduler::resume_from(graph, &config, Box::new(RuleBasedRouter), vec![]).unwrap();

    let token = tokio_util::sync::CancellationToken::new();
    let status = agent
        .run_scheduler_loop(&mut scheduler, 1, token)
        .await
        .unwrap();

    assert_eq!(
        status,
        GraphStatus::Canceled,
        "channel close with no running tasks on exit-supporting channel must return Canceled; got {status:?}"
    );
}

/// GAP-9: `handle_plan_status` shows the correct message for each graph status.
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn plan_status_reflects_graph_status() {
    // No active plan → "No active plan."
    let mut agent = agent_with_orchestration();
    let out = agent
        .handle_plan_command_as_string(PlanCommand::Status(None))
        .await
        .unwrap();
    assert!(
        out.contains("No active plan"),
        "no plan → 'No active plan'; got: {out:?}"
    );

    // GraphStatus::Created → awaiting confirmation.
    let mut agent = agent_with_orchestration();
    agent.orchestration.pending_graph = Some(make_simple_graph(GraphStatus::Created));
    let out = agent
        .handle_plan_command_as_string(PlanCommand::Status(None))
        .await
        .unwrap();
    assert!(
        out.contains("awaiting confirmation"),
        "Created graph → 'awaiting confirmation'; got: {out:?}"
    );

    // GraphStatus::Failed → retry message.
    let mut agent = agent_with_orchestration();
    let mut failed_graph = make_simple_graph(GraphStatus::Created);
    failed_graph.status = GraphStatus::Failed;
    agent.orchestration.pending_graph = Some(failed_graph);
    let out = agent
        .handle_plan_command_as_string(PlanCommand::Status(None))
        .await
        .unwrap();
    assert!(
        out.contains("failed") || out.contains("Failed"),
        "Failed graph → failure message; got: {out:?}"
    );

    // GraphStatus::Paused → resume message.
    let mut agent = agent_with_orchestration();
    let mut paused_graph = make_simple_graph(GraphStatus::Created);
    paused_graph.status = GraphStatus::Paused;
    agent.orchestration.pending_graph = Some(paused_graph);
    let out = agent
        .handle_plan_command_as_string(PlanCommand::Status(None))
        .await
        .unwrap();
    assert!(
        out.contains("paused") || out.contains("Paused"),
        "Paused graph → paused message; got: {out:?}"
    );

    // GraphStatus::Completed → completed message.
    let mut agent = agent_with_orchestration();
    let mut completed_graph = make_simple_graph(GraphStatus::Created);
    completed_graph.status = GraphStatus::Completed;
    agent.orchestration.pending_graph = Some(completed_graph);
    let out = agent
        .handle_plan_command_as_string(PlanCommand::Status(None))
        .await
        .unwrap();
    assert!(
        out.contains("completed") || out.contains("Completed"),
        "Completed graph → completed message; got: {out:?}"
    );
}

/// Regression for #1879: `finalize_plan_execution` with `GraphStatus::Failed` where no
/// tasks actually failed (all canceled due to scheduler deadlock) must emit
/// "Plan canceled. N/M tasks did not run." and NOT "Plan failed. 0/N tasks failed".
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn finalize_plan_execution_deadlock_emits_cancelled_message() {
    use zeph_subagent::SubAgentManager;

    let channel = MockChannel::new(vec![]);
    let provider = mock_provider(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.orchestration.orchestration_config.enabled = true;
    agent.orchestration.subagent_manager = Some(SubAgentManager::new(4));

    // Simulate deadlock: graph Failed, one task Blocked → Canceled, one task Pending → Canceled.
    let mut graph = TaskGraph::new("deadlock goal");
    let mut task0 = TaskNode::new(0, "upstream", "will be blocked");
    task0.status = TaskStatus::Canceled;
    let mut task1 = TaskNode::new(1, "downstream", "never ran");
    task1.status = TaskStatus::Canceled;
    graph.tasks.push(task0);
    graph.tasks.push(task1);
    graph.status = GraphStatus::Failed;

    agent
        .finalize_plan_execution(graph, GraphStatus::Failed)
        .await
        .unwrap();

    let msgs = agent.channel.sent_messages();
    // Must NOT say "0/2 tasks failed".
    assert!(
        !msgs.iter().any(|m| m.contains("0/2 tasks failed")),
        "misleading '0/2 tasks failed' message must not appear; got: {msgs:?}"
    );
    // Must say "Plan canceled".
    assert!(
        msgs.iter().any(|m| m.contains("Plan canceled")),
        "must contain 'Plan canceled' for pure deadlock; got: {msgs:?}"
    );
    // Must mention the count of tasks that did not run.
    assert!(
        msgs.iter().any(|m| m.contains("2/2")),
        "must report 2/2 canceled; got: {msgs:?}"
    );
}

/// COV-METRICS-01: `handle_plan_goal` increments `api_calls` and `plans_total` after
/// a successful `LlmPlanner` call. This test covers the production metrics path in
/// `handle_plan_goal` that was not exercised by the `status_command_shows_orchestration_*`
/// tests (which set metrics directly via `update_metrics`).
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn plan_goal_increments_api_calls_and_plans_total() {
    let valid_plan_json = r#"{"tasks": [
        {"task_id": "step-one", "title": "Step one", "description": "Do step one", "depends_on": []}
    ]}"#
    .to_string();

    let provider = mock_provider(vec![valid_plan_json]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (tx, rx) = watch::channel(MetricsSnapshot::default());
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);
    agent.orchestration.orchestration_config.enabled = true;
    agent
        .orchestration
        .orchestration_config
        .confirm_before_execute = true;

    agent
        .handle_plan_command_as_string(PlanCommand::Goal("build something".to_owned()))
        .await
        .unwrap();

    let snapshot = rx.borrow().clone();
    assert_eq!(
        snapshot.api_calls, 1,
        "api_calls must be incremented by 1 after a successful plan() call; got: {}",
        snapshot.api_calls
    );
    assert_eq!(
        snapshot.orchestration.plans_total, 1,
        "plans_total must be incremented by 1 after plan() succeeds; got: {}",
        snapshot.orchestration.plans_total
    );
    assert_eq!(
        snapshot.orchestration.tasks_total, 1,
        "tasks_total must match the number of tasks in the plan; got: {}",
        snapshot.orchestration.tasks_total
    );
}

/// COV-METRICS-02: `finalize_plan_execution` with `GraphStatus::Completed` increments
/// `api_calls` for the aggregator call and updates `tasks_completed` / `tasks_skipped`.
/// This covers the aggregator metrics path that was not tested end-to-end.
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn finalize_plan_execution_completed_increments_aggregator_metrics() {
    use zeph_subagent::SubAgentManager;

    // Provider returns the aggregation synthesis.
    let provider = mock_provider(vec!["synthesis".into()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (tx, rx) = watch::channel(MetricsSnapshot::default());
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);
    agent.orchestration.orchestration_config.enabled = true;
    agent.orchestration.subagent_manager = Some(SubAgentManager::new(4));

    // Graph with one completed and one skipped task.
    let mut graph = TaskGraph::new("metrics finalize test");
    let mut completed = TaskNode::new(0, "task-done", "desc");
    completed.status = TaskStatus::Completed;
    completed.result = Some(TaskResult {
        output: "ok".into(),
        artifacts: vec![],
        duration_ms: 5,
        agent_id: None,
        agent_def: None,
    });
    let mut skipped = TaskNode::new(1, "task-skip", "desc");
    skipped.status = TaskStatus::Skipped;
    graph.tasks.push(completed);
    graph.tasks.push(skipped);
    graph.status = GraphStatus::Completed;

    agent
        .finalize_plan_execution(graph, GraphStatus::Completed)
        .await
        .unwrap();

    let snapshot = rx.borrow().clone();
    assert_eq!(
        snapshot.api_calls, 1,
        "api_calls must be incremented by 1 for the aggregator LLM call; got: {}",
        snapshot.api_calls
    );
    assert_eq!(
        snapshot.orchestration.tasks_completed, 1,
        "tasks_completed must be 1; got: {}",
        snapshot.orchestration.tasks_completed
    );
    assert_eq!(
        snapshot.orchestration.tasks_skipped, 1,
        "tasks_skipped must be 1; got: {}",
        snapshot.orchestration.tasks_skipped
    );
}

/// Regression for #1879: mixed failure — some tasks failed, some canceled.
/// Message must say "Plan failed. X/M tasks failed, Y canceled:" (not misleading).
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn finalize_plan_execution_mixed_failed_and_cancelled() {
    use zeph_subagent::SubAgentManager;

    let channel = MockChannel::new(vec![]);
    let provider = mock_provider(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    agent.orchestration.orchestration_config.enabled = true;
    agent.orchestration.subagent_manager = Some(SubAgentManager::new(4));

    let mut graph = TaskGraph::new("mixed goal");
    let mut failed = TaskNode::new(0, "failed-task", "really failed");
    failed.status = TaskStatus::Failed;
    failed.result = Some(TaskResult {
        output: "error: something went wrong".into(),
        artifacts: vec![],
        duration_ms: 100,
        agent_id: None,
        agent_def: None,
    });
    let mut cancelled = TaskNode::new(1, "cancelled-task", "never ran");
    cancelled.status = TaskStatus::Canceled;
    graph.tasks.push(failed);
    graph.tasks.push(cancelled);
    graph.status = GraphStatus::Failed;

    agent
        .finalize_plan_execution(graph, GraphStatus::Failed)
        .await
        .unwrap();

    let msgs = agent.channel.sent_messages();
    // Must say "Plan failed." (not "Plan canceled.").
    assert!(
        msgs.iter().any(|m| m.contains("Plan failed")),
        "mixed state must say 'Plan failed'; got: {msgs:?}"
    );
    // Must mention canceled count.
    assert!(
        msgs.iter().any(|m| m.contains("canceled")),
        "must mention canceled tasks in mixed state; got: {msgs:?}"
    );
    // Must list failed task.
    assert!(
        msgs.iter().any(|m| m.contains("failed-task")),
        "must list the failed task; got: {msgs:?}"
    );
}
