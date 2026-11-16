// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::large_futures, clippy::unwrap_used, clippy::expect_used)]
#![recursion_limit = "256"]

//! Acceptance tests (tests-first) for #7122 (`ask_user` is offered only for a User-origin turn on
//! a channel whose `ask_user_support()` is `Interactive`) and #7123 (`[tools.permissions]` deny
//! rules are enforced for agent built-in tools, listing and dispatch).
//!
//! Assumed API: `zeph_core::channel::AskUserSupport { Interactive, Unavailable }` and
//! `Channel::ask_user_support(&self) -> AskUserSupport` (default `Unavailable`).

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};

use zeph_config::tools::{AutonomyLevel, PermissionAction, PermissionRule, PermissionsConfig};
use zeph_core::channel::{
    AskUserOutcome, AskUserRequest, AskUserSupport, Channel, ChannelError, ChannelMessage,
};
use zeph_core::{Agent, AgentSessionConfig, InteractionConfig};
use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::{ChatResponse, Message, ToolUseRequest};
use zeph_skills::registry::SkillRegistry;
use zeph_tools::executor::{ToolError, ToolExecutor, ToolOutput};

struct ScriptedChannel {
    inbox: VecDeque<ChannelMessage>,
    support: AskUserSupport,
    asked: Arc<Mutex<Vec<String>>>,
}

impl Channel for ScriptedChannel {
    #[allow(clippy::unused_async_trait_impl)]
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
        Ok(self.inbox.pop_front())
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
        self.support
    }

    fn ask_user(
        &mut self,
        request: AskUserRequest,
    ) -> impl Future<Output = Result<AskUserOutcome, ChannelError>> + Send {
        self.asked.lock().unwrap().push(format!("{request:?}"));
        std::future::ready(Ok(AskUserOutcome::answered_text("staging")))
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

struct Setup {
    support: AskUserSupport,
    from_bot: bool,
    autonomy: AutonomyLevel,
    rules: Vec<(&'static str, Vec<(&'static str, PermissionAction)>)>,
    max_questions: u8,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            support: AskUserSupport::Interactive,
            from_bot: false,
            autonomy: AutonomyLevel::Supervised,
            rules: Vec::new(),
            max_questions: 3,
        }
    }
}

struct Run {
    calls: Vec<Vec<Message>>,
    tool_defs: Vec<String>,
    asked: Vec<String>,
}

