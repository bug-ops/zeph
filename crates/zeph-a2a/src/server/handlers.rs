use std::convert::Infallible;
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::StreamExt;
use futures::stream::Stream;
use tokio::sync::mpsc;

use crate::jsonrpc::{
    ERR_TASK_NOT_CANCELABLE, ERR_TASK_NOT_FOUND, JsonRpcError, JsonRpcResponse, METHOD_CANCEL_TASK,
    METHOD_GET_TASK, METHOD_SEND_MESSAGE, METHOD_SEND_STREAMING_MESSAGE, SendMessageParams,
    TaskIdParams,
};
use crate::types::{TaskArtifactUpdateEvent, TaskState, TaskStatusUpdateEvent};

use super::state::{AppState, CancelError, ProcessorEvent, now_rfc3339};

const ERR_METHOD_NOT_FOUND: i32 = -32601;
const ERR_INVALID_PARAMS: i32 = -32602;
const ERR_INTERNAL: i32 = -32603;

#[derive(serde::Deserialize)]
pub(super) struct RawRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id: serde_json::Value,
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

fn success_response<R: serde::Serialize>(
    id: serde_json::Value,
    result: R,
) -> JsonRpcResponse<serde_json::Value> {
    JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id,
        result: Some(serde_json::to_value(result).unwrap_or_default()),
        error: None,
    }
}

fn error_response(
    id: serde_json::Value,
    code: i32,
    message: impl Into<String>,
) -> JsonRpcResponse<serde_json::Value> {
    JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id,
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.into(),
            data: None,
        }),
    }
}

pub async fn jsonrpc_handler(
    State(state): State<AppState>,
    Json(raw): Json<RawRequest>,
) -> Json<JsonRpcResponse<serde_json::Value>> {
    let id = raw.id.clone();

    let response = match raw.method.as_str() {
        METHOD_SEND_MESSAGE => handle_send_message(state, id.clone(), raw.params).await,
        METHOD_SEND_STREAMING_MESSAGE => error_response(
            id.clone(),
            ERR_METHOD_NOT_FOUND,
            "use POST /a2a/stream for streaming",
        ),
        METHOD_GET_TASK => handle_get_task(state, id.clone(), raw.params).await,
        METHOD_CANCEL_TASK => handle_cancel_task(state, id.clone(), raw.params).await,
        _ => {
            tracing::warn!(method = %raw.method, "unknown JSON-RPC method");
            error_response(id.clone(), ERR_METHOD_NOT_FOUND, "method not found")
        }
    };

    Json(response)
}

async fn handle_send_message(
    state: AppState,
    id: serde_json::Value,
    params: serde_json::Value,
) -> JsonRpcResponse<serde_json::Value> {
    let params: SendMessageParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("invalid params in send_message: {e}");
            return error_response(id, ERR_INVALID_PARAMS, "invalid parameters");
        }
    };

    let task = state.task_manager.create_task(params.message.clone()).await;

    state
        .task_manager
        .update_status(&task.id, TaskState::Working, None)
        .await;

    let (event_tx, mut event_rx) = mpsc::channel::<ProcessorEvent>(32);
    let proc_future = state
        .processor
        .process(task.id.clone(), params.message, event_tx);

    let proc_handle = tokio::spawn(proc_future);

    let mut accumulated = String::new();
    while let Some(event) = event_rx.recv().await {
        match event {
            ProcessorEvent::ArtifactChunk { text, .. } => {
                accumulated.push_str(&text);
            }
            ProcessorEvent::StatusUpdate { .. } => {}
        }
    }

    let final_state = match proc_handle.await {
        Ok(Ok(())) => TaskState::Completed,
        Ok(Err(e)) => {
            tracing::error!(task_id = %task.id, "task processing failed: {e}");
            TaskState::Failed
        }
        Err(e) => {
            tracing::error!(task_id = %task.id, "task processor panicked: {e}");
            TaskState::Failed
        }
    };

    if final_state == TaskState::Completed && !accumulated.is_empty() {
        use crate::types::{Artifact, Part};
        let artifact = Artifact {
            artifact_id: format!("{}-artifact", task.id),
            name: None,
            parts: vec![Part::text(accumulated)],
            metadata: None,
        };
        state.task_manager.add_artifact(&task.id, artifact).await;
    }

    state
        .task_manager
        .update_status(&task.id, final_state, None)
        .await;

    match state.task_manager.get_task(&task.id, None).await {
        Some(t) => success_response(id, t),
        None => error_response(id, ERR_INTERNAL, "task vanished during processing"),
    }
}

async fn handle_get_task(
    state: AppState,
    id: serde_json::Value,
    params: serde_json::Value,
) -> JsonRpcResponse<serde_json::Value> {
    let params: TaskIdParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("invalid params in get_task: {e}");
            return error_response(id, ERR_INVALID_PARAMS, "invalid parameters");
        }
    };

    match state
        .task_manager
        .get_task(&params.id, params.history_length)
        .await
    {
        Some(task) => success_response(id, task),
        None => error_response(id, ERR_TASK_NOT_FOUND, "task not found"),
    }
}

