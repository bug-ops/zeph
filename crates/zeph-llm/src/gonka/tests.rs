// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;
use std::time::Duration;

use tokio_stream::StreamExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use schemars::JsonSchema;
use serde::Deserialize;

use crate::gonka::endpoints::{EndpointPool, GonkaEndpoint};
use crate::gonka::provider::GonkaProvider;
use crate::gonka::signer::RequestSigner;
use crate::provider::{
    ChatResponse, LlmProvider, Message, MessageMetadata, Role, StreamChunk, ToolDefinition,
};

const PRIV_KEY: &str = "0000000000000000000000000000000000000000000000000000000000000001";

fn make_signer() -> Arc<RequestSigner> {
    Arc::new(RequestSigner::from_hex(PRIV_KEY, "gonka").unwrap())
}

fn make_provider(base_url: &str) -> GonkaProvider {
    let signer = make_signer();
    let pool = Arc::new(
        EndpointPool::new(vec![GonkaEndpoint {
            base_url: base_url.to_owned(),
            address: "gonka1w508d6qejxtdg4y5r3zarvary0c5xw7k2gsyg6".to_owned(),
        }])
        .unwrap(),
    );
    GonkaProvider::new(
        signer,
        pool,
        "gpt-4o",
        1024,
        Some("text-embedding-3-small".to_owned()),
        Duration::from_secs(10),
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
    r#"{"data": [{"index": 0, "embedding": [0.1, 0.2, 0.3]}], "model": "text-embedding-3-small"}"#;

/// Test 1: Happy path chat — verifies signing headers are present and response is extracted.
#[tokio::test]
async fn gonka_chat_signed_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(CHAT_RESPONSE),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri());
    let messages = user_message("hi");
    let result = provider.chat(&messages).await.unwrap();
    assert_eq!(result, "hello");

    // Verify signature headers were sent.
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let req = &reqs[0];
    assert!(
        req.headers.get("x-gonka-signature").is_some(),
        "X-Gonka-Signature header missing"
    );
    assert!(
        req.headers.get("x-gonka-timestamp").is_some(),
        "X-Gonka-Timestamp header missing"
    );
    assert!(
        req.headers.get("x-gonka-sender").is_some(),
        "X-Gonka-Sender header missing"
    );
    // Verify the sender address matches the known address for PRIV_KEY_1.
    let sender = req.headers.get("x-gonka-sender").unwrap().to_str().unwrap();
    assert_eq!(sender, "gonka1w508d6qejxtdg4y5r3zarvary0c5xw7k2gsyg6");

    // Verify signature is 88-char base64 (ECDSA 64 bytes STANDARD encoded).
    let sig = req
        .headers
        .get("x-gonka-signature")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(sig.len(), 88, "signature must be 88-char base64");
}

/// Test 2: Happy path streaming — SSE response is consumed and text assembled correctly.
#[tokio::test]
async fn gonka_chat_stream() {
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n\n",
        "data: [DONE]\n\n",
    );
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri());
    let messages = user_message("stream test");
    let stream = provider.chat_stream(&messages).await.unwrap();

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

/// Test 3: Happy path embed — single embedding vector is returned.
#[tokio::test]
async fn gonka_embed_happy_path() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(EMBED_RESPONSE),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri());
    let embedding = provider.embed("hello world").await.unwrap();
    assert_eq!(embedding, vec![0.1f32, 0.2f32, 0.3f32]);
}

/// Test 4: `embed_batch` — multiple embeddings returned in index order.
#[tokio::test]
async fn gonka_embed_batch_happy_path() {
    // Return embeddings in reverse order to test sort-by-index.
    let batch_response = r#"{
        "data": [
            {"index": 1, "embedding": [0.4, 0.5]},
            {"index": 0, "embedding": [0.1, 0.2]}
        ]
    }"#;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
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

/// Test 5: retry on 503 — first endpoint fails, second succeeds.
#[tokio::test]
async fn gonka_retry_on_endpoint_failure() {
    let server1 = MockServer::start().await;
    let server2 = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&server1)
        .await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(CHAT_RESPONSE),
        )
        .expect(1)
        .mount(&server2)
        .await;

    let signer = make_signer();
    let pool = Arc::new(
        EndpointPool::new(vec![
            GonkaEndpoint {
                base_url: server1.uri(),
                address: "gonka1w508d6qejxtdg4y5r3zarvary0c5xw7k2gsyg6".to_owned(),
            },
            GonkaEndpoint {
                base_url: server2.uri(),
                address: "gonka1w508d6qejxtdg4y5r3zarvary0c5xw7k2gsyg6".to_owned(),
            },
        ])
        .unwrap(),
    );
    let provider = GonkaProvider::new(signer, pool, "gpt-4o", 1024, None, Duration::from_secs(10));
    let messages = user_message("retry test");
    let result = provider.chat(&messages).await.unwrap();
    assert_eq!(result, "hello");
}

/// Test 6: all endpoints fail — [`LlmError`] returned after exhausting retries.
#[tokio::test]
async fn gonka_all_endpoints_fail() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri());
    let messages = user_message("fail test");
    let result = provider.chat(&messages).await;
    assert!(result.is_err(), "expected error when all endpoints fail");
}

/// Test 7: context length error via 400 response.
#[tokio::test]
async fn gonka_context_length_error_on_400() {
    let body_400 = r#"{"error": {"message": "context length exceeded"}}"#;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(400)
                .insert_header("content-type", "application/json")
                .set_body_string(body_400),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri());
    let messages = user_message("very long text");
    let result = provider.chat(&messages).await;
    assert!(
        matches!(result, Err(crate::error::LlmError::ContextLengthExceeded)),
        "expected ContextLengthExceeded, got: {result:?}"
    );
}

