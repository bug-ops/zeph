// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Acceptance tests (tests-first) for #7139 and the Ollama part of #7131: the auxiliary Ollama
//! endpoints (`/api/show`, `/api/embed`, `/api/tags`, warmup) must map non-2xx responses to
//! typed, sanitized errors instead of `LlmError::Other(<ollama-rs text>)`, and a connect failure
//! must stay `LlmError::Http`.

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zeph_llm::ollama::OllamaProvider;
use zeph_llm::provider::{LlmProvider, Message, Role};
use zeph_llm::{ApiErrorDetail, LlmError};

const SECRET: &str = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789";
const BEARER: &str = "Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abcdefghijklmnop";

#[derive(Clone, Copy, Debug)]
enum Endpoint {
    Show,
    Embed,
    EmbedBatch,
    Tags,
    Warmup,
}

fn error_body() -> serde_json::Value {
    serde_json::json!({
        "error": format!("bad key {SECRET} and {BEARER} \u{1b}[31m\u{7}ctl {}", "x".repeat(5000))
    })
}

async fn mock_error(endpoint: Endpoint, status: u16, body: serde_json::Value) -> MockServer {
    let server = MockServer::start().await;
    let (verb, route) = match endpoint {
        Endpoint::Show => ("POST", "/api/show"),
        Endpoint::Embed | Endpoint::EmbedBatch => ("POST", "/api/embed"),
        Endpoint::Tags => ("GET", "/api/tags"),
        Endpoint::Warmup => ("POST", "/api/chat"),
    };
    Mock::given(method(verb))
        .and(path(route))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(&server)
        .await;
    server
}

async fn call(endpoint: Endpoint, base_url: &str) -> LlmError {
    let provider = OllamaProvider::new(base_url, "test-model".into(), "embed-model".into());
    match endpoint {
        Endpoint::Show => provider.fetch_model_info().await.map(|_| ()),
        Endpoint::Embed => provider.embed("hello").await.map(|_| ()),
        Endpoint::EmbedBatch => provider.embed_batch(&["a", "b"]).await.map(|_| ()),
        Endpoint::Tags => provider.list_models_remote().await.map(|_| ()),
        Endpoint::Warmup => provider.warmup().await,
    }
    .expect_err("non-2xx must be an error")
}

fn assert_sanitized_api_error(endpoint: Endpoint, err: &LlmError) {
    let LlmError::ApiError { status, detail, .. } = err else {
        panic!("{endpoint:?}: expected typed ApiError, got {err:?}");
    };
    assert_eq!(*status, 500, "{endpoint:?}: status preserved");
    let detail = detail
        .as_ref()
        .unwrap_or_else(|| panic!("{endpoint:?}: body parsed into detail"));
    let message = detail.message();
    assert!(!message.contains(SECRET), "{endpoint:?}: api key leaked");
    assert!(
        !message.contains("eyJ"),
        "{endpoint:?}: bearer token leaked"
    );
    assert!(message.contains("[REDACTED]"), "{endpoint:?}: not redacted");
    assert!(
        !message.chars().any(char::is_control),
        "{endpoint:?}: control characters kept"
    );
    assert!(message.chars().count() <= ApiErrorDetail::MAX_MESSAGE_CHARS);
    let shown = err.to_string();
    assert!(!shown.contains(SECRET) && !shown.contains("eyJ"));
    assert!(!shown.chars().any(char::is_control));
}

#[tokio::test]
async fn fetch_model_info_non_2xx_maps_to_sanitized_api_error() {
    let server = mock_error(Endpoint::Show, 500, error_body()).await;
    assert_sanitized_api_error(Endpoint::Show, &call(Endpoint::Show, &server.uri()).await);
}

#[tokio::test]
async fn embed_non_2xx_maps_to_sanitized_api_error() {
    let server = mock_error(Endpoint::Embed, 500, error_body()).await;
    assert_sanitized_api_error(Endpoint::Embed, &call(Endpoint::Embed, &server.uri()).await);
}

#[tokio::test]
async fn embed_batch_non_2xx_maps_to_sanitized_api_error() {
    let server = mock_error(Endpoint::EmbedBatch, 500, error_body()).await;
    assert_sanitized_api_error(
        Endpoint::EmbedBatch,
        &call(Endpoint::EmbedBatch, &server.uri()).await,
    );
}

#[tokio::test]
async fn list_models_remote_non_2xx_maps_to_sanitized_api_error() {
    let server = mock_error(Endpoint::Tags, 500, error_body()).await;
    assert_sanitized_api_error(Endpoint::Tags, &call(Endpoint::Tags, &server.uri()).await);
}

#[tokio::test]
async fn warmup_non_2xx_maps_to_sanitized_api_error() {
    let server = mock_error(Endpoint::Warmup, 500, error_body()).await;
    assert_sanitized_api_error(
        Endpoint::Warmup,
        &call(Endpoint::Warmup, &server.uri()).await,
    );
}

#[tokio::test]
async fn embed_context_length_400_maps_to_context_length_exceeded() {
    let body =
        serde_json::json!({ "error": "the input length exceeds the context length exceeded" });
    let server = mock_error(Endpoint::Embed, 400, body).await;
    let err = call(Endpoint::Embed, &server.uri()).await;
    assert!(
        matches!(err, LlmError::ContextLengthExceeded),
        "expected ContextLengthExceeded, got {err:?}"
    );
}

#[tokio::test]
async fn embed_batch_context_length_400_maps_to_context_length_exceeded() {
    let body = serde_json::json!({ "error": "input too long for this model" });
    let server = mock_error(Endpoint::EmbedBatch, 400, body).await;
    let err = call(Endpoint::EmbedBatch, &server.uri()).await;
    assert!(
        matches!(err, LlmError::ContextLengthExceeded),
        "expected ContextLengthExceeded, got {err:?}"
    );
}

#[tokio::test]
async fn chat_connect_failure_stays_http_error_with_is_connect() {
    let provider = OllamaProvider::new("http://127.0.0.1:1", "m".into(), "e".into());
    let msgs = vec![Message::from_legacy(Role::User, "hello")];
    let err = provider.chat(&msgs).await.unwrap_err();
    assert!(
        matches!(&err, LlmError::Http(e) if e.is_connect()),
        "expected LlmError::Http with is_connect(), got {err:?}"
    );
}

#[tokio::test]
async fn embed_connect_failure_stays_http_error_with_is_connect() {
    let provider = OllamaProvider::new("http://127.0.0.1:1", "m".into(), "e".into());
    let err = provider.embed("hello").await.unwrap_err();
    assert!(
        matches!(&err, LlmError::Http(e) if e.is_connect()),
        "expected LlmError::Http with is_connect(), got {err:?}"
    );
}