async fn handle_cancel_task(
    state: AppState,
    id: serde_json::Value,
    params: serde_json::Value,
) -> JsonRpcResponse<serde_json::Value> {
    let params: TaskIdParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("invalid params in cancel_task: {e}");
            return error_response(id, ERR_INVALID_PARAMS, "invalid parameters");
        }
    };

    match state.task_manager.cancel_task(&params.id).await {
        Ok(task) => success_response(id, task),
        Err(CancelError::NotFound) => error_response(id, ERR_TASK_NOT_FOUND, "task not found"),
        Err(CancelError::NotCancelable(s)) => error_response(
            id,
            ERR_TASK_NOT_CANCELABLE,
            format!("task in state {s:?} cannot be canceled"),
        ),
    }
}

pub async fn agent_card_handler(State(state): State<AppState>) -> Json<crate::types::AgentCard> {
    Json(state.card.clone())
}

#[derive(serde::Deserialize)]
pub(super) struct StreamRequest {
    #[serde(default)]
    params: StreamParams,
}

#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct StreamParams {
    message: Option<crate::types::Message>,
}

fn sse_rpc_event(result: &impl serde::Serialize) -> Event {
    let rpc = crate::jsonrpc::JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id: serde_json::Value::Null,
        result: Some(serde_json::to_value(result).unwrap_or_default()),
        error: None,
    };
    Event::default().data(serde_json::to_string(&rpc).unwrap_or_default())
}

fn status_event(
    task_id: &str,
    context_id: Option<&String>,
    state: TaskState,
    is_final: bool,
) -> Event {
    let event = TaskStatusUpdateEvent {
        kind: "status-update".into(),
        task_id: task_id.to_owned(),
        context_id: context_id.cloned(),
        status: crate::types::TaskStatus {
            state,
            timestamp: now_rfc3339(),
            message: None,
        },
        is_final,
    };
    sse_rpc_event(&event)
}

