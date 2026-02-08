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

use super::state::{AppState, CancelError, now_rfc3339};

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
        _ => error_response(
            id.clone(),
            ERR_METHOD_NOT_FOUND,
            format!("unknown method: {}", raw.method),
        ),
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
        Err(e) => return error_response(id, ERR_INVALID_PARAMS, format!("invalid params: {e}")),
    };

    let task = state.task_manager.create_task(params.message.clone()).await;

    state
        .task_manager
        .update_status(&task.id, TaskState::Working, None)
        .await;

    match state
        .processor
        .process(task.id.clone(), params.message)
        .await
    {
        Ok(result) => {
            state
                .task_manager
                .append_history(&task.id, result.response)
                .await;
            for artifact in result.artifacts {
                state.task_manager.add_artifact(&task.id, artifact).await;
            }
            state
                .task_manager
                .update_status(&task.id, TaskState::Completed, None)
                .await;
        }
        Err(e) => {
            tracing::error!(task_id = %task.id, "task processing failed: {e}");
            state
                .task_manager
                .update_status(&task.id, TaskState::Failed, None)
                .await;
        }
    }

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
        Err(e) => return error_response(id, ERR_INVALID_PARAMS, format!("invalid params: {e}")),
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
        Err(e) => return error_response(id, ERR_INVALID_PARAMS, format!("invalid params: {e}")),
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

    match state.processor.process(task_id.clone(), message).await {
        Ok(result) => {
            state
                .task_manager
                .append_history(&task_id, result.response)
                .await;

            for artifact in result.artifacts {
                let evt = TaskArtifactUpdateEvent {
                    kind: "artifact-update".into(),
                    task_id: task_id.clone(),
                    context_id: context_id.clone(),
                    artifact: artifact.clone(),
                    is_final: false,
                };
                let _ = tx.send(sse_rpc_event(&evt)).await;
                state.task_manager.add_artifact(&task_id, artifact).await;
            }

            state
                .task_manager
                .update_status(&task_id, TaskState::Completed, None)
                .await;
            let _ = tx
                .send(status_event(
                    &task_id,
                    context_id.as_ref(),
                    TaskState::Completed,
                    true,
                ))
                .await;
        }
        Err(e) => {
            tracing::error!(task_id = %task_id, "stream task processing failed: {e}");
            state
                .task_manager
                .update_status(&task_id, TaskState::Failed, None)
                .await;
            let _ = tx
                .send(status_event(
                    &task_id,
                    context_id.as_ref(),
                    TaskState::Failed,
                    true,
                ))
                .await;
        }
    }
}
