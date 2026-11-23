// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![expect(
    clippy::unwrap_used,
    reason = "Fixture construction failures should fail the test immediately."
)]

//! Acceptance tests for #7171: assistant `ToolUse` parts reach Ollama as structured
//! `tool_calls`, and each `ToolResult` becomes its own `tool` message.

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zeph_common::ToolName;
use zeph_llm::ollama::OllamaProvider;
use zeph_llm::provider::{LlmProvider, Message, MessagePart, Role, ToolDefinition};

fn tools() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: ToolName::new("bash"),
        description: "run".into(),
        parameters: json!({"type": "object"}),
        output_schema: None,
    }]
}

fn use_part(id: &str, name: &str, input: Value) -> MessagePart {
    MessagePart::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
    }
}

fn result_part(id: &str, content: &str) -> MessagePart {
    MessagePart::ToolResult {
        tool_use_id: id.into(),
        content: content.into(),
        is_error: false,
    }
}

async fn sent_messages(history: Vec<Message>) -> Vec<Value> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "m",
            "created_at": "2024-01-01T00:00:00Z",
            "message": {"role": "assistant", "content": "done"},
            "done": true,
            "total_duration": 1, "load_duration": 1,
            "prompt_eval_count": 3, "prompt_eval_duration": 1,
            "eval_count": 2, "eval_duration": 1
        })))
        .mount(&server)
        .await;
    let provider = OllamaProvider::new(&server.uri(), "m".into(), "embed".into());
    provider.chat_with_tools(&history, &tools()).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    body["messages"].as_array().unwrap().clone()
}

#[tokio::test]
async fn test_chat_with_tools_assistant_tool_use_parts_become_one_message_with_tool_calls() {
    let history = vec![
        Message::from_legacy(Role::User, "list and read"),
        Message::from_parts(
            Role::Assistant,
            vec![
                MessagePart::Text {
                    text: "working".into(),
                },
                use_part("a", "bash", json!({"command": "ls"})),
                use_part("b", "bash", json!({"command": "pwd"})),
            ],
        ),
        Message::from_parts(
            Role::User,
            vec![result_part("a", "file.txt"), result_part("b", "/work")],
        ),
    ];
    let msgs = sent_messages(history).await;
    let assistant: Vec<&Value> = msgs.iter().filter(|m| m["role"] == "assistant").collect();
    assert_eq!(assistant.len(), 1, "{msgs:?}");
    let calls = assistant[0]["tool_calls"].as_array().expect("tool_calls");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["function"]["name"], "bash");
    assert_eq!(calls[0]["function"]["arguments"], json!({"command": "ls"}));
    assert_eq!(calls[1]["function"]["arguments"], json!({"command": "pwd"}));
    let content = assistant[0]["content"].as_str().unwrap_or_default();
    assert_eq!(content, "working");
    assert!(!content.contains("[tool_use:"), "{content}");
}

#[tokio::test]
async fn test_chat_with_tools_two_tool_results_become_two_tool_messages_in_order() {
    let history = vec![
        Message::from_legacy(Role::User, "go"),
        Message::from_parts(
            Role::Assistant,
            vec![
                use_part("a", "bash", json!({})),
                use_part("b", "bash", json!({})),
            ],
        ),
        Message::from_parts(
            Role::User,
            vec![result_part("a", "FIRST"), result_part("b", "SECOND")],
        ),
    ];
    let msgs = sent_messages(history).await;
    let tool: Vec<&str> = msgs
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(tool, ["FIRST", "SECOND"]);
}

#[tokio::test]
async fn test_chat_with_tools_thinking_text_and_tool_use_keep_thinking_field() {
    let history = vec![
        Message::from_legacy(Role::User, "go"),
        Message::from_parts(
            Role::Assistant,
            vec![
                MessagePart::ThinkingBlock {
                    thinking: "pondering".into(),
                    signature: String::new(),
                },
                MessagePart::Text {
                    text: "answer".into(),
                },
                use_part("a", "bash", json!({})),
                use_part("b", "bash", json!({})),
            ],
        ),
    ];
    let msgs = sent_messages(history).await;
    let assistant: Vec<&Value> = msgs.iter().filter(|m| m["role"] == "assistant").collect();
    assert_eq!(assistant.len(), 1, "{msgs:?}");
    assert_eq!(assistant[0]["thinking"], "pondering");
    assert_eq!(assistant[0]["content"], "answer");
    assert_eq!(assistant[0]["tool_calls"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn test_chat_with_tools_plain_text_messages_are_unchanged() {
    let history = vec![
        Message::from_legacy(Role::System, "sys"),
        Message::from_legacy(Role::User, "hi"),
        Message::from_legacy(Role::Assistant, "hello"),
        Message::from_legacy(Role::User, "again"),
    ];
    let msgs = sent_messages(history).await;
    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["system", "user", "assistant", "user"]);
    assert!(
        msgs[2]
            .get("tool_calls")
            .is_none_or(|t| t.as_array().is_some_and(Vec::is_empty) || t.is_null())
    );
}