pub async fn stream_handler(
    State(state): State<AppState>,
    Json(req): Json<StreamRequest>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = mpsc::channel::<Event>(32);

    tokio::spawn(async move {
        let Some(message) = req.params.message else {
            let _ = tx
                .send(
                    Event::default()
                        .event("error")
                        .data("{\"code\":-32700,\"message\":\"missing message param\"}"),
                )
                .await;
            return;
        };

        stream_task(state, message, tx).await;
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok);
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

async fn stream_task(state: AppState, message: crate::types::Message, tx: mpsc::Sender<Event>) {
    let task = state.task_manager.create_task(message.clone()).await;
    let task_id = task.id.clone();
    let context_id = task.context_id.clone();

    state
        .task_manager
        .update_status(&task_id, TaskState::Working, None)
        .await;
    let _ = tx
        .send(status_event(
            &task_id,
            context_id.as_ref(),
            TaskState::Working,
            false,
        ))
        .await;

    let (event_tx, mut event_rx) = mpsc::channel::<ProcessorEvent>(32);
    let proc_future = state.processor.process(task_id.clone(), message, event_tx);

    let proc_handle = tokio::spawn(proc_future);

    let mut accumulated = String::new();
    while let Some(event) = event_rx.recv().await {
        match event {
            ProcessorEvent::ArtifactChunk { text, is_final } => {
                accumulated.push_str(&text);
                let artifact = crate::types::Artifact {
                    artifact_id: uuid::Uuid::new_v4().to_string(),
                    name: None,
                    parts: vec![crate::types::Part::text(text)],
                    metadata: None,
                };
                let evt = TaskArtifactUpdateEvent {
                    kind: "artifact-update".into(),
                    task_id: task_id.clone(),
                    context_id: context_id.clone(),
                    artifact,
                    is_final,
                };
                let _ = tx.send(sse_rpc_event(&evt)).await;
            }
            ProcessorEvent::StatusUpdate {
                state: task_state,
                is_final,
            } => {
                state
                    .task_manager
                    .update_status(&task_id, task_state, None)
                    .await;
                let _ = tx
                    .send(status_event(
                        &task_id,
                        context_id.as_ref(),
                        task_state,
                        is_final,
                    ))
                    .await;
            }
        }
    }

    let final_state = match proc_handle.await {
        Ok(Ok(())) => TaskState::Completed,
        Ok(Err(e)) => {
            tracing::error!(task_id = %task_id, "stream task processing failed: {e}");
            TaskState::Failed
        }
        Err(e) => {
            tracing::error!(task_id = %task_id, "stream task processor panicked: {e}");
            TaskState::Failed
        }
    };

    if final_state == TaskState::Completed && !accumulated.is_empty() {
        use crate::types::{Artifact, Part};
        let artifact = Artifact {
            artifact_id: format!("{task_id}-artifact"),
            name: None,
            parts: vec![Part::text(accumulated)],
            metadata: None,
        };
        state.task_manager.add_artifact(&task_id, artifact).await;
    }

    state
        .task_manager
        .update_status(&task_id, final_state, None)
        .await;
    let _ = tx
        .send(status_event(
            &task_id,
            context_id.as_ref(),
            final_state,
            true,
        ))
        .await;
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::super::router::build_router_with_config;
    use super::super::testing::test_state;

    fn make_rpc_request(method: &str, params: serde_json::Value) -> axum::http::Request<Body> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": method,
            "params": params,
        });
        axum::http::Request::builder()
            .method("POST")
            .uri("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    async fn get_rpc_body(resp: axum::http::Response<Body>) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn unknown_method_does_not_echo_method_name() {
        let app = build_router_with_config(test_state(), None, 0);
        let req = make_rpc_request("tasks/evil_probe", serde_json::json!({}));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body = get_rpc_body(resp).await;
        let msg = body["error"]["message"].as_str().unwrap_or("");
        assert_eq!(msg, "method not found", "must not echo method name");
        assert!(
            !msg.contains("evil_probe"),
            "method name must not appear in error"
        );
        assert!(
            !msg.contains("unknown"),
            "must not leak 'unknown method' phrasing"
        );
    }

    #[tokio::test]
    async fn invalid_params_send_message_no_serde_details() {
        let app = build_router_with_config(test_state(), None, 0);
        // Pass wrong type for message to trigger serde deserialization error
        let req = make_rpc_request("message/send", serde_json::json!({"message": 42}));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body = get_rpc_body(resp).await;
        let msg = body["error"]["message"].as_str().unwrap_or("");
        assert_eq!(msg, "invalid parameters");
        // Serde error text like "invalid type" or field names must not leak
        assert!(!msg.contains("invalid type"), "serde details must not leak");
        assert!(!msg.contains("expected"), "serde details must not leak");
    }

    #[tokio::test]
    async fn invalid_params_get_task_no_serde_details() {
        let app = build_router_with_config(test_state(), None, 0);
        // Pass wrong type for id field
        let req = make_rpc_request("tasks/get", serde_json::json!({"id": [1, 2, 3]}));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body = get_rpc_body(resp).await;
        let msg = body["error"]["message"].as_str().unwrap_or("");
        assert_eq!(msg, "invalid parameters");
        assert!(!msg.contains("invalid type"), "serde details must not leak");
    }

    #[tokio::test]
    async fn invalid_params_cancel_task_no_serde_details() {
        let app = build_router_with_config(test_state(), None, 0);
        let req = make_rpc_request("tasks/cancel", serde_json::json!({"id": false}));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body = get_rpc_body(resp).await;
        let msg = body["error"]["message"].as_str().unwrap_or("");
        assert_eq!(msg, "invalid parameters");
        assert!(!msg.contains("invalid type"), "serde details must not leak");
    }

    // Multi-chunk ArtifactChunk accumulation test

    struct MultiChunkProcessor;

    impl super::super::state::TaskProcessor for MultiChunkProcessor {
        fn process(
            &self,
            _task_id: String,
            _message: crate::types::Message,
            event_tx: tokio::sync::mpsc::Sender<super::super::state::ProcessorEvent>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(), crate::error::A2aError>> + Send>,
        > {
            Box::pin(async move {
                let _ = event_tx
                    .send(super::super::state::ProcessorEvent::ArtifactChunk {
                        text: "chunk1".into(),
                        is_final: false,
                    })
                    .await;
                let _ = event_tx
                    .send(super::super::state::ProcessorEvent::ArtifactChunk {
                        text: " chunk2".into(),
                        is_final: false,
                    })
                    .await;
                let _ = event_tx
                    .send(super::super::state::ProcessorEvent::ArtifactChunk {
                        text: " chunk3".into(),
                        is_final: true,
                    })
                    .await;
                let _ = event_tx
                    .send(super::super::state::ProcessorEvent::StatusUpdate {
                        state: crate::types::TaskState::Completed,
                        is_final: true,
                    })
                    .await;
                Ok(())
            })
        }
    }

    fn multi_chunk_state() -> super::super::state::AppState {
        use std::sync::Arc;
        super::super::state::AppState {
            card: super::super::testing::test_card(),
            task_manager: super::super::state::TaskManager::new(),
            processor: Arc::new(MultiChunkProcessor),
        }
    }

    #[tokio::test]
    async fn multi_chunk_accumulation_produces_joined_artifact() {
        let app = build_router_with_config(multi_chunk_state(), None, 0);
        let msg = crate::types::Message {
            role: crate::types::Role::User,
            parts: vec![crate::types::Part::Text {
                text: "hello".into(),
                metadata: None,
            }],
            message_id: Some("m1".into()),
            task_id: None,
            context_id: None,
            metadata: None,
        };
        let req = make_rpc_request(
            "message/send",
            serde_json::json!({ "message": serde_json::to_value(&msg).unwrap() }),
        );
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body = get_rpc_body(resp).await;
        // Result should contain artifact with all chunks joined
        let artifacts = &body["result"]["artifacts"];
        let text = artifacts[0]["parts"][0]["text"].as_str().unwrap_or("");
        assert_eq!(text, "chunk1 chunk2 chunk3");
    }
}