impl Run {
    fn listed(&self, tool: &str) -> bool {
        self.tool_defs.iter().any(|d| d.contains(tool))
    }
    fn last_text(&self) -> String {
        self.calls
            .last()
            .expect("provider was called")
            .iter()
            .map(|m| format!("{} {:?}", m.content, m.parts))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn tool_call(id: &str, name: &str, input: serde_json::Value) -> ChatResponse {
    ChatResponse::tool_use(
        None,
        vec![ToolUseRequest {
            id: id.into(),
            name: name.into(),
            input,
        }],
    )
}

fn ask(id: &str, question: &str) -> ChatResponse {
    tool_call(id, "ask_user", serde_json::json!({ "question": question }))
}

async fn run(setup: Setup, script: Vec<ChatResponse>) -> Run {
    let mut cfg = zeph_core::config::Config::default();
    cfg.tools.ask_user.enabled = true;
    cfg.tools.ask_user.max_questions_per_turn =
        std::num::NonZeroU8::new(setup.max_questions).unwrap();
    cfg.security.autonomy_level = setup.autonomy;
    if !setup.rules.is_empty() {
        cfg.tools.permissions = Some(PermissionsConfig {
            tools: setup
                .rules
                .iter()
                .map(|(tool, rules)| {
                    (
                        (*tool).to_owned(),
                        rules
                            .iter()
                            .map(|(pattern, action)| PermissionRule {
                                pattern: (*pattern).to_owned(),
                                action: *action,
                            })
                            .collect(),
                    )
                })
                .collect(),
        });
    }
    let dir = tempfile::tempdir().unwrap();
    let registry = SkillRegistry::load(&[dir.path().to_path_buf()]);
    let (mock, _count) = MockProvider::default().with_tool_use(script);
    let (mock, calls) = mock.with_recording();
    let (mock, defs) = mock.with_tool_recording();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let channel = ScriptedChannel {
        inbox: VecDeque::from([ChannelMessage {
            text: "please ask me".to_owned(),
            attachments: vec![],
            is_guest_context: false,
            is_from_bot: setup.from_bot,
            owner_key: None,
        }]),
        support: setup.support,
        asked: Arc::clone(&asked),
    };
    let mut agent = Agent::new(
        AnyProvider::Mock(mock),
        channel,
        registry,
        None,
        5,
        NoopExecutor,
    )
    .with_interaction_config(InteractionConfig::from_config(&cfg))
    .apply_session_config(AgentSessionConfig::from_config(&cfg, 100_000));
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

fn done() -> Vec<ChatResponse> {
    vec![ChatResponse::text("done")]
}

// ---- #7122 -------------------------------------------------------------------------------

#[tokio::test]
async fn ask_user_listed_and_reachable_for_user_turn_on_interactive_channel() {
    let r = run(
        Setup::default(),
        vec![ask("call1", "Which target?"), ChatResponse::text("ok")],
    )
    .await;
    assert!(r.listed("ask_user"));
    assert_eq!(r.asked.len(), 1);
}

#[tokio::test]
async fn ask_user_not_listed_on_channel_without_interactive_support() {
    let r = run(
        Setup {
            support: AskUserSupport::Unavailable,
            ..Setup::default()
        },
        done(),
    )
    .await;
    assert!(r.listed("compress_context"), "provider saw a tool list");
    assert!(
        !r.listed("ask_user"),
        "tool offered on a non-answering channel"
    );
}

#[tokio::test]
async fn ask_user_call_on_unavailable_channel_is_declined_without_channel_io() {
    let r = run(
        Setup {
            support: AskUserSupport::Unavailable,
            ..Setup::default()
        },
        vec![ask("call9", "Which target?"), ChatResponse::text("ok")],
    )
    .await;
    assert!(
        r.asked.is_empty(),
        "channel must not be asked: {:?}",
        r.asked
    );
    assert!(r.last_text().contains("call9"), "refusal must stay paired");
}

#[tokio::test]
async fn ask_user_not_listed_for_injected_origin_turn() {
    let r = run(
        Setup {
            from_bot: true,
            ..Setup::default()
        },
        done(),
    )
    .await;
    assert!(r.listed("compress_context"), "provider saw a tool list");
    assert!(!r.listed("ask_user"), "tool offered to an injected turn");
}

#[tokio::test]
async fn ask_user_call_in_injected_origin_turn_is_declined_without_channel_io() {
    let r = run(
        Setup {
            from_bot: true,
            ..Setup::default()
        },
        vec![ask("call5", "Which target?"), ChatResponse::text("ok")],
    )
    .await;
    assert!(
        r.asked.is_empty(),
        "channel must not be asked: {:?}",
        r.asked
    );
    assert!(r.last_text().contains("call5"));
}

// ---- #7123 -------------------------------------------------------------------------------

#[tokio::test]
async fn builtin_fully_denied_ask_user_is_absent_from_listing() {
    let r = run(
        Setup {
            rules: vec![("ask_user", vec![("*", PermissionAction::Deny)])],
            ..Setup::default()
        },
        done(),
    )
    .await;
    assert!(!r.listed("ask_user"));
}

#[tokio::test]
async fn builtin_fully_denied_compress_context_is_absent_from_listing() {
    let r = run(
        Setup {
            rules: vec![("compress_context", vec![("*", PermissionAction::Deny)])],
            ..Setup::default()
        },
        done(),
    )
    .await;
    assert!(r.listed("ask_user"), "unrelated built-ins stay listed");
    assert!(!r.listed("compress_context"));
}

#[tokio::test]
async fn builtin_without_rules_stays_listed() {
    let r = run(Setup::default(), done()).await;
    assert!(r.listed("compress_context"));
    assert!(r.listed("ask_user"));
}

#[tokio::test]
async fn builtin_ask_user_pattern_deny_blocks_before_channel_and_uses_no_slot() {
    let r = run(
        Setup {
            max_questions: 1,
            rules: vec![(
                "ask_user",
                vec![
                    ("*production*", PermissionAction::Deny),
                    ("*", PermissionAction::Allow),
                ],
            )],
            ..Setup::default()
        },
        vec![
            ask("call1", "Deploy to production?"),
            ask("call2", "Which staging region?"),
            ChatResponse::text("ok"),
        ],
    )
    .await;
    assert_eq!(
        r.asked.len(),
        1,
        "denied call must not reach the channel nor consume the single question slot"
    );
    assert!(r.asked[0].contains("staging"));
    assert!(r.last_text().contains("denied by [tools.permissions]"));
}

#[tokio::test]
async fn builtin_compress_context_deny_call_is_refused_with_denied_reply() {
    let r = run(
        Setup {
            rules: vec![("compress_context", vec![("*", PermissionAction::Deny)])],
            ..Setup::default()
        },
        vec![
            tool_call("call3", "compress_context", serde_json::json!({})),
            ChatResponse::text("ok"),
        ],
    )
    .await;
    let text = r.last_text();
    assert!(text.contains("call3"));
    assert!(
        text.contains("compress_context is denied by [tools.permissions]"),
        "{text}"
    );
}

#[tokio::test]
async fn builtin_full_autonomy_ignores_deny_rules() {
    let r = run(
        Setup {
            autonomy: AutonomyLevel::Full,
            rules: vec![("ask_user", vec![("*", PermissionAction::Deny)])],
            ..Setup::default()
        },
        vec![ask("call1", "Which target?"), ChatResponse::text("ok")],
    )
    .await;
    assert_eq!(
        r.asked.len(),
        1,
        "Full autonomy mirrors TrustGate: rules ignored"
    );
}
