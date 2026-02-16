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
    let msg = format!("[{}@{}] {}", payload.sender, payload.channel, payload.body);
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
}
