// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::sync::Mutex;

use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::{ChatResponse, ToolUseRequest};
use zeph_tools::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};
use zeph_tools::registry::{InvocationHint, ToolDef};

use crate::agent::Agent;
use crate::agent::agent_tests::{MockChannel, create_test_registry};

struct DagAwareToolExecutor {
    results: Mutex<HashMap<String, Result<Option<ToolOutput>, ToolError>>>,
}

impl DagAwareToolExecutor {
    // Each entry is consumed on first call; subsequent calls for the same tool_id return Ok(None).
    fn new(entries: Vec<(&str, Result<Option<ToolOutput>, ToolError>)>) -> Self {
        Self {
            results: Mutex::new(
                entries
                    .into_iter()
                    .map(|(k, v)| (k.to_owned(), v))
                    .collect(),
            ),
        }
    }

    fn make_output(tool_name: &str, summary: &str) -> ToolOutput {
        ToolOutput {
            tool_name: tool_name.into(),
            summary: summary.to_owned(),
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: None,
        }
    }
}

impl ToolExecutor for DagAwareToolExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        vec![
            ToolDef {
                id: "tool_a".into(),
                description: "tool a".into(),
                schema: schemars::Schema::default(),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
            },
            ToolDef {
                id: "tool_b".into(),
                description: "tool b".into(),
                schema: schemars::Schema::default(),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
            },
            ToolDef {
                id: "tool_c".into(),
                description: "tool c".into(),
                schema: schemars::Schema::default(),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
            },
        ]
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        let mut results = self.results.lock().unwrap();
        results.remove(call.tool_id.as_str()).unwrap_or(Ok(None))
    }

    async fn execute_tool_call_confirmed(
        &self,
        call: &ToolCall,
    ) -> Result<Option<ToolOutput>, ToolError> {
        Ok(Some(Self::make_output(call.tool_id.as_str(), "confirmed")))
    }
}

fn dag_tool_use_response() -> ChatResponse {
    ChatResponse::ToolUse {
        text: None,
        tool_calls: vec![
            ToolUseRequest {
                id: "tool_a_id".to_owned(),
                name: "tool_a".to_owned().into(),
                input: serde_json::json!({"arg": "value"}),
            },
            ToolUseRequest {
                id: "tool_b_id".to_owned(),
                name: "tool_b".to_owned().into(),
                input: serde_json::json!({"source": "tool_a_id"}),
            },
        ],
        thinking_blocks: vec![],
    }
}

fn independent_tool_use_response() -> ChatResponse {
    ChatResponse::ToolUse {
        text: None,
        tool_calls: vec![
            ToolUseRequest {
                id: "tool_a_id".to_owned(),
                name: "tool_a".to_owned().into(),
                input: serde_json::json!({"arg": "value"}),
            },
            ToolUseRequest {
                id: "tool_c_id".to_owned(),
                name: "tool_c".to_owned().into(),
                input: serde_json::json!({"arg": "independent"}),
            },
        ],
        thinking_blocks: vec![],
    }
}

#[tokio::test]
async fn confirmation_required_propagates_to_dependent_tier() {
    let (mock, _counter) = MockProvider::default().with_tool_use(vec![
        dag_tool_use_response(),
        ChatResponse::Text("done".into()),
    ]);
    let provider = AnyProvider::Mock(mock);
    let channel = MockChannel::new(vec!["test".to_string()]).with_confirmations(vec![false]);
    let registry = create_test_registry();
    let executor = DagAwareToolExecutor::new(vec![(
        "tool_a",
        Err(ToolError::ConfirmationRequired {
            command: "cmd".to_owned(),
        }),
    )]);

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let result = agent.run().await;
    assert!(result.is_ok());

    let has_skipped = agent.msg.messages.iter().any(|m| {
        m.content
            .contains("Skipped: a prerequisite tool failed or requires confirmation")
    });
    let sent = agent.channel.sent_messages();
    let has_skipped_in_sent = sent
        .iter()
        .any(|m| m.contains("Skipped: a prerequisite tool failed or requires confirmation"));
    assert!(
        has_skipped || has_skipped_in_sent,
        "expected synthetic skip message for tool_b; sent={sent:?}, agent_msgs={:?}",
        agent
            .msg
            .messages
            .iter()
            .map(|m| &m.content)
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn independent_tool_not_affected_by_confirmation_required() {
    let (mock, _counter) = MockProvider::default().with_tool_use(vec![
        independent_tool_use_response(),
        ChatResponse::Text("done".into()),
    ]);
    let provider = AnyProvider::Mock(mock);
    let channel = MockChannel::new(vec!["test".to_string()]).with_confirmations(vec![false]);
    let registry = create_test_registry();
    let executor = DagAwareToolExecutor::new(vec![
        (
            "tool_a",
            Err(ToolError::ConfirmationRequired {
                command: "cmd".to_owned(),
            }),
        ),
        (
            "tool_c",
            Ok(Some(DagAwareToolExecutor::make_output(
                "tool_c",
                "independent result",
            ))),
        ),
    ]);

    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let result = agent.run().await;
    assert!(result.is_ok());

    let has_independent_result = agent
        .msg
        .messages
        .iter()
        .any(|m| m.content.contains("independent result"));
    assert!(
        has_independent_result,
        "expected tool_c (independent) to execute normally; agent_msgs={:?}",
        agent
            .msg
            .messages
            .iter()
            .map(|m| &m.content)
            .collect::<Vec<_>>()
    );
}
