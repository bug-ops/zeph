// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::large_futures, clippy::unwrap_used, clippy::expect_used)]
#![recursion_limit = "256"]

//! Acceptance tests (tests-first) for #7136: a dropped agent leaves no parked cancel-bridge task
//! in the supervisor registry, and the speculative sweeper is scoped and removed with its engine.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use zeph_common::TaskSupervisor;
use zeph_core::Agent;
use zeph_core::agent::speculative::SpeculationEngine;
use zeph_core::channel::{Channel, ChannelError, ChannelMessage};
use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_skills::registry::SkillRegistry;
use zeph_tools::executor::{ToolError, ToolExecutor, ToolOutput};

struct OneShotChannel(VecDeque<String>);

impl Channel for OneShotChannel {
    #[allow(clippy::unused_async_trait_impl)]
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
        Ok(self.0.pop_front().map(|text| ChannelMessage {
            text,
            attachments: vec![],
            is_guest_context: false,
            is_from_bot: false,
            owner_key: None,
        }))
    }
    #[allow(clippy::unused_async_trait_impl)]
    async fn send(&mut self, _text: &str) -> Result<(), ChannelError> {
        Ok(())
    }
    #[allow(clippy::unused_async_trait_impl)]
    async fn send_chunk(&mut self, _chunk: &str) -> Result<(), ChannelError> {
        Ok(())
    }
    fn flush_chunks(&mut self) -> impl Future<Output = Result<(), ChannelError>> + Send {
        std::future::ready(Ok(()))
    }
}

struct NoopExecutor;
impl ToolExecutor for NoopExecutor {
    fn execute(
        &self,
        _response: &str,
    ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
        std::future::ready(Ok(None))
    }
    fn set_skill_env(&self, _env: Option<HashMap<String, String>>) {}
    zeph_tools::tool_executor_no_inner_defaults!();
}

fn live(sup: &TaskSupervisor, needle: &str) -> usize {
    sup.snapshot()
        .iter()
        .filter(|t| t.name.contains(needle))
        .count()
}

async fn eventually(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..100 {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    cond()
}

fn one_turn_agent(sup: &Arc<TaskSupervisor>, scope: &str) -> Agent<OneShotChannel> {
    let dir = tempfile::tempdir().unwrap();
    let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
    Agent::new(
        AnyProvider::Mock(MockProvider::default()),
        OneShotChannel(VecDeque::from(["hello".to_owned()])),
        registry,
        None,
        5,
        NoopExecutor,
    )
    .with_task_supervisor(Arc::new(sup.scoped(scope)))
}

#[tokio::test]
async fn dropped_agent_leaves_no_cancel_bridge_entry() {
    let sup = Arc::new(TaskSupervisor::new(CancellationToken::new()));
    let mut agent = one_turn_agent(&sup, "s1");
    agent.run().await.expect("agent run");
    assert_eq!(
        live(&sup, "cancel_bridge"),
        1,
        "bridge parks while the agent lives"
    );
    drop(agent);
    assert!(
        eventually(|| live(&sup, "cancel_bridge") == 0).await,
        "parked cancel bridge must be aborted and reaped when the agent drops"
    );
}

#[tokio::test]
async fn dropped_agent_cancel_signal_does_not_resurrect_a_bridge() {
    let sup = Arc::new(TaskSupervisor::new(CancellationToken::new()));
    let mut agent = one_turn_agent(&sup, "s1");
    agent.run().await.expect("agent run");
    let signal = agent.cancel_signal();
    drop(agent);
    assert!(eventually(|| live(&sup, "cancel_bridge") == 0).await);
    signal.notify_waiters();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(live(&sup, "cancel_bridge"), 0);
}

#[tokio::test]
async fn many_scoped_agents_dropped_leave_zero_bridge_entries() {
    let sup = Arc::new(TaskSupervisor::new(CancellationToken::new()));
    for i in 0..20 {
        let mut agent = one_turn_agent(&sup, &format!("s{i}"));
        agent.run().await.expect("agent run");
    }
    assert!(
        eventually(|| live(&sup, "cancel_bridge") == 0).await,
        "{} bridge entries leaked",
        live(&sup, "cancel_bridge")
    );
}

fn engine(view: Arc<TaskSupervisor>) -> SpeculationEngine {
    let executor: Arc<dyn zeph_tools::ErasedToolExecutor> = Arc::new(NoopExecutor);
    let config: zeph_config::tools::SpeculativeConfig =
        serde_json::from_value(serde_json::json!({})).unwrap();
    SpeculationEngine::new_with_supervisor(executor, config, Some(view))
}

#[tokio::test]
async fn speculative_sweepers_on_two_scoped_views_both_stay_registered() {
    let sup = Arc::new(TaskSupervisor::new(CancellationToken::new()));
    let a = engine(Arc::new(sup.scoped("a")));
    let b = engine(Arc::new(sup.scoped("b")));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(live(&sup, "speculative.sweeper"), 2);
    drop(a);
    assert!(eventually(|| live(&sup, "speculative.sweeper") == 1).await);
    drop(b);
    assert!(eventually(|| live(&sup, "speculative.sweeper") == 0).await);
}
