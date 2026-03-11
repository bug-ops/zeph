// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Wiremock fixture helpers for LLM provider tests.
//!
//! Each helper returns a [`wiremock::ResponseTemplate`] that mimics a real
//! provider response.  Pair them with a [`wiremock::MockServer`] to intercept
//! HTTP calls made by [`ClaudeProvider`], [`OpenAiProvider`], or the
//! compatible-endpoint provider.

use std::fmt::Write as _;
use wiremock::ResponseTemplate;

// ---------------------------------------------------------------------------
// OpenAI-compatible response shapes
// ---------------------------------------------------------------------------

/// Non-streaming `OpenAI` chat completion response.
///
/// Compatible with `OpenAiProvider` and `CompatibleProvider`.
#[must_use]
pub fn openai_chat_response(content: &str) -> ResponseTemplate {
    let body = serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": content
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "total_tokens": 15
        }
    });
    ResponseTemplate::new(200).set_body_json(body)
}

/// `OpenAI` 429 rate-limit response.
#[must_use]
pub fn openai_rate_limit_response() -> ResponseTemplate {
    let body = serde_json::json!({
        "error": {
            "message": "Rate limit exceeded",
            "type": "requests",
            "code": "rate_limit_exceeded"
        }
    });
    ResponseTemplate::new(429).set_body_json(body)
}

/// `OpenAI` 401 auth-error response.
#[must_use]
pub fn openai_auth_error_response() -> ResponseTemplate {
    let body = serde_json::json!({
        "error": {
            "message": "Incorrect API key",
            "type": "invalid_request_error",
            "code": "invalid_api_key"
        }
    });
    ResponseTemplate::new(401).set_body_json(body)
}

/// `OpenAI` 500 server-error response.
#[must_use]
pub fn openai_server_error_response() -> ResponseTemplate {
    ResponseTemplate::new(500).set_body_string("Internal Server Error")
}

/// SSE streaming response for OpenAI-compatible endpoints.
///
/// Encodes `chunks` as `data: {...}\n\n` events followed by `data: [DONE]\n\n`.
#[must_use]
pub fn openai_sse_stream_response(chunks: &[&str]) -> ResponseTemplate {
    let mut body = String::new();
    for chunk in chunks {
        let event = serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion.chunk",
            "choices": [{
                "index": 0,
                "delta": { "content": chunk },
                "finish_reason": null
            }]
        });
        let _ = write!(body, "data: {event}\n\n");
    }
    let stop_event = serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": "stop"
        }]
    });
    let _ = write!(body, "data: {stop_event}\n\n");
    body.push_str("data: [DONE]\n\n");
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

// ---------------------------------------------------------------------------
// Claude (Anthropic) response shapes
// ---------------------------------------------------------------------------

/// Non-streaming Anthropic Messages API response.
#[must_use]
pub fn claude_messages_response(content: &str) -> ResponseTemplate {
    let body = serde_json::json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6",
        "content": [{
            "type": "text",
            "text": content
        }],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 10,
            "output_tokens": 5,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0
        }
    });
    ResponseTemplate::new(200).set_body_json(body)
}

/// Claude 429 rate-limit / 529 overload response.
#[must_use]
pub fn claude_overload_response(status: u16) -> ResponseTemplate {
    let body = serde_json::json!({
        "type": "error",
        "error": {
            "type": "overloaded_error",
            "message": "Overloaded"
        }
    });
    ResponseTemplate::new(status).set_body_json(body)
}

/// Claude streaming SSE response.
///
/// Encodes `chunks` as Anthropic `content_block_delta` events.
#[must_use]
pub fn claude_sse_stream_response(chunks: &[&str]) -> ResponseTemplate {
    let mut body = String::new();

    body.push_str(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_test\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-6\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
    );
    body.push_str(
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    );

    for chunk in chunks {
        let escaped = chunk.replace('\\', "\\\\").replace('"', "\\\"");
        let _ = write!(
            body,
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{escaped}\"}}}}\n\n"
        );
    }

    body.push_str(
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    );
    body.push_str(
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":5}}\n\n",
    );
    body.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");

    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

// ---------------------------------------------------------------------------
// Ollama-compatible response shapes (HTTP API, not ollama-rs client)
// ---------------------------------------------------------------------------

/// Ollama `/api/chat` non-streaming response.
///
/// Note: `OllamaProvider` uses the `ollama-rs` crate which speaks the same
/// JSON shape but communicates over its own HTTP client, not reqwest directly.
/// These fixtures are intended for the compatible-endpoint path or manual HTTP tests.
#[must_use]
pub fn ollama_chat_response(content: &str) -> ResponseTemplate {
    let body = serde_json::json!({
        "model": "llama3",
        "created_at": "2024-01-01T00:00:00Z",
        "message": {
            "role": "assistant",
            "content": content
        },
        "done": true,
        "total_duration": 1_000_000,
        "load_duration": 100_000,
        "prompt_eval_count": 10,
        "eval_count": 5
    });
    ResponseTemplate::new(200).set_body_json(body)
}

/// Ollama 500 error response.
#[must_use]
pub fn ollama_server_error_response() -> ResponseTemplate {
    ResponseTemplate::new(500).set_body_string("model not found")
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    // ResponseTemplate does not expose its body, so we verify fixtures via a
    // real MockServer round-trip using reqwest.

    #[tokio::test]
    async fn openai_chat_response_is_200() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(openai_chat_response("hello"))
            .mount(&server)
            .await;
        let resp = reqwest::Client::new()
            .post(format!("{}/v1/chat/completions", server.uri()))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["choices"][0]["message"]["content"], "hello");
    }

    #[tokio::test]
    async fn claude_messages_response_shape() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(claude_messages_response("world"))
            .mount(&server)
            .await;
        let resp = reqwest::Client::new()
            .post(format!("{}/v1/messages", server.uri()))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["content"][0]["text"], "world");
        assert_eq!(body["role"], "assistant");
    }

    #[tokio::test]
    async fn openai_sse_contains_done_sentinel() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/stream"))
            .respond_with(openai_sse_stream_response(&["chunk1", "chunk2"]))
            .mount(&server)
            .await;
        let raw = reqwest::Client::new()
            .post(format!("{}/stream", server.uri()))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(raw.contains("chunk1"));
        assert!(raw.contains("chunk2"));
        assert!(raw.contains("[DONE]"));
    }

    #[tokio::test]
    async fn claude_sse_contains_chunks() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/stream"))
            .respond_with(claude_sse_stream_response(&["part1", "part2"]))
            .mount(&server)
            .await;
        let raw = reqwest::Client::new()
            .post(format!("{}/stream", server.uri()))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(raw.contains("part1"));
        assert!(raw.contains("part2"));
        assert!(raw.contains("message_stop"));
    }

    #[tokio::test]
    async fn ollama_response_shape() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ollama_chat_response("ok"))
            .mount(&server)
            .await;
        let resp = reqwest::Client::new()
            .post(format!("{}/api/chat", server.uri()))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["message"]["content"], "ok");
        assert_eq!(body["done"], true);
    }
}