/// Test 8: fresh timestamp on each retry attempt (non-replayable signatures).
#[tokio::test]
async fn gonka_fresh_timestamp_on_retry() {
    let server1 = MockServer::start().await;
    let server2 = MockServer::start().await;

    // First endpoint always returns 503.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server1)
        .await;

    // Second endpoint returns success.
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(CHAT_RESPONSE),
        )
        .mount(&server2)
        .await;

    let signer = make_signer();
    let pool = Arc::new(
        EndpointPool::new(vec![
            GonkaEndpoint {
                base_url: server1.uri(),
                address: "gonka1w508d6qejxtdg4y5r3zarvary0c5xw7k2gsyg6".to_owned(),
            },
            GonkaEndpoint {
                base_url: server2.uri(),
                address: "gonka1w508d6qejxtdg4y5r3zarvary0c5xw7k2gsyg6".to_owned(),
            },
        ])
        .unwrap(),
    );
    let provider = GonkaProvider::new(signer, pool, "gpt-4o", 1024, None, Duration::from_secs(10));
    let messages = user_message("timestamp test");
    let result = provider.chat(&messages).await.unwrap();
    assert_eq!(result, "hello");

    // Collect timestamps from both servers.
    let ts1_reqs = server1.received_requests().await.unwrap();
    let ts2_reqs = server2.received_requests().await.unwrap();

    let ts1 = ts1_reqs[0]
        .headers
        .get("x-gonka-timestamp")
        .unwrap()
        .to_str()
        .unwrap()
        .parse::<u128>()
        .unwrap();
    let ts2 = ts2_reqs[0]
        .headers
        .get("x-gonka-timestamp")
        .unwrap()
        .to_str()
        .unwrap()
        .parse::<u128>()
        .unwrap();

    // Timestamps must be valid nanosecond values (non-zero).
    assert!(ts1 > 0, "timestamp on first attempt must be non-zero");
    assert!(ts2 > 0, "timestamp on second attempt must be non-zero");
    // Both are fresh timestamps; they may or may not differ depending on clock resolution,
    // but both must be recent (within 10 seconds of each other).
    let diff = ts2.abs_diff(ts1);
    let ten_sec_ns = 10_u128 * 1_000_000_000;
    assert!(
        diff < ten_sec_ns,
        "timestamps should be within 10s of each other"
    );
}

/// Test 9: `embed_batch` returns error when count mismatches.
#[tokio::test]
async fn gonka_embed_batch_count_mismatch_returns_error() {
    // Return 1 embedding for 2 inputs.
    let mismatch_response = r#"{"data": [{"index": 0, "embedding": [0.1]}]}"#;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(mismatch_response),
        )
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri());
    let texts = ["first", "second"];
    let result = provider.embed_batch(&texts).await;
    assert!(result.is_err(), "expected error for count mismatch");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains('2') && (msg.contains('1') || msg.contains("gonka")),
        "unexpected error message: {msg}"
    );
}

/// Test 10: `embed` returns `EmbedUnsupported` when no embedding model is configured.
#[tokio::test]
async fn gonka_embed_unsupported_without_model() {
    let signer = make_signer();
    let pool = Arc::new(
        EndpointPool::new(vec![GonkaEndpoint {
            base_url: "https://dummy.example".to_owned(),
            address: "gonka1w508d6qejxtdg4y5r3zarvary0c5xw7k2gsyg6".to_owned(),
        }])
        .unwrap(),
    );
    // No embedding model configured.
    let provider = GonkaProvider::new(signer, pool, "gpt-4o", 1024, None, Duration::from_secs(5));
    assert!(!provider.supports_embeddings());

    let result = provider.embed("test").await;
    assert!(
        matches!(result, Err(crate::error::LlmError::EmbedUnsupported { .. })),
        "expected EmbedUnsupported, got: {result:?}"
    );
}

/// Test 11: `chat_with_tools` returns `ChatResponse::ToolUse` with correct call ID and arguments.
#[tokio::test]
async fn gonka_tools_chat_with_tools_returns_tool_use() {
    let tool_response = r#"{
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_abc123",
                    "type": "function",
                    "function": {
                        "name": "get_weather",
                        "arguments": "{\"location\":\"London\"}"
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 20, "completion_tokens": 10, "total_tokens": 30}
    }"#;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(tool_response),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri());
    let messages = user_message("What is the weather in London?");

    let tool = ToolDefinition {
        name: "get_weather".into(),
        description: "Get weather for a location".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "location": {"type": "string"}
            },
            "required": ["location"]
        }),
        output_schema: None,
    };

    let result = provider.chat_with_tools(&messages, &[tool]).await.unwrap();

    match result {
        ChatResponse::ToolUse { tool_calls, .. } => {
            assert_eq!(tool_calls.len(), 1);
            assert_eq!(tool_calls[0].id, "call_abc123");
            assert_eq!(tool_calls[0].name.as_ref(), "get_weather");
            assert_eq!(tool_calls[0].input["location"], "London");
        }
        other @ ChatResponse::Text(_) => panic!("expected ToolUse, got: {other:?}"),
    }
}

/// Test 12: `chat_typed` returns a deserialized struct from JSON content.
#[tokio::test]
async fn gonka_tools_chat_typed_returns_struct() {
    #[derive(Debug, Deserialize, JsonSchema)]
    struct CityInfo {
        name: String,
        population: u64,
    }

    let typed_response = r#"{
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "{\"name\":\"London\",\"population\":9000000}"
            },
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 15, "completion_tokens": 8, "total_tokens": 23}
    }"#;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(typed_response),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri());
    let messages = user_message("Give me info about London");

    let result: CityInfo = provider.chat_typed(&messages).await.unwrap();

    assert_eq!(result.name, "London");
    assert_eq!(result.population, 9_000_000);
}
