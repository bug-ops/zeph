// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![expect(
    clippy::large_futures,
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "The test exercises the concrete async path without boxing its future.; Fixture construction failures should fail the test immediately."
)]
#![recursion_limit = "256"]

//! Acceptance tests (tests-first) for the model-callable `ask_user` tool inside the agent loop
//! (spec 102 FR-001/004/005/007/008/009, NFR-003; plan r2 Track A, SC-003/SC-004).
//!
//! A scripted `Channel` stands in for every real channel; it implements the planned
//! `Channel::ask_user` method and counts / records the questions it receives.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zeph_core::channel::{
    AskUserOutcome, AskUserRequest, Channel, ChannelError, ChannelMessage, DeclineReason,
};
use zeph_core::{Agent, InteractionConfig};
use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::{ChatResponse, Message, ToolUseRequest};
use zeph_skills::registry::SkillRegistry;
use zeph_tools::executor::{ToolError, ToolExecutor, ToolOutput};

/// What the scripted channel does when asked.
enum Reply {
    Outcome(AskUserOutcome),
    /// Never resolves; sets `dropped` when the future is dropped (cancel-safety probe).
    Pending(Arc<std::sync::atomic::AtomicBool>),
}

struct AskChannel {
    inbox: VecDeque<String>,
    replies: Mutex<VecDeque<Reply>>,
    asked: Arc<Mutex<Vec<String>>>,
}

struct DropFlag(Arc<std::sync::atomic::AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl AskChannel {
    fn new(replies: Vec<Reply>) -> (Self, Arc<Mutex<Vec<String>>>) {
        let asked = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                inbox: VecDeque::from(["please ask me".to_owned()]),
                replies: Mutex::new(replies.into()),
                asked: Arc::clone(&asked),
            },
            asked,
        )
    }
}

impl Channel for AskChannel {
    fn ask_user_support(&self) -> zeph_core::channel::AskUserSupport {
        zeph_core::channel::AskUserSupport::Interactive
    }

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "recv implements the trait-required async signature while completing synchronously"
    )]
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
        Ok(self.inbox.pop_front().map(|text| ChannelMessage {
            text,
            attachments: vec![],
            is_guest_context: false,
            is_from_bot: false,
            owner_key: None,
        }))
    }

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "send implements the trait-required async signature while completing synchronously"
    )]
    async fn send(&mut self, _text: &str) -> Result<(), ChannelError> {
        Ok(())
    }

    #[expect(
        clippy::unused_async_trait_impl,
        reason = "send_chunk implements the trait-required async signature while completing synchronously"
    )]
    async fn send_chunk(&mut self, _chunk: &str) -> Result<(), ChannelError> {
        Ok(())
    }

    fn flush_chunks(&mut self) -> impl Future<Output = Result<(), ChannelError>> + Send {
        std::future::ready(Ok(()))
    }

    fn ask_user(
        &mut self,
        request: AskUserRequest,
    ) -> impl Future<Output = Result<AskUserOutcome, ChannelError>> + Send {
        self.asked.lock().unwrap().push(format!("{request:?}"));
        let reply = self.replies.lock().unwrap().pop_front();
        async move {
            match reply {
                Some(Reply::Outcome(o)) => Ok(o),
                Some(Reply::Pending(flag)) => {
                    let _guard = DropFlag(flag);
                    std::future::pending::<()>().await;
                    unreachable!()
                }
                None => Ok(AskUserOutcome::Declined(DeclineReason::NonInteractive)),
            }
        }
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
    fn set_skill_env(&self, _env: Option<std::collections::HashMap<String, String>>) {}
    zeph_tools::tool_executor_no_inner_defaults!();
}

fn ask_call(id: &str) -> ChatResponse {
    ChatResponse::tool_use(
        None,
        vec![ToolUseRequest {
            id: id.into(),
            name: "ask_user".into(),
            input: serde_json::json!({
                "question": "Which target?",
                "options": ["staging", "production"]
            }),
        }],
    )
}

fn enabled(cfg: impl FnOnce(&mut zeph_core::config::Config)) -> InteractionConfig {
    let mut c = zeph_core::config::Config::default();
    c.tools.ask_user.enabled = true;
    cfg(&mut c);
    InteractionConfig::from_config(&c)
}

struct Run {
    /// Messages of every recorded provider call.
    calls: Vec<Vec<Message>>,
    tool_defs: Vec<String>,
    asked: Vec<String>,
}

async fn run_agent(
    interaction: InteractionConfig,
    script: Vec<ChatResponse>,
    channel_replies: Vec<Reply>,
) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
    let (mock, _count) = MockProvider::default().with_tool_use(script);
    let (mock, calls) = mock.with_recording();
    let (mock, defs) = mock.with_tool_recording();
    let (channel, asked) = AskChannel::new(channel_replies);
    let mut agent = Agent::new(
        AnyProvider::Mock(mock),
        channel,
        registry,
        None,
        5,
        NoopExecutor,
    )
    .with_interaction_config(interaction);
    agent.run().await.expect("agent run");
    let tool_defs = defs
        .lock()
        .unwrap()
        .iter()
        .map(|d| format!("{d:?}"))
        .collect();
    let calls = calls.lock().unwrap().clone();
    let asked = asked.lock().unwrap().clone();
    Run {
        calls,
        tool_defs,
        asked,
    }
}

