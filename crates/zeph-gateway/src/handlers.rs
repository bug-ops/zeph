// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;

use super::server::AppState;

#[derive(serde::Deserialize)]
pub(crate) struct WebhookPayload {
    pub channel: String,
    pub sender: String,
    pub body: String,
}

pub(crate) fn sanitize_control_chars(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_ascii_control() || *c == '\n')
        .collect()
}

impl WebhookPayload {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.sender.len() > 256 {
            return Err("sender exceeds 256 bytes");
        }
        if self.channel.len() > 256 {
            return Err("channel exceeds 256 bytes");
        }
        if self.body.len() > 65536 {
            return Err("body exceeds 65536 bytes");
        }
        Ok(())
    }
}

#[derive(serde::Serialize)]
struct WebhookResponse {
    status: &'static str,
}

#[derive(serde::Serialize)]
struct HealthResponse {
    status: &'static str,
    uptime_secs: u64,
}

pub(crate) async fn webhook_handler(
    State(state): State<AppState>,
    Json(payload): Json<WebhookPayload>,
) -> impl IntoResponse {
    if let Err(e) = payload.validate() {
        return (StatusCode::UNPROCESSABLE_ENTITY, e).into_response();
    }
    let sender = sanitize_control_chars(&payload.sender);
    let channel = sanitize_control_chars(&payload.channel);
    let msg = format!("[{}@{}] {}", sender, channel, payload.body);
    match state.webhook_tx.send(msg).await {
        Ok(()) => Json(WebhookResponse { status: "accepted" }).into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

pub(crate) async fn health_handler(State(state): State<AppState>) -> impl IntoResponse {
    Json(HealthResponse {
        status: "ok",
        uptime_secs: state.started_at.elapsed().as_secs(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_response_serializes() {
        let resp = HealthResponse {
            status: "ok",
            uptime_secs: 42,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"status\":\"ok\""));
    }

    #[test]
    fn webhook_payload_deserializes() {
        let json = r#"{"channel":"discord","sender":"user1","body":"hello"}"#;
        let payload: WebhookPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.channel, "discord");
        assert_eq!(payload.sender, "user1");
        assert_eq!(payload.body, "hello");
    }

    #[test]
    fn validate_accepts_valid_payload() {
        let payload = WebhookPayload {
            channel: "ch".into(),
            sender: "user".into(),
            body: "hello".into(),
        };
        assert!(payload.validate().is_ok());
    }

    #[test]
    fn validate_rejects_oversized_sender() {
        let payload = WebhookPayload {
            channel: "ch".into(),
            sender: "a".repeat(257),
            body: "hello".into(),
        };
        assert!(payload.validate().is_err());
    }

    #[test]
    fn validate_rejects_oversized_channel() {
        let payload = WebhookPayload {
            channel: "c".repeat(257),
            sender: "user".into(),
            body: "hello".into(),
        };
        assert!(payload.validate().is_err());
    }

    #[test]
    fn validate_rejects_oversized_body() {
        let payload = WebhookPayload {
            channel: "ch".into(),
            sender: "user".into(),
            body: "b".repeat(65537),
        };
        assert!(payload.validate().is_err());
    }

    #[test]
    fn sanitize_strips_control_chars_keeps_newline() {
        let input = "hel\x01lo\x7f\nworld";
        let result = sanitize_control_chars(input);
        assert_eq!(result, "hello\nworld");
    }

    #[test]
    fn sanitize_strips_null_byte() {
        let input = "he\x00llo";
        let result = sanitize_control_chars(input);
        assert_eq!(result, "hello");
    }

    #[test]
    fn validate_accepts_at_limit_sender() {
        let payload = WebhookPayload {
            channel: "ch".into(),
            sender: "a".repeat(256),
            body: "hello".into(),
        };
        assert!(payload.validate().is_ok());
    }

    #[test]
    fn validate_accepts_at_limit_channel() {
        let payload = WebhookPayload {
            channel: "c".repeat(256),
            sender: "user".into(),
            body: "hello".into(),
        };
        assert!(payload.validate().is_ok());
    }

    #[test]
    fn validate_accepts_at_limit_body() {
        let payload = WebhookPayload {
            channel: "ch".into(),
            sender: "user".into(),
            body: "b".repeat(65536),
        };
        assert!(payload.validate().is_ok());
    }
}
