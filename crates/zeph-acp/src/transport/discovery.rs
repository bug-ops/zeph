// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use agent_client_protocol as acp;
use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;
use serde_json::{Value, json};

use crate::transport::http::AcpHttpState;

/// `GET /.well-known/acp.json` — publicly accessible agent discovery manifest.
///
/// Returns a JSON document describing the agent's identity, supported transports,
/// and authentication requirements. This endpoint is never behind auth middleware.
pub async fn discovery_handler(State(state): State<AcpHttpState>) -> impl IntoResponse {
    let auth = if state.server_config.auth_bearer_token.is_some() {
        json!({ "type": "bearer" })
    } else {
        Value::Null
    };

    let manifest = json!({
        "name": state.server_config.agent_name,
        "version": state.server_config.agent_version,
        "protocol": "acp",
        "protocol_version": acp::ProtocolVersion::LATEST,
        "transports": {
            "http_sse": { "url": "/acp" },
            "websocket": { "url": "/acp/ws" },
            "health": { "url": "/health" }
        },
        "authentication": auth,
        "readiness": {
            "stdio_notification": "zeph/ready",
            "http_health_endpoint": "/health"
        }
    });

    Json(manifest)
}