fn flat(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|m| format!("{} {:?}", m.content, m.parts))
        .collect::<Vec<_>>()
        .join("\n")
}

fn last_call_text(run: &Run) -> String {
    flat(run.calls.last().expect("provider was called"))
}

// ---- FR-007 / SC-004: toggle controls tool listing --------------------------------------

#[tokio::test]
async fn ask_user_tool_absent_from_tool_list_when_disabled() {
    let run = run_agent(
        InteractionConfig::from_config(&zeph_core::config::Config::default()),
        vec![ChatResponse::text("done".to_owned())],
        vec![],
    )
    .await;
    assert!(
        !run.tool_defs.is_empty(),
        "provider saw at least one tool list"
    );
    assert!(run.tool_defs.iter().all(|d| !d.contains("ask_user")));
}

#[tokio::test]
async fn ask_user_tool_listed_when_enabled() {
    let run = run_agent(
        enabled(|_| {}),
        vec![ChatResponse::text("done".to_owned())],
        vec![],
    )
    .await;
    assert!(run.tool_defs.iter().any(|d| d.contains("ask_user")));
}

// ---- FR-001: answer returns as the tool result ---------------------------------------------

#[tokio::test]
async fn ask_user_answered_outcome_reaches_model_as_tool_result() {
    let run = run_agent(
        enabled(|_| {}),
        vec![ask_call("call1"), ChatResponse::text("ok".to_owned())],
        vec![Reply::Outcome(AskUserOutcome::answered_text("staging"))],
    )
    .await;
    assert_eq!(run.asked.len(), 1);
    assert!(run.asked[0].contains("Which target?"));
    let seen = last_call_text(&run);
    assert!(
        seen.contains("staging"),
        "tool result missing answer: {seen}"
    );
    assert!(seen.contains("call1"), "tool_use id must be paired: {seen}");
}

// ---- SC-003: pairing stays valid for all four outcomes ----------------------------------------

#[tokio::test]
async fn ask_user_non_answer_outcomes_still_pair_tool_result_and_turn_completes() {
    for outcome in [
        AskUserOutcome::Declined(DeclineReason::NonInteractive),
        AskUserOutcome::Cancelled,
        AskUserOutcome::TimedOut,
    ] {
        let run = run_agent(
            enabled(|_| {}),
            vec![ask_call("call7"), ChatResponse::text("ok".to_owned())],
            vec![Reply::Outcome(outcome.clone())],
        )
        .await;
        let seen = last_call_text(&run);
        assert!(
            seen.contains("call7"),
            "outcome {outcome:?}: no paired tool result in {seen}"
        );
        assert_eq!(
            run.calls.len(),
            2,
            "outcome {outcome:?}: model gets a follow-up call"
        );
    }
}

// ---- S5: secrets scrubbed before the model sees them -------------------------------------------

#[tokio::test]
async fn ask_user_answer_with_secret_is_redacted_before_model_and_history() {
    let secret = "sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789";
    let run = run_agent(
        enabled(|_| {}),
        vec![ask_call("call1"), ChatResponse::text("ok".to_owned())],
        vec![Reply::Outcome(AskUserOutcome::answered_text(&format!(
            "use {secret} please"
        )))],
    )
    .await;
    for call in &run.calls {
        assert!(
            !flat(call).contains(secret),
            "raw secret leaked into provider input"
        );
    }
}

// ---- FR-009: per-turn cap ----------------------------------------------------------------------

#[tokio::test]
async fn ask_user_calls_beyond_per_turn_cap_do_not_reach_the_channel() {
    let run = run_agent(
        enabled(|c| c.tools.ask_user.max_questions_per_turn = std::num::NonZeroU8::new(1).unwrap()),
        vec![
            ask_call("call1"),
            ask_call("call2"),
            ChatResponse::text("ok".to_owned()),
        ],
        vec![
            Reply::Outcome(AskUserOutcome::answered_text("staging")),
            Reply::Outcome(AskUserOutcome::answered_text("production")),
        ],
    )
    .await;
    assert_eq!(
        run.asked.len(),
        1,
        "second question must be refused by the cap"
    );
    assert!(
        last_call_text(&run).contains("call2"),
        "refusal is still paired"
    );
}

// ---- FR-008 + cancel safety: timeout owned by the agent --------------------------------------------

#[tokio::test(start_paused = true)]
async fn ask_user_pending_question_times_out_and_drops_channel_future() {
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let run = tokio::time::timeout(
        Duration::from_secs(600),
        run_agent(
            enabled(|c| {
                c.tools.ask_user.timeout_secs = zeph_config::AskUserTimeout::new(2).unwrap();
            }),
            vec![ask_call("call1"), ChatResponse::text("ok".to_owned())],
            vec![Reply::Pending(Arc::clone(&dropped))],
        ),
    )
    .await
    .expect("agent must not hang on an unanswered question");
    assert!(
        dropped.load(std::sync::atomic::Ordering::SeqCst),
        "timeout must drop the channel's ask_user future"
    );
    assert!(last_call_text(&run).contains("call1"));
}
