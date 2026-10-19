// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Usage recording for streamed Claude responses (text and tool streams).

use tokio_stream::StreamExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::provider::AuxiliaryUsage;

const MESSAGE_START: &str = r#"{"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"model":"claude-opus-5-5","usage":{"input_tokens":100,"cache_creation_input_tokens":10,"cache_read_input_tokens":50,"output_tokens":1}}}"#;

/// Final counters with one compaction iteration billed on top of the final message iteration.
const MESSAGE_DELTA: &str = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":30,"iterations":[{"type":"compaction","input_tokens":7000,"output_tokens":400,"cache_creation_input_tokens":3,"cache_read_input_tokens":4},{"type":"message","input_tokens":100,"output_tokens":30}]}}"#;

fn sse(events: &[(&str, &str)]) -> ResponseTemplate {
    let body = events.iter().fold(String::new(), |mut body, (name, data)| {
        use std::fmt::Write as _;
        let _ = write!(body, "event: {name}\ndata: {data}\n\n");
        body
    });
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

fn text_stream(start: &'static str, delta: &'static str) -> ResponseTemplate {
    sse(&[
        ("message_start", start),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        ("message_delta", delta),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ])
}

async fn provider_with(server: &MockServer, template: ResponseTemplate) -> ClaudeProvider {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(template)
        .mount(server)
        .await;
    ClaudeProvider::new("key".into(), "claude-opus-5-5".into(), 1024)
        .with_api_url(format!("{}/v1/messages", server.uri()))
}

fn user_turn() -> Vec<Message> {
    vec![Message::from_legacy(Role::User, "hello")]
}

fn assert_final_usage(provider: &ClaudeProvider) {
    assert_eq!(provider.last_usage(), Some((100, 30)));
    assert_eq!(provider.last_cache_usage(), Some((10, 50)));
    assert_eq!(
        provider.last_auxiliary_usage(),
        Some(AuxiliaryUsage {
            input: 7000,
            output: 400,
            cache_creation: 3,
            cache_read: 4,
        })
    );
}

#[tokio::test]
async fn text_stream_records_merged_usage_and_compaction_iterations() {
    let server = MockServer::start().await;
    let provider = provider_with(&server, text_stream(MESSAGE_START, MESSAGE_DELTA)).await;
    let mut stream = provider.chat_stream(&user_turn()).await.unwrap();
    while let Some(chunk) = stream.next().await {
        chunk.unwrap();
    }
    assert_final_usage(&provider);
}

#[tokio::test]
async fn tool_stream_records_merged_usage_and_compaction_iterations() {
    let server = MockServer::start().await;
    let provider = provider_with(&server, text_stream(MESSAGE_START, MESSAGE_DELTA)).await;
    let tools = [crate::provider::ToolDefinition {
        name: "lookup".into(),
        description: "d".into(),
        parameters: serde_json::json!({"type": "object"}),
        output_schema: None,
    }];
    let mut stream = provider
        .chat_with_tools_stream(&user_turn(), &tools)
        .await
        .unwrap();
    while stream.next().await.is_some() {}
    assert_final_usage(&provider);
}

#[tokio::test]
async fn usage_is_recorded_after_message_start_before_the_delta_arrives() {
    let server = MockServer::start().await;
    let provider = provider_with(&server, sse(&[("message_start", MESSAGE_START)])).await;
    let mut stream = provider.chat_stream(&user_turn()).await.unwrap();
    while stream.next().await.is_some() {}
    assert_eq!(provider.last_usage(), Some((100, 1)));
    assert_eq!(provider.last_cache_usage(), Some((10, 50)));
    assert_eq!(provider.last_auxiliary_usage(), None);
}

#[tokio::test]
async fn delta_without_iterations_leaves_no_auxiliary_usage() {
    let server = MockServer::start().await;
    let provider = provider_with(
        &server,
        text_stream(
            MESSAGE_START,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}}"#,
        ),
    )
    .await;
    let mut stream = provider.chat_stream(&user_turn()).await.unwrap();
    while stream.next().await.is_some() {}
    assert_eq!(
        provider.last_usage(),
        Some((100, 9)),
        "input from message_start"
    );
    assert_eq!(provider.last_auxiliary_usage(), None);
}

#[tokio::test]
async fn failed_stream_open_leaves_no_stale_usage_but_keeps_ttft() {
    let server = MockServer::start().await;
    let provider = provider_with(&server, text_stream(MESSAGE_START, MESSAGE_DELTA)).await;
    let mut stream = provider.chat_stream(&user_turn()).await.unwrap();
    while stream.next().await.is_some() {}
    assert_final_usage(&provider);

    server.reset().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(400).set_body_string("invalid request"))
        .mount(&server)
        .await;
    assert!(provider.chat_stream(&user_turn()).await.is_err());
    assert_eq!(provider.last_usage(), None);
    assert_eq!(provider.last_cache_usage(), None);
    assert_eq!(provider.last_auxiliary_usage(), None);
    assert!(
        provider.last_ttft_ms().is_some(),
        "ttft of the failed attempt is kept"
    );
}
