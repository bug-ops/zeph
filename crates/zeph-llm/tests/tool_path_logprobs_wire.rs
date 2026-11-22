// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![expect(
    clippy::unwrap_used,
    reason = "Fixture construction failures should fail the test immediately."
)]

//! Acceptance tests (tests-first) for #7143 (E, N1): `LlmProvider::chat_with_tools_extras` asks
//! for logprobs on the tool request (`OpenAI`, `Ollama`), retries once without them on a logprobs
//! 400 (latching per instance only when the retry succeeds), and never retries a context-length
//! 400. Plain `chat_with_tools` never requests logprobs.

use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zeph_common::ToolName;
use zeph_llm::ollama::OllamaProvider;
use zeph_llm::openai::OpenAiProvider;
use zeph_llm::provider::{ChatResponse, LlmProvider, Message, Role, ToolDefinition};
use zeph_llm::{LlmError, OpenAiConfig};

fn messages() -> Vec<Message> {
    vec![Message::from_legacy(Role::User, "hi")]
}

fn tools() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: ToolName::new("shell"),
        description: "run".into(),
        parameters: serde_json::json!({"type": "object"}),
        output_schema: None,
    }]
}

fn has_logprobs(req: &wiremock::Request) -> bool {
    let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
    body.get("logprobs") == Some(&serde_json::Value::Bool(true))
}

async fn flags(server: &MockServer) -> Vec<bool> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(has_logprobs)
        .collect()
}

fn openai(server: &MockServer) -> OpenAiProvider {
    OpenAiProvider::new(OpenAiConfig {
        api_key: "k".into(),
        base_url: server.uri(),
        model: "gpt-4o".into(),
        max_tokens: 256,
        embedding_model: None,
        reasoning_effort: None,
        context_window: None,
        completion_tokens_param: None,
        vision: None,
    })
}

fn openai_ok(logprob: Option<f64>) -> ResponseTemplate {
    let mut choice = serde_json::json!({
        "index": 0,
        "message": {"role": "assistant", "content": "answer"},
        "finish_reason": "stop"
    });
    if let Some(lp) = logprob {
        choice["logprobs"] = serde_json::json!({"content": [{"token": "a", "logprob": lp}]});
    }
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "id": "c", "object": "chat.completion", "model": "gpt-4o",
        "choices": [choice],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    }))
}

fn openai_400(message: &str) -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_json(serde_json::json!({"error": {"message": message}}))
}

async fn mount_logprobs_400_then_ok(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(serde_json::json!({"logprobs": true})))
        .respond_with(openai_400("logprobs is not supported for this model"))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(openai_ok(None))
        .mount(server)
        .await;
}

#[tokio::test]
async fn openai_chat_with_tools_extras_requests_logprobs_and_reports_entropy() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(openai_ok(Some(-1.0)))
        .mount(&server)
        .await;

    let (resp, extras) = openai(&server)
        .chat_with_tools_extras(&messages(), &tools())
        .await
        .unwrap();

    assert!(matches!(resp, ChatResponse::Text { ref text, .. } if text == "answer"));
    assert!(extras.entropy.is_some_and(|e| (e - 1.0).abs() < 1e-9));
    assert_eq!(flags(&server).await, [true]);
}

#[tokio::test]
async fn openai_chat_with_tools_never_requests_logprobs() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(openai_ok(None))
        .mount(&server)
        .await;

    openai(&server)
        .chat_with_tools(&messages(), &tools())
        .await
        .unwrap();

    assert_eq!(flags(&server).await, [false]);
}

#[tokio::test]
async fn openai_chat_with_tools_extras_logprobs_400_retries_without_and_latches() {
    let server = MockServer::start().await;
    mount_logprobs_400_then_ok(&server).await;
    let provider = openai(&server);

    let (_, extras) = provider
        .chat_with_tools_extras(&messages(), &tools())
        .await
        .unwrap();
    assert_eq!(extras.entropy, None);
    provider
        .chat_with_tools_extras(&messages(), &tools())
        .await
        .unwrap();

    assert_eq!(flags(&server).await, [true, false, false]);
}

#[tokio::test]
async fn openai_chat_with_tools_extras_400_on_both_attempts_is_invalid_input_and_not_latched() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(openai_400("messages are malformed"))
        .mount(&server)
        .await;
    let provider = openai(&server);

    let err = provider
        .chat_with_tools_extras(&messages(), &tools())
        .await
        .unwrap_err();
    assert!(matches!(err, LlmError::InvalidInput { .. }), "{err:?}");
    let _ = provider.chat_with_tools_extras(&messages(), &tools()).await;

    assert_eq!(flags(&server).await, [true, false, true, false]);
}

#[tokio::test]
async fn openai_chat_with_tools_extras_context_length_400_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(openai_400("maximum context length is 4096 tokens"))
        .mount(&server)
        .await;

    let err = openai(&server)
        .chat_with_tools_extras(&messages(), &tools())
        .await
        .unwrap_err();

    assert!(matches!(err, LlmError::ContextLengthExceeded), "{err:?}");
    assert_eq!(flags(&server).await, [true]);
}

fn ollama_ok() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "model": "m",
        "created_at": "2024-01-01T00:00:00Z",
        "message": {"role": "assistant", "content": "answer"},
        "logprobs": [{"token": "t", "logprob": -1.0, "bytes": [116]}],
        "done": true,
        "total_duration": 1, "load_duration": 1,
        "prompt_eval_count": 3, "prompt_eval_duration": 1,
        "eval_count": 2, "eval_duration": 1
    }))
}

fn ollama(server: &MockServer) -> OllamaProvider {
    OllamaProvider::new(&server.uri(), "m".into(), "embed".into())
}

#[tokio::test]
async fn ollama_chat_with_tools_extras_requests_logprobs_and_plain_call_does_not() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ollama_ok())
        .mount(&server)
        .await;
    let provider = ollama(&server);

    let (_, extras) = provider
        .chat_with_tools_extras(&messages(), &tools())
        .await
        .unwrap();
    provider
        .chat_with_tools(&messages(), &tools())
        .await
        .unwrap();

    assert!(extras.entropy.is_some_and(|e| (e - 1.0).abs() < 1e-9));
    assert_eq!(flags(&server).await, [true, false]);
}

#[tokio::test]
async fn ollama_chat_with_tools_extras_logprobs_400_retries_without_and_latches() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .and(body_partial_json(serde_json::json!({"logprobs": true})))
        .respond_with(ResponseTemplate::new(400).set_body_string("logprobs unsupported"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ollama_ok())
        .mount(&server)
        .await;
    let provider = ollama(&server);

    provider
        .chat_with_tools_extras(&messages(), &tools())
        .await
        .unwrap();
    provider
        .chat_with_tools_extras(&messages(), &tools())
        .await
        .unwrap();

    assert_eq!(flags(&server).await, [true, false, false]);
}
