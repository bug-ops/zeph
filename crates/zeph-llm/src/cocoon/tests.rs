// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;
use std::time::Duration;

use tokio_stream::StreamExt;
use wiremock::matchers::{header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::cocoon::client::{CocoonClient, CocoonHealth};
use crate::cocoon::provider::CocoonProvider;
use crate::provider::{ChatResponse, LlmProvider, Message, MessageMetadata, Role, StreamChunk};

fn make_client(base_url: &str, access_hash: Option<String>) -> Arc<CocoonClient> {
    Arc::new(CocoonClient::new(
        base_url,
        access_hash,
        Duration::from_secs(5),
    ))
}

fn make_provider(base_url: &str) -> CocoonProvider {
    CocoonProvider::new(
        "Qwen/Qwen3-0.6B",
        4096,
        Some("embed-model".into()),
        make_client(base_url, None),
    )
}

fn user_message(text: &str) -> Vec<Message> {
    vec![Message {
        role: Role::User,
        content: text.to_owned(),
        parts: vec![],
        metadata: MessageMetadata::default(),
    }]
}

const CHAT_RESPONSE: &str = r#"{
    "choices": [{"message": {"role": "assistant", "content": "hello"}, "finish_reason": "stop"}],
    "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}
}"#;

const EMBED_RESPONSE: &str =
    r#"{"data": [{"index": 0, "embedding": [0.1, 0.2, 0.3]}], "model": "embed-model"}"#;

/// Test 1: health check success — parses CocoonHealth from /stats JSON.
#[tokio::test]
async fn cocoon_health_check_success() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/stats"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(r#"{"proxy_connected":true,"worker_count":3}"#),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = make_client(&server.uri(), None);
    let health = client.health_check().await.unwrap();
    assert!(health.proxy_connected);
    assert_eq!(health.worker_count, 3);
}

/// Test 2: health check unavailable — returns LlmError::Unavailable on connection refused.
#[tokio::test]
async fn cocoon_health_check_unavailable() {
    let client = make_client("http://127.0.0.1:1", None);
    let result = client.health_check().await;
    assert!(result.is_err());
}

/// Test 4: list_models parses /v1/models response.
#[tokio::test]
async fn cocoon_list_models_parses_response() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(r#"{"data":[{"id":"Qwen/Qwen3-0.6B"},{"id":"Qwen/Qwen3-1.7B"}]}"#),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = make_client(&server.uri(), None);
    let models = client.list_models().await.unwrap();
    assert_eq!(models, vec!["Qwen/Qwen3-0.6B", "Qwen/Qwen3-1.7B"]);
}

/// Test 5: post with access hash attaches X-Access-Hash header.
#[tokio::test]
async fn cocoon_post_with_access_hash() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header_exists("x-access-hash"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(CHAT_RESPONSE),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = make_client(&server.uri(), Some("test-hash-value".into()));
    let result = client
        .post("/v1/chat/completions", b"{\"messages\":[]}")
        .await;
    assert!(result.is_ok());
}

/// Test 6: post without access hash omits X-Access-Hash header.
#[tokio::test]
async fn cocoon_post_without_access_hash() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(CHAT_RESPONSE),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = make_client(&server.uri(), None);
    let result = client
        .post("/v1/chat/completions", b"{\"messages\":[]}")
        .await;
    assert!(result.is_ok());
    let reqs = server.received_requests().await.unwrap();
    assert!(
        reqs[0].headers.get("x-access-hash").is_none(),
        "X-Access-Hash must not be present when access_hash is None"
    );
}

/// Test 7: chat happy path — returns extracted content from 200 response.
#[tokio::test]
async fn cocoon_chat_happy_path() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(CHAT_RESPONSE),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri());
    let result = provider.chat(&user_message("hi")).await.unwrap();
    assert_eq!(result, "hello");
}

/// Test 8: chat_stream happy path — assembles SSE chunks into full text.
#[tokio::test]
async fn cocoon_chat_stream_happy_path() {
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n\n",
        "data: [DONE]\n\n",
    );
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri());
    let stream = provider.chat_stream(&user_message("stream")).await.unwrap();
    let chunks: Vec<_> = stream.collect::<Vec<_>>().await;
    let text: String = chunks
        .into_iter()
        .filter_map(|c| match c {
            Ok(StreamChunk::Content(t)) => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(text, "hello");
}

/// Test 9: embed happy path — returns embedding vector from 200 response.
#[tokio::test]
async fn cocoon_embed_happy_path() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(EMBED_RESPONSE),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri());
    let embedding = provider.embed("hello").await.unwrap();
    assert_eq!(embedding, vec![0.1f32, 0.2f32, 0.3f32]);
}

/// Test 10: embed returns EmbedUnsupported on 404.
#[tokio::test]
async fn cocoon_embed_returns_unsupported_on_404() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri());
    let err = provider.embed("test").await.unwrap_err();
    assert!(
        matches!(err, crate::error::LlmError::EmbedUnsupported { .. }),
        "expected EmbedUnsupported, got: {err:?}"
    );
}

