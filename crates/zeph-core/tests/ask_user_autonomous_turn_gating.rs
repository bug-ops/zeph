// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::large_futures, clippy::unwrap_used, clippy::expect_used)]
#![recursion_limit = "256"]

//! Acceptance test for #7122: an autonomous goal turn (`TurnOrigin::Injected`) is never offered
//! `ask_user`, while a following user turn on the same interactive channel is.
//!
//! The session is started through the real `/goal create ... --auto` path and paced by the
//! driver's own tick; the channel releases the follow-up user message only once the autonomous
//! turn has reached the provider, so there is no wall-clock assumption.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zeph_core::channel::{
    AskUserOutcome, AskUserRequest, AskUserSupport, Channel, ChannelError, ChannelMessage,
};
use zeph_core::{Agent, AgentSessionConfig, InteractionConfig};
use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::{ChatResponse, ToolDefinition};
use zeph_skills::registry::SkillRegistry;
use zeph_tools::executor::{ToolError, ToolExecutor, ToolOutput};

type Defs = Arc<Mutex<Vec<Vec<ToolDefinition>>>>;

struct GatedChannel {
    steps: VecDeque<String>,
    defs: Defs,
}

impl Channel for GatedChannel {
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
        let Some(next) = self.steps.front() else {
            return Ok(None);
        };
        if !next.starts_with('/') {
            while self.defs.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        let text = self.steps.pop_front().expect("front checked above");
        Ok(Some(ChannelMessage {
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

    fn ask_user_support(&self) -> AskUserSupport {
        AskUserSupport::Interactive
    }

    fn ask_user(
        &mut self,
        _request: AskUserRequest,
    ) -> impl Future<Output = Result<AskUserOutcome, ChannelError>> + Send {
        std::future::ready(Ok(AskUserOutcome::answered_text("x")))
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

#[tokio::test]
async fn autonomous_goal_turn_is_not_offered_ask_user_but_a_user_turn_is() {
    let mut cfg = zeph_core::config::Config::default();
    cfg.tools.ask_user.enabled = true;
    cfg.goals.enabled = true;
    cfg.goals.autonomous_enabled = true;
    cfg.goals.autonomous_max_turns = 1;
    cfg.goals.autonomous_turn_delay_ms = 10;
    cfg.goals.verify_interval = 1000;

    let dir = tempfile::tempdir().unwrap();
    let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
    let (mock, _count) = MockProvider::default().with_tool_use(vec![
        ChatResponse::text("working"),
        ChatResponse::text("working"),
        ChatResponse::text("working"),
    ]);
    let (mock, defs) = mock.with_tool_recording();

    let memory = zeph_memory::semantic::SemanticMemory::with_sqlite_backend(
        ":memory:",
        AnyProvider::Mock(MockProvider::default()),
        "embed",
        0.7,
        0.3,
    )
    .await
    .unwrap();
    let cid = memory.sqlite().create_conversation().await.unwrap();

    let channel = GatedChannel {
        steps: VecDeque::from([
            "/goal create finish the report --auto".to_owned(),
            "follow-up question".to_owned(),
        ]),
        defs: Arc::clone(&defs),
    };
    let mut agent = Agent::new(
        AnyProvider::Mock(mock),
        channel,
        registry,
        None,
        5,
        NoopExecutor,
    )
    .apply_session_config(AgentSessionConfig::from_config(&cfg, 100_000))
    .with_interaction_config(InteractionConfig::from_config(&cfg))
    .with_memory(Arc::new(memory), cid, 50, 5, 10_000);

    tokio::time::timeout(Duration::from_secs(20), agent.run())
        .await
        .expect("agent run must finish")
        .expect("agent run");

    let calls: Vec<String> = defs
        .lock()
        .unwrap()
        .iter()
        .map(|tools| format!("{tools:?}"))
        .collect();
    assert_eq!(
        calls.len(),
        2,
        "one autonomous turn and one user turn: {calls:?}"
    );
    assert!(
        calls[0].contains("compress_context") && !calls[0].contains("ask_user"),
        "ask_user must not be offered to the autonomous goal turn"
    );
    assert!(
        calls[1].contains("ask_user"),
        "ask_user must be offered to the user turn on an interactive channel"
    );
}
