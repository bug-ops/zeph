// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#![cfg(test)]

use wiremock::ResponseTemplate;

pub fn agent_card_response(name: &str, base_url: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "name": name,
        "description": "test agent",
        "url": base_url,
        "version": "0.1.0",
        "protocolVersion": crate::A2A_PROTOCOL_VERSION,
        "capabilities": {"streaming": true, "pushNotifications": false, "stateTransitionHistory": false},
        "defaultInputModes": [],
        "defaultOutputModes": [],
        "skills": []
    }))
}

pub fn task_rpc_response(task_id: &str, state: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "jsonrpc": "2.0",
        "id": "req-1",
        "result": {
            "id": task_id,
            "status": {
                "state": state,
                "timestamp": "2026-01-01T00:00:00Z"
            }
        }
    }))
}

pub fn task_rpc_error_response(code: i32, message: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "jsonrpc": "2.0",
        "id": "req-1",
        "error": {
            "code": code,
            "message": message
        }
    }))
}

pub fn sse_task_events_response(task_id: &str, content: &str) -> ResponseTemplate {
    let status_event = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "stream-1",
        "result": {
            "kind": "status-update",
            "taskId": task_id,
            "status": {
                "state": "working",
                "timestamp": "2026-01-01T00:00:00Z"
            },
            "final": false
        }
    });
    let artifact_event = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "stream-1",
        "result": {
            "kind": "artifact-update",
            "taskId": task_id,
            "artifact": {
                "artifactId": "a-1",
                "parts": [{"kind": "text", "text": content}]
            },
            "final": true
        }
    });
    let body = format!(
        "data: {}\n\ndata: {}\n\n",
        serde_json::to_string(&status_event).unwrap(),
        serde_json::to_string(&artifact_event).unwrap()
    );
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}