/// Test 11: embed returns EmbedUnsupported when no embedding model configured.
#[tokio::test]
async fn cocoon_embed_unsupported_without_model() {
    let server = MockServer::start().await;
    let provider = CocoonProvider::new(
        "Qwen/Qwen3-0.6B",
        4096,
        None, // no embedding model
        make_client(&server.uri(), None),
    );
    let err = provider.embed("test").await.unwrap_err();
    assert!(matches!(
        err,
        crate::error::LlmError::EmbedUnsupported { .. }
    ));
}

/// Test 12: embed_batch happy path — sorts by index and returns correct vectors.
#[tokio::test]
async fn cocoon_embed_batch_happy_path() {
    let batch_response = r#"{
        "data": [
            {"index": 1, "embedding": [0.4, 0.5]},
            {"index": 0, "embedding": [0.1, 0.2]}
        ]
    }"#;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(batch_response),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri());
    let texts = ["first", "second"];
    let embeddings = provider.embed_batch(&texts).await.unwrap();
    assert_eq!(embeddings.len(), 2);
    assert_eq!(embeddings[0], vec![0.1f32, 0.2f32]);
    assert_eq!(embeddings[1], vec![0.4f32, 0.5f32]);
}

/// Test 13: embed_batch count mismatch returns error.
#[tokio::test]
async fn cocoon_embed_batch_count_mismatch() {
    let mismatch_response = r#"{"data": [{"index": 0, "embedding": [0.1, 0.2]}]}"#;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(mismatch_response),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri());
    let texts = ["first", "second", "third"];
    let err = provider.embed_batch(&texts).await.unwrap_err();
    assert!(
        matches!(err, crate::error::LlmError::Other(_)),
        "expected Other error on count mismatch, got: {err:?}"
    );
}

/// Test 14: chat returns ContextLengthExceeded on 400 with context error body.
#[tokio::test]
async fn cocoon_chat_context_length_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(400)
                .insert_header("content-type", "application/json")
                .set_body_string(
                    r#"{"error":{"message":"context_length_exceeded: maximum context length"}}"#,
                ),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri());
    let err = provider
        .chat(&user_message("long input"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, crate::error::LlmError::ContextLengthExceeded),
        "expected ContextLengthExceeded, got: {err:?}"
    );
}

/// Test 17: clone independence — clone has independent UsageTracker, shares Arc<CocoonClient>.
#[test]
fn cocoon_provider_clone_independence() {
    let server_uri = "http://localhost:10000";
    let provider = make_provider(server_uri);
    let cloned = provider.clone();
    assert_eq!(provider.name(), cloned.name());
    assert_eq!(provider.model_identifier(), cloned.model_identifier());
    assert!(provider.supports_streaming());
    assert!(cloned.supports_streaming());
}

/// Test 18: with_generation_overrides preserves model identifier.
#[test]
fn cocoon_provider_with_generation_overrides() {
    let provider = make_provider("http://localhost:10000");
    let model_before = provider.model_identifier().to_owned();
    let overrides = crate::provider::GenerationOverrides {
        temperature: Some(0.5),
        top_p: None,
        top_k: None,
        frequency_penalty: None,
        presence_penalty: None,
    };
    let patched = provider.with_generation_overrides(overrides);
    assert_eq!(patched.model_identifier(), model_before);
}

/// Test 19: chat returns ApiError on 500 response.
#[tokio::test]
async fn cocoon_api_error_on_5xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
        .expect(1)
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri());
    let err = provider.chat(&user_message("test")).await.unwrap_err();
    assert!(
        matches!(err, crate::error::LlmError::ApiError { status: 500, .. }),
        "expected ApiError(500), got: {err:?}"
    );
}

/// Test 20: health check ignores unknown fields in /stats JSON.
#[tokio::test]
async fn cocoon_health_check_unknown_fields_ignored() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/stats"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(
                    r#"{"proxy_connected":false,"worker_count":0,"unknown_field":"value","version":"1.2.3"}"#,
                ),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = make_client(&server.uri(), None);
    let health = client.health_check().await.unwrap();
    assert!(!health.proxy_connected);
    assert_eq!(health.worker_count, 0);
}

/// MINOR-2: Debug output must not expose the raw access hash value.
#[test]
fn cocoon_client_debug_redacts_access_hash() {
    let secret_hash = "super-secret-access-hash-12345";
    let client = CocoonClient::new(
        "http://localhost:10000",
        Some(secret_hash.to_owned()),
        Duration::from_secs(30),
    );
    let debug_output = format!("{client:?}");
    assert!(
        !debug_output.contains(secret_hash),
        "Debug output must not contain the raw access hash; got: {debug_output}"
    );
    assert!(
        debug_output.contains("redacted"),
        "Debug output must indicate the hash is redacted; got: {debug_output}"
    );
}

/// Malformed JSON from sidecar on 200 returns a parse error.
#[tokio::test]
async fn cocoon_malformed_json_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string("not valid json {{{"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri());
    let err = provider.chat(&user_message("test")).await.unwrap_err();
    assert!(
        matches!(err, crate::error::LlmError::Json(_)),
        "expected Json parse error, got: {err:?}"
    );
}
