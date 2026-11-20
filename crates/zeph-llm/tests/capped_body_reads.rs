// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![expect(
    clippy::expect_used,
    reason = "Fixture construction failures should fail the test immediately.; Fixture construction failures should fail the test immediately."
)]

//! Acceptance tests (tests-first) for #7131: response bodies are read through the capped helpers
//! and error bodies are sanitized before they reach `LlmError`.

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zeph_llm::LlmError;
use zeph_llm::stt::SpeechToText;
use zeph_llm::whisper::WhisperProvider;

const SECRET: &str = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789";

fn whisper(server: &MockServer) -> WhisperProvider {
    WhisperProvider::new(reqwest::Client::new(), "key", server.uri(), "whisper-1")
}

async fn whisper_server(status: u16, body: Vec<u8>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(status).set_body_bytes(body))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn whisper_transcribe_oversized_success_body_is_response_too_large() {
    let server = whisper_server(200, vec![b' '; 33 * 1024 * 1024]).await;
    let err = whisper(&server)
        .transcribe(b"audio", None)
        .await
        .expect_err("oversized body must be rejected");
    assert!(
        matches!(err, LlmError::ResponseTooLarge { .. }),
        "expected ResponseTooLarge, got {err:?}"
    );
}

#[tokio::test]
async fn whisper_transcribe_oversized_error_body_is_response_too_large() {
    let server = whisper_server(500, vec![b'x'; 33 * 1024 * 1024]).await;
    let err = whisper(&server)
        .transcribe(b"audio", None)
        .await
        .expect_err("oversized body must be rejected");
    assert!(
        matches!(err, LlmError::ResponseTooLarge { .. }),
        "expected ResponseTooLarge, got {err:?}"
    );
}

#[tokio::test]
async fn whisper_transcribe_error_body_multibyte_at_cap_boundary_does_not_panic() {
    let mut body = "a".repeat(499);
    body.push_str("\u{20ac}\u{20ac}tail");
    let server = whisper_server(500, body.into_bytes()).await;
    let err = whisper(&server)
        .transcribe(b"audio", None)
        .await
        .expect_err("500 must be an error");
    assert!(
        matches!(err, LlmError::TranscriptionFailed(_)),
        "got {err:?}"
    );
}

#[tokio::test]
async fn whisper_transcribe_error_body_is_scrubbed_and_control_free() {
    let body = format!("oops {SECRET} \u{1b}[31m\u{7}end {}", "y".repeat(2000));
    let server = whisper_server(500, body.into_bytes()).await;
    let err = whisper(&server)
        .transcribe(b"audio", None)
        .await
        .expect_err("500 must be an error");
    let LlmError::TranscriptionFailed(message) = &err else {
        panic!("expected TranscriptionFailed, got {err:?}");
    };
    assert!(!message.contains(SECRET), "api key leaked: {message}");
    assert!(message.contains("[REDACTED]"), "not redacted: {message}");
    assert!(!message.chars().any(char::is_control));
    assert!(message.chars().count() <= 600, "unbounded error message");
}

#[cfg(feature = "gonka")]
mod gonka_straddle {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use zeph_llm::gonka::endpoints::{EndpointPool, GonkaEndpoint};
    use zeph_llm::gonka::{GonkaConfig, GonkaProvider, RequestSigner};
    use zeph_llm::provider::{LlmProvider, Message, Role};

    #[tokio::test]
    async fn gonka_chat_with_tools_secret_straddling_256_chars_is_redacted() {
        let signer = Arc::new(RequestSigner::from_hex(&"11".repeat(32), "gonka").unwrap());
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({
                "error": { "type": "server_error",
                           "message": format!("{} {SECRET} tail", "a".repeat(250)) }
            })))
            .mount(&server)
            .await;
        let pool = Arc::new(
            EndpointPool::new(vec![GonkaEndpoint {
                base_url: server.uri(),
                address: signer.address().to_owned(),
            }])
            .unwrap(),
        );
        let provider = GonkaProvider::new(GonkaConfig {
            signer,
            pool,
            model: "m".into(),
            max_tokens: 16,
            embedding_model: None,
            timeout: Duration::from_secs(5),
        });
        let err = provider
            .chat_with_tools(&[Message::from_legacy(Role::User, "hi")], &[])
            .await
            .expect_err("500 must be an error");
        let shown = err.to_string();
        assert!(
            !shown.contains("sk-ant-api03"),
            "secret fragment leaked: {shown}"
        );
        assert!(
            shown.contains("[REDACTED]"),
            "full body must be parsed and redacted: {shown}"
        );
    }
}
