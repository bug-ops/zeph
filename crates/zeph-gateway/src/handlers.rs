// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;

use super::server::AppState;

/// JSON body expected on `POST /webhook`.
///
/// All three fields are required.  Individual field limits are enforced by
/// [`WebhookPayload::validate`] before the message is forwarded to the agent.
#[derive(serde::Deserialize)]
pub(crate) struct WebhookPayload {
    /// Logical channel name (e.g. `"discord"`, `"slack"`). Maximum 256 bytes.
    pub channel: String,
    /// Display name or identifier of the message sender. Maximum 256 bytes.
    pub sender: String,
    /// Raw message content. Maximum 65 536 bytes.
    pub body: String,
}

impl WebhookPayload {
    /// Validate field lengths before forwarding to the agent.
    ///
    /// Returns `Ok(())` when all fields are within their limits, or `Err` with a
    /// human-readable description of the first violation.
    ///
    /// | Field | Limit |
    /// |---|---|
    /// | `sender` | 256 bytes |
    /// | `channel` | 256 bytes |
    /// | `body` | 65 536 bytes |
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

/// JSON body returned by a successful `POST /webhook` call.
#[derive(serde::Serialize)]
struct WebhookResponse {
    /// Always `"accepted"` on success.
    status: &'static str,
}

/// JSON body returned by `GET /health`.
#[derive(serde::Serialize)]
struct HealthResponse {
    /// Always `"ok"`.
    status: &'static str,
    /// Seconds elapsed since the server started.
    uptime_secs: u64,
}

/// Handler for `POST /webhook`.
///
/// Validates the payload, sanitises `sender` and `channel` by stripping control
/// characters, then forwards the message as `"[sender@channel] body"` on the
/// internal webhook channel.
///
/// # Responses
///
/// | Status | Condition |
/// |---|---|
/// | 200 | Message accepted and queued |
/// | 422 | Payload failed field-length validation |
/// | 503 | Internal channel is closed (agent shut down) |
pub(crate) async fn webhook_handler(
    State(state): State<AppState>,
    Json(payload): Json<WebhookPayload>,
) -> impl IntoResponse {
    if let Err(e) = payload.validate() {
        return (StatusCode::UNPROCESSABLE_ENTITY, e).into_response();
    }
    let sender = zeph_common::sanitize::strip_control_chars_preserve_whitespace(&payload.sender);
    let channel = zeph_common::sanitize::strip_control_chars_preserve_whitespace(&payload.channel);
    let msg = format!("[{}@{}] {}", sender, channel, payload.body);
    match state.webhook_tx.send(msg).await {
        Ok(()) => Json(WebhookResponse { status: "accepted" }).into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

/// Handler for `GET /health`.
///
/// Returns a JSON object with a static `"ok"` status and the server uptime in
/// seconds.  This endpoint bypasses authentication and rate limiting so that
/// load balancers can poll it freely.
///
/// # Response body
///
/// ```json
/// { "status": "ok", "uptime_secs": 42 }
/// ```
pub(crate) async fn health_handler(State(state): State<AppState>) -> impl IntoResponse {
    Json(HealthResponse {
        status: "ok",
        uptime_secs: state.started_at.elapsed().as_secs(),
    })
}

/// Handler for `GET /metrics` (Prometheus scrape endpoint).
///
/// Returns the current registry contents encoded as `OpenMetrics` 1.0.0 text format, suitable for
/// scraping by Prometheus or any compatible monitoring system.
///
/// This handler requires `State<Arc<Registry>>` injected via the nested router in
/// [`crate::GatewayServer::with_metrics_registry`].
///
/// # Responses
///
/// | Status | Condition |
/// |---|---|
/// | 200 | Registry encoded successfully; `Content-Type: application/openmetrics-text; version=1.0.0; charset=utf-8` |
/// | 500 | Registry encoding failed (logged as error) |
#[cfg(feature = "prometheus")]
pub(crate) async fn metrics_handler(
    axum::extract::State(registry): axum::extract::State<
        std::sync::Arc<prometheus_client::registry::Registry>,
    >,
) -> impl axum::response::IntoResponse {
    let mut buf = String::new();
    match prometheus_client::encoding::text::encode(&mut buf, &registry) {
        Ok(()) => (
            [(
                axum::http::header::CONTENT_TYPE,
                "application/openmetrics-text; version=1.0.0; charset=utf-8",
            )],
            buf,
        )
            .into_response(),
        Err(e) => {
            tracing::error!("failed to encode prometheus metrics: {e}");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "metrics encoding failed",
            )
                .into_response()
        }
    }
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
        let result = zeph_common::sanitize::strip_control_chars_preserve_whitespace(input);
        assert_eq!(result, "hello\nworld");
    }

    #[test]
    fn sanitize_strips_null_byte() {
        let input = "he\x00llo";
        let result = zeph_common::sanitize::strip_control_chars_preserve_whitespace(input);
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
