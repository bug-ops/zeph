// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Acceptance tests for scrubbing the upstream 400 body in `OpenAI` error messages (#7085).
//!
//! Registered from `openai/mod.rs` with `#[cfg(test)] mod message_scrub_tests;`.

use super::*;
use crate::provider::{MessageMetadata, Role};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SECRET: &str = "sk-test-abcdefghijklmnop1234";
const MAX_BODY_CHARS: usize = 500;

fn hostile_tail() -> String {
    format!("key {SECRET} \x1b[31mred\x1b[0m {}", "x".repeat(5000))
}

async fn provider_returning_400(body: String, reasoning_effort: Option<String>) -> OpenAiProvider {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_string(body))
        .mount(&server)
        .await;
    let provider = OpenAiProvider::new(OpenAiConfig {
        api_key: "sk-test".into(),
        base_url: server.uri(),
        model: "gpt-5.4-mini".into(),
        max_tokens: 256,
        embedding_model: None,
        reasoning_effort,
        context_window: None,
        completion_tokens_param: None,
        vision: None,
    });
    // Keep the mock server alive for the provider's lifetime.
    std::mem::forget(server);
    provider
}

async fn chat_error(provider: &OpenAiProvider) -> LlmError {
    let messages = vec![Message {
        role: Role::User,
        content: "hi".into(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    }];
    let tools = vec![ToolDefinition {
        name: "bash".into(),
        description: "Execute a shell command".into(),
        parameters: serde_json::json!({"type": "object", "properties": {}}),
        output_schema: None,
    }];
    provider
        .chat_with_tools(&messages, &tools)
        .await
        .expect_err("400 must be an error")
}

fn assert_scrubbed(message: &str) {
    let head: String = message.chars().take(300).collect();
    assert!(!message.contains(SECRET), "secret leaked: {head}");
    assert!(
        !message.chars().any(char::is_control),
        "control char leaked: {head:?}"
    );
}

#[tokio::test]
async fn chat_with_tools_capability_mismatch_message_is_scrubbed_and_capped() {
    let body = format!(
        "Function tools with reasoning_effort are not supported for gpt-5.4-mini in \
         /v1/chat/completions. Please use /v1/responses instead. {}",
        hostile_tail()
    );
    let provider = provider_returning_400(body, Some("high".into())).await;
    match chat_error(&provider).await {
        LlmError::ModelCapabilityMismatch { message, .. } => {
            assert_scrubbed(&message);
            assert!(message.contains("reasoning_effort"));
            assert!(message.contains("Unset"));
            let embedded = message
                .split_once("Original error: ")
                .expect("original error marker")
                .1;
            assert!(embedded.chars().count() <= MAX_BODY_CHARS, "uncapped body");
        }
        other => panic!("expected ModelCapabilityMismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn chat_with_tools_invalid_input_message_is_scrubbed_and_capped() {
    let provider = provider_returning_400(format!("bad request {}", hostile_tail()), None).await;
    match chat_error(&provider).await {
        LlmError::InvalidInput { message, .. } => {
            assert_scrubbed(&message);
            assert!(message.chars().count() <= MAX_BODY_CHARS, "uncapped body");
            assert!(message.contains("bad request"));
        }
        other => panic!("expected InvalidInput, got {other:?}"),
    }
}

async fn embed_provider_returning_400(body: String) -> OpenAiProvider {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(400).set_body_string(body))
        .mount(&server)
        .await;
    let provider = OpenAiProvider::new(OpenAiConfig {
        api_key: "sk-test".into(),
        base_url: server.uri(),
        model: "gpt-5.4-mini".into(),
        max_tokens: 256,
        embedding_model: Some("text-embedding-3-small".into()),
        reasoning_effort: None,
        context_window: None,
        completion_tokens_param: None,
        vision: None,
    });
    std::mem::forget(server);
    provider
}

fn assert_invalid_input_scrubbed(err: LlmError) {
    match err {
        LlmError::InvalidInput { message, .. } => {
            assert_scrubbed(&message);
            assert!(message.chars().count() <= MAX_BODY_CHARS, "uncapped body");
            assert!(message.contains("bad request"));
        }
        other => panic!("expected InvalidInput, got {other:?}"),
    }
}

#[tokio::test]
async fn embed_invalid_input_message_is_scrubbed_and_capped() {
    let provider = embed_provider_returning_400(format!("bad request {}", hostile_tail())).await;
    let err = provider
        .embed("hi")
        .await
        .expect_err("400 must be an error");
    assert_invalid_input_scrubbed(err);
}

#[tokio::test]
async fn embed_batch_invalid_input_message_is_scrubbed_and_capped() {
    let provider = embed_provider_returning_400(format!("bad request {}", hostile_tail())).await;
    let err = provider
        .embed_batch(&["hi"])
        .await
        .expect_err("400 must be an error");
    assert_invalid_input_scrubbed(err);
}

#[tokio::test]
async fn chat_with_tools_body_over_cap_is_rejected() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![
            b'a';
            crate::http::LLM_BODY_LIMIT.get()
                + 1
        ]))
        .mount(&server)
        .await;
    let provider = OpenAiProvider::new(OpenAiConfig {
        api_key: "sk-test".into(),
        base_url: server.uri(),
        model: "gpt-5.4-mini".into(),
        max_tokens: 256,
        embedding_model: None,
        reasoning_effort: None,
        context_window: None,
        completion_tokens_param: None,
        vision: None,
    });
    assert!(matches!(
        chat_error(&provider).await,
        LlmError::ResponseTooLarge { limit } if limit == crate::http::LLM_BODY_LIMIT.get()
    ));
}
