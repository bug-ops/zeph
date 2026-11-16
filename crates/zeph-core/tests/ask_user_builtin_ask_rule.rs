// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::large_futures, clippy::unwrap_used, clippy::expect_used)]
#![recursion_limit = "256"]

//! Acceptance tests for #7123 deny-only semantics: a built-in with an `Ask` rule, or with no
//! rule, is still listed and still executes (it is neither hidden nor blocked).

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

#[tokio::test]
async fn builtin_ask_user_with_ask_rule_is_listed_and_still_executes() {
    let r = run(
        Setup {
            rules: vec![("ask_user", vec![("*", PermissionAction::Ask)])],
            ..Setup::default()
        },
        vec![ask("call1", "Which target?"), ChatResponse::text("ok")],
    )
    .await;
    assert!(r.listed("ask_user"));
    assert_eq!(r.asked.len(), 1, "an Ask rule must not block the built-in");
    assert!(!r.last_text().contains("denied by [tools.permissions]"));
}

#[tokio::test]
async fn builtin_ask_user_with_unmatched_deny_pattern_and_ask_fallback_executes() {
    let r = run(
        Setup {
            rules: vec![(
                "ask_user",
                vec![
                    ("*production*", PermissionAction::Deny),
                    ("*", PermissionAction::Ask),
                ],
            )],
            ..Setup::default()
        },
        vec![
            ask("call1", "Which staging region?"),
            ChatResponse::text("ok"),
        ],
    )
    .await;
    assert_eq!(r.asked.len(), 1);
}

#[tokio::test]
async fn builtin_compress_context_with_ask_rule_is_listed_and_not_denied() {
    let r = run(
        Setup {
            rules: vec![("compress_context", vec![("*", PermissionAction::Ask)])],
            ..Setup::default()
        },
        vec![
            tool_call("call3", "compress_context", serde_json::json!({})),
            ChatResponse::text("ok"),
        ],
    )
    .await;
    assert!(r.listed("compress_context"));
    let text = r.last_text();
    assert!(text.contains("call3"));
    assert!(!text.contains("denied by [tools.permissions]"), "{text}");
}

#[tokio::test]
async fn builtin_compress_context_without_rule_executes_and_is_not_denied() {
    let r = run(
        Setup::default(),
        vec![
            tool_call("call4", "compress_context", serde_json::json!({})),
            ChatResponse::text("ok"),
        ],
    )
    .await;
    let text = r.last_text();
    assert!(text.contains("call4"));
    assert!(!text.contains("denied by [tools.permissions]"), "{text}");
}
