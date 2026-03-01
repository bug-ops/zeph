// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use agent_client_protocol as acp;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::agent::ZephAcpAgent;

// ── Constants ─────────────────────────────────────────────────────────────────

const MAX_IMPORT_EVENTS: usize = 10_000;
const MAX_SESSION_ID_LEN: usize = 128;

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct SessionListParams {}

#[derive(Serialize, Deserialize)]
pub(crate) struct SessionListEntry {
    pub session_id: String,
    pub created_at: String,
    pub busy: bool,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SessionListResponse {
    pub sessions: Vec<SessionListEntry>,
}

#[derive(Deserialize)]
pub(crate) struct SessionGetParams {
    pub session_id: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SessionEventEntry {
    pub event_type: String,
    pub payload: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SessionGetResponse {
    pub session_id: String,
    pub created_at: String,
    pub busy: bool,
    pub events: Vec<SessionEventEntry>,
}

#[derive(Deserialize)]
pub(crate) struct SessionDeleteParams {
    pub session_id: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SessionDeleteResponse {
    pub deleted: bool,
}

#[derive(Deserialize)]
pub(crate) struct SessionExportParams {
    pub session_id: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SessionExportResponse {
    pub session_id: String,
    pub events: Vec<SessionEventEntry>,
    pub exported_at: String,
}

#[derive(Deserialize)]
pub(crate) struct SessionImportParams {
    pub events: Vec<SessionEventEntry>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SessionImportResponse {
    pub session_id: String,
}

#[derive(Deserialize)]
pub(crate) struct AgentToolsParams {
    #[expect(
        dead_code,
        reason = "required for JSON deserialization of the ACP ext method params"
    )]
    pub session_id: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ToolInfo {
    pub id: String,
    pub description: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct AgentToolsResponse {
    pub tools: Vec<ToolInfo>,
}

#[derive(Deserialize)]
pub(crate) struct WorkingDirUpdateParams {
    pub session_id: String,
    pub path: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct WorkingDirUpdateResponse {
    pub updated: bool,
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

/// Dispatch an `ExtRequest` to the appropriate custom method handler.
/// Returns `None` if the method name is not recognized.
pub(crate) fn dispatch<'a>(
    agent: &'a ZephAcpAgent,
    req: &'a acp::ExtRequest,
) -> Option<Pin<Box<dyn std::future::Future<Output = acp::Result<acp::ExtResponse>> + 'a>>> {
    match req.method.as_ref() {
        "_session/list" => Some(Box::pin(handle_session_list(agent, &req.params))),
        "_session/get" => Some(Box::pin(handle_session_get(agent, &req.params))),
        "_session/delete" => Some(Box::pin(handle_session_delete(agent, &req.params))),
        "_session/export" => Some(Box::pin(handle_session_export(agent, &req.params))),
        "_session/import" => Some(Box::pin(handle_session_import(agent, &req.params))),
        "_agent/tools" => Some(Box::pin(handle_agent_tools(agent, &req.params))),
        "_agent/working_dir/update" => {
            Some(Box::pin(handle_working_dir_update(agent, &req.params)))
        }
        _ => None,
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn parse_params<T: serde::de::DeserializeOwned>(raw: &Arc<RawValue>) -> acp::Result<T> {
    serde_json::from_str(raw.get()).map_err(|e| acp::Error::invalid_request().data(e.to_string()))
}

fn to_ext_response<T: Serialize>(value: &T) -> acp::Result<acp::ExtResponse> {
    let json = serde_json::to_string(value)
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
    let raw = RawValue::from_string(json)
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
    Ok(acp::ExtResponse::new(Arc::from(raw)))
}

fn session_not_found() -> acp::Error {
    acp::Error::invalid_request().data("session not found")
}

fn now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Validate `session_id`: reject if too long or contains characters outside `[a-zA-Z0-9_-]`.
fn validate_session_id(id: &str) -> acp::Result<()> {
    if id.len() > MAX_SESSION_ID_LEN {
        return Err(acp::Error::invalid_request().data("session_id too long"));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(acp::Error::invalid_request().data("session_id contains invalid characters"));
    }
    Ok(())
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn handle_session_list(
    agent: &ZephAcpAgent,
    raw: &Arc<RawValue>,
) -> acp::Result<acp::ExtResponse> {
    // Deprecated: use the native `list_sessions` ACP method instead.
    // This extension method returns a reduced `SessionListEntry` schema and will be removed
    // in a future release.
    tracing::warn!(
        "ext method `_session/list` is deprecated; use the native `list_sessions` ACP method"
    );
    let _: SessionListParams = parse_params(raw)?;

    // Collect in-memory session tuples while holding the borrow, then release it.
    let in_memory: Vec<(String, String, bool)> = {
        let sessions = agent.sessions.borrow();
        let mut tuples = Vec::with_capacity(sessions.len());
        for (id, entry) in sessions.iter() {
            let sid = id.to_string();
            let busy = entry.output_rx.borrow().is_none();
            let created_at = entry.created_at.format("%Y-%m-%dT%H:%M:%SZ").to_string();
            tuples.push((sid, created_at, busy));
        }
        tuples
    };

    // Pre-size map: persisted count unknown, use in-memory size as initial hint.
    let mut sessions: std::collections::HashMap<String, SessionListEntry> =
        std::collections::HashMap::with_capacity(in_memory.len());

    // Load persisted sessions first (lower priority, overridden by in-memory).
    if let Some(ref store) = agent.store {
        match store.list_acp_sessions(0).await {
            Ok(rows) => {
                sessions.reserve(rows.len());
                for row in rows {
                    sessions.insert(
                        row.id.clone(),
                        SessionListEntry {
                            session_id: row.id,
                            created_at: row.created_at,
                            busy: false,
                        },
                    );
                }
            }
            Err(e) => tracing::warn!(error = %e, "failed to list persisted ACP sessions"),
        }
    }

    // Override with live in-memory sessions.
    for (sid, created_at, busy) in in_memory {
        sessions.insert(
            sid.clone(),
            SessionListEntry {
                session_id: sid,
                created_at,
                busy,
            },
        );
    }

    let resp = SessionListResponse {
        sessions: sessions.into_values().collect(),
    };
    to_ext_response(&resp)
}

async fn handle_session_get(
    agent: &ZephAcpAgent,
    raw: &Arc<RawValue>,
) -> acp::Result<acp::ExtResponse> {
    let params: SessionGetParams = parse_params(raw)?;
    let sid = params.session_id.as_str();
    validate_session_id(sid)?;

    let (in_memory, created_at, busy) = {
        let sessions = agent.sessions.borrow();
        if let Some(entry) = sessions.get(&acp::SessionId::new(sid)) {
            let busy = entry.output_rx.borrow().is_none();
            let created_at = entry.created_at.format("%Y-%m-%dT%H:%M:%SZ").to_string();
            (true, created_at, busy)
        } else {
            (false, now_iso8601(), false)
        }
    };

    if !in_memory {
        match &agent.store {
            Some(store) => {
                let exists = store
                    .acp_session_exists(sid)
                    .await
                    .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
                if !exists {
                    return Err(session_not_found());
                }
            }
            None => return Err(session_not_found()),
        }
    }

    let events = if let Some(ref store) = agent.store {
        match store.load_acp_events(sid).await {
            Ok(evs) => evs
                .into_iter()
                .map(|e| SessionEventEntry {
                    event_type: e.event_type,
                    payload: e.payload,
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, session_id = %sid, "failed to load ACP events");
                vec![]
            }
        }
    } else {
        vec![]
    };

    let resp = SessionGetResponse {
        session_id: sid.to_owned(),
        created_at,
        busy,
        events,
    };
    to_ext_response(&resp)
}

async fn handle_session_delete(
    agent: &ZephAcpAgent,
    raw: &Arc<RawValue>,
) -> acp::Result<acp::ExtResponse> {
    let params: SessionDeleteParams = parse_params(raw)?;
    validate_session_id(&params.session_id)?;

    let acp_id = acp::SessionId::new(params.session_id.as_str());
    let removed_memory = agent.sessions.borrow_mut().remove(&acp_id).is_some();
    if removed_memory {
        // cancel_signal already dropped with the entry; nothing extra needed.
        tracing::debug!(session_id = %params.session_id, "removed in-memory ACP session");
    }

    let removed_store = if let Some(ref store) = agent.store {
        match store.delete_acp_session(&params.session_id).await {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, session_id = %params.session_id, "failed to delete ACP session from store");
                false
            }
        }
    } else {
        false
    };

    to_ext_response(&SessionDeleteResponse {
        deleted: removed_memory || removed_store,
    })
}

async fn handle_session_export(
    agent: &ZephAcpAgent,
    raw: &Arc<RawValue>,
) -> acp::Result<acp::ExtResponse> {
    let params: SessionExportParams = parse_params(raw)?;
    validate_session_id(&params.session_id)?;

    let events = match &agent.store {
        Some(store) => store
            .load_acp_events(&params.session_id)
            .await
            .map_err(|e| acp::Error::internal_error().data(e.to_string()))?
            .into_iter()
            .map(|e| SessionEventEntry {
                event_type: e.event_type,
                payload: e.payload,
            })
            .collect(),
        None => vec![],
    };

    to_ext_response(&SessionExportResponse {
        session_id: params.session_id,
        events,
        exported_at: now_iso8601(),
    })
}

async fn handle_session_import(
    agent: &ZephAcpAgent,
    raw: &Arc<RawValue>,
) -> acp::Result<acp::ExtResponse> {
    let params: SessionImportParams = parse_params(raw)?;

    if params.events.len() > MAX_IMPORT_EVENTS {
        return Err(acp::Error::invalid_request()
            .data(format!("too many events: limit is {MAX_IMPORT_EVENTS}")));
    }

    let new_id = uuid::Uuid::new_v4().to_string();

    if let Some(ref store) = agent.store {
        store
            .create_acp_session(&new_id)
            .await
            .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
        let pairs: Vec<(&str, &str)> = params
            .events
            .iter()
            .map(|e| (e.event_type.as_str(), e.payload.as_str()))
            .collect();
        store
            .import_acp_events(&new_id, &pairs)
            .await
            .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
    }

    to_ext_response(&SessionImportResponse { session_id: new_id })
}

#[allow(clippy::unused_async)]
async fn handle_agent_tools(
    _agent: &ZephAcpAgent,
    raw: &Arc<RawValue>,
) -> acp::Result<acp::ExtResponse> {
    let _params: AgentToolsParams = parse_params(raw)?;

    let tools = vec![
        ToolInfo {
            id: "bash".to_owned(),
            description: "Execute shell commands".to_owned(),
        },
        ToolInfo {
            id: "read_file".to_owned(),
            description: "Read file contents".to_owned(),
        },
        ToolInfo {
            id: "write_file".to_owned(),
            description: "Write or update file contents".to_owned(),
        },
        ToolInfo {
            id: "search".to_owned(),
            description: "Search file content with regex".to_owned(),
        },
        ToolInfo {
            id: "web_scrape".to_owned(),
            description: "Fetch and extract content from a URL".to_owned(),
        },
    ];

    to_ext_response(&AgentToolsResponse { tools })
}

#[allow(clippy::unused_async)]
async fn handle_working_dir_update(
    agent: &ZephAcpAgent,
    raw: &Arc<RawValue>,
) -> acp::Result<acp::ExtResponse> {
    let params: WorkingDirUpdateParams = parse_params(raw)?;
    validate_session_id(&params.session_id)?;

    // Reject path traversal: disallow any ParentDir (..) component.
    let p = std::path::Path::new(&params.path);
    if p.components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(acp::Error::invalid_request().data("path traversal not allowed"));
    }

    let acp_id = acp::SessionId::new(params.session_id.as_str());
    let updated = {
        let sessions = agent.sessions.borrow();
        if let Some(entry) = sessions.get(&acp_id) {
            *entry.working_dir.borrow_mut() = Some(PathBuf::from(&params.path));
            true
        } else {
            false
        }
    };

    to_ext_response(&WorkingDirUpdateResponse { updated })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agent_client_protocol as acp;
    use serde_json::value::RawValue;
    use tokio::sync::{mpsc, oneshot};

    use crate::agent::ZephAcpAgent;
    use crate::transport::ConnSlot;

    fn make_spawner() -> crate::agent::AgentSpawner {
        Arc::new(|_channel, _ctx| Box::pin(async {}))
    }

    fn make_agent() -> (
        ZephAcpAgent,
        mpsc::UnboundedReceiver<(acp::SessionNotification, oneshot::Sender<()>)>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let conn_slot: ConnSlot = std::rc::Rc::new(std::cell::RefCell::new(None));
        (
            ZephAcpAgent::new(make_spawner(), tx, conn_slot, 4, 1800, None),
            rx,
        )
    }

    fn null_params() -> Arc<RawValue> {
        Arc::from(RawValue::from_string("{}".to_owned()).unwrap())
    }

    fn params_json(json: &str) -> Arc<RawValue> {
        Arc::from(RawValue::from_string(json.to_owned()).unwrap())
    }

    #[tokio::test]
    async fn dispatch_returns_none_for_unknown_method() {
        let (agent, _rx) = make_agent();
        let req = acp::ExtRequest::new("unknown/method", null_params());
        assert!(super::dispatch(&agent, &req).is_none());
    }

    #[tokio::test]
    async fn dispatch_session_list_returns_empty() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                let req = acp::ExtRequest::new("_session/list", null_params());
                let fut = super::dispatch(&agent, &req).unwrap();
                let resp = fut.await.unwrap();
                let parsed: super::SessionListResponse =
                    serde_json::from_str(resp.0.get()).unwrap();
                assert!(parsed.sessions.is_empty());
            })
            .await;
    }

    #[tokio::test]
    async fn dispatch_session_list_includes_in_memory_session() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let sid = resp.session_id.to_string();

                let req = acp::ExtRequest::new("_session/list", null_params());
                let fut = super::dispatch(&agent, &req).unwrap();
                let list_resp: super::SessionListResponse =
                    serde_json::from_str(fut.await.unwrap().0.get()).unwrap();
                assert!(list_resp.sessions.iter().any(|s| s.session_id == sid));
            })
            .await;
    }

    #[tokio::test]
    async fn dispatch_session_delete_removes_session() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let sid = resp.session_id.to_string();

                let json = format!(r#"{{"session_id":"{sid}"}}"#);
                let req = acp::ExtRequest::new("_session/delete", params_json(&json));
                let fut = super::dispatch(&agent, &req).unwrap();
                let del_resp: super::SessionDeleteResponse =
                    serde_json::from_str(fut.await.unwrap().0.get()).unwrap();
                assert!(del_resp.deleted);
                assert!(
                    !agent
                        .sessions
                        .borrow()
                        .contains_key(&acp::SessionId::new(sid.as_str()))
                );
            })
            .await;
    }

    #[tokio::test]
    async fn dispatch_session_delete_returns_false_for_unknown() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                let json = r#"{"session_id":"no-such-session"}"#;
                let req = acp::ExtRequest::new("_session/delete", params_json(json));
                let fut = super::dispatch(&agent, &req).unwrap();
                let del_resp: super::SessionDeleteResponse =
                    serde_json::from_str(fut.await.unwrap().0.get()).unwrap();
                assert!(!del_resp.deleted);
            })
            .await;
    }

    #[tokio::test]
    async fn dispatch_agent_tools_returns_list() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                let json = r#"{"session_id":"any-session"}"#;
                let req = acp::ExtRequest::new("_agent/tools", params_json(json));
                let fut = super::dispatch(&agent, &req).unwrap();
                let tools_resp: super::AgentToolsResponse =
                    serde_json::from_str(fut.await.unwrap().0.get()).unwrap();
                assert!(!tools_resp.tools.is_empty());
            })
            .await;
    }

    #[tokio::test]
    async fn dispatch_working_dir_update_returns_false_for_unknown() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                let json = r#"{"session_id":"no-such-session","path":"/tmp"}"#;
                let req = acp::ExtRequest::new("_agent/working_dir/update", params_json(json));
                let fut = super::dispatch(&agent, &req).unwrap();
                let wd_resp: super::WorkingDirUpdateResponse =
                    serde_json::from_str(fut.await.unwrap().0.get()).unwrap();
                assert!(!wd_resp.updated);
            })
            .await;
    }

    #[tokio::test]
    async fn dispatch_working_dir_update_stores_path() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let sid = resp.session_id.to_string();

                let json = format!(r#"{{"session_id":"{sid}","path":"/workspace"}}"#);
                let req = acp::ExtRequest::new("_agent/working_dir/update", params_json(&json));
                let fut = super::dispatch(&agent, &req).unwrap();
                let wd_resp: super::WorkingDirUpdateResponse =
                    serde_json::from_str(fut.await.unwrap().0.get()).unwrap();
                assert!(wd_resp.updated);

                let sessions = agent.sessions.borrow();
                let entry = sessions.get(&acp::SessionId::new(sid.as_str())).unwrap();
                assert_eq!(
                    entry.working_dir.borrow().as_deref(),
                    Some(std::path::Path::new("/workspace"))
                );
            })
            .await;
    }

    #[tokio::test]
    async fn working_dir_update_rejects_path_traversal() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let sid = resp.session_id.to_string();

                let json = format!(r#"{{"session_id":"{sid}","path":"../../etc/passwd"}}"#);
                let req = acp::ExtRequest::new("_agent/working_dir/update", params_json(&json));
                let fut = super::dispatch(&agent, &req).unwrap();
                assert!(fut.await.is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn session_import_rejects_oversized() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                // Build a payload with MAX_IMPORT_EVENTS + 1 events.
                let events: Vec<_> = (0..=super::MAX_IMPORT_EVENTS)
                    .map(|i| {
                        serde_json::json!({"event_type": "user_message", "payload": i.to_string()})
                    })
                    .collect();
                let json = serde_json::to_string(&serde_json::json!({ "events": events })).unwrap();
                let req = acp::ExtRequest::new("_session/import", params_json(&json));
                let fut = super::dispatch(&agent, &req).unwrap();
                assert!(fut.await.is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn session_import_no_store_returns_new_id() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                let json = r#"{"events":[]}"#;
                let req = acp::ExtRequest::new("_session/import", params_json(json));
                let fut = super::dispatch(&agent, &req).unwrap();
                let import_resp: super::SessionImportResponse =
                    serde_json::from_str(fut.await.unwrap().0.get()).unwrap();
                assert!(!import_resp.session_id.is_empty());
            })
            .await;
    }

    #[tokio::test]
    async fn session_export_no_store_returns_empty_events() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                let json = r#"{"session_id":"any-session-id"}"#;
                let req = acp::ExtRequest::new("_session/export", params_json(json));
                let fut = super::dispatch(&agent, &req).unwrap();
                let export_resp: super::SessionExportResponse =
                    serde_json::from_str(fut.await.unwrap().0.get()).unwrap();
                assert!(export_resp.events.is_empty());
            })
            .await;
    }

    #[tokio::test]
    async fn session_get_returns_error_for_unknown_no_store() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                let json = r#"{"session_id":"no-such-session"}"#;
                let req = acp::ExtRequest::new("_session/get", params_json(json));
                let fut = super::dispatch(&agent, &req).unwrap();
                assert!(fut.await.is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn session_get_returns_data_for_in_memory_session() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let sid = resp.session_id.to_string();

                let json = format!(r#"{{"session_id":"{sid}"}}"#);
                let req = acp::ExtRequest::new("_session/get", params_json(&json));
                let fut = super::dispatch(&agent, &req).unwrap();
                let get_resp: super::SessionGetResponse =
                    serde_json::from_str(fut.await.unwrap().0.get()).unwrap();
                assert_eq!(get_resp.session_id, sid);
                assert!(!get_resp.busy);
            })
            .await;
    }

    #[tokio::test]
    async fn validate_session_id_rejects_long() {
        let id = "a".repeat(super::MAX_SESSION_ID_LEN + 1);
        assert!(super::validate_session_id(&id).is_err());
    }

    #[tokio::test]
    async fn validate_session_id_rejects_control_chars() {
        assert!(super::validate_session_id("abc\x00def").is_err());
        assert!(super::validate_session_id("abc def").is_err());
        assert!(super::validate_session_id("abc/def").is_err());
    }

    #[tokio::test]
    async fn validate_session_id_accepts_valid() {
        assert!(super::validate_session_id("abc-123_XYZ").is_ok());
        assert!(super::validate_session_id("550e8400-e29b-41d4-a716-446655440000").is_ok());
    }

    // ── Error path: malformed params ──────────────────────────────────────────

    #[tokio::test]
    async fn dispatch_session_get_rejects_malformed_params() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                // Missing required `session_id` field.
                let req = acp::ExtRequest::new("_session/get", null_params());
                let fut = super::dispatch(&agent, &req).unwrap();
                assert!(fut.await.is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn dispatch_session_delete_rejects_malformed_params() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                let req = acp::ExtRequest::new("_session/delete", null_params());
                let fut = super::dispatch(&agent, &req).unwrap();
                assert!(fut.await.is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn dispatch_session_export_rejects_malformed_params() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                let req = acp::ExtRequest::new("_session/export", null_params());
                let fut = super::dispatch(&agent, &req).unwrap();
                assert!(fut.await.is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn dispatch_working_dir_update_rejects_malformed_params() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                let req = acp::ExtRequest::new("_agent/working_dir/update", null_params());
                let fut = super::dispatch(&agent, &req).unwrap();
                assert!(fut.await.is_err());
            })
            .await;
    }

    // ── Error path: invalid session_id in handlers ────────────────────────────

    #[tokio::test]
    async fn session_get_rejects_invalid_session_id() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                // session_id with slash — invalid character.
                let json = r#"{"session_id":"invalid/id"}"#;
                let req = acp::ExtRequest::new("_session/get", params_json(json));
                let fut = super::dispatch(&agent, &req).unwrap();
                assert!(fut.await.is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn session_export_rejects_invalid_session_id() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                let json = r#"{"session_id":"bad id with space"}"#;
                let req = acp::ExtRequest::new("_session/export", params_json(json));
                let fut = super::dispatch(&agent, &req).unwrap();
                assert!(fut.await.is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn session_delete_rejects_invalid_session_id() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                // session_id with slash — not in allowed charset.
                let json = r#"{"session_id":"bad/session/id"}"#;
                let req = acp::ExtRequest::new("_session/delete", params_json(json));
                let fut = super::dispatch(&agent, &req).unwrap();
                assert!(fut.await.is_err());
            })
            .await;
    }

    // ── Edge case: import with zero events ────────────────────────────────────

    #[tokio::test]
    async fn session_import_zero_events_returns_new_id() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                let json = r#"{"events":[]}"#;
                let req = acp::ExtRequest::new("_session/import", params_json(json));
                let fut = super::dispatch(&agent, &req).unwrap();
                let resp: super::SessionImportResponse =
                    serde_json::from_str(fut.await.unwrap().0.get()).unwrap();
                // New UUID must be non-empty and a valid UUID.
                assert_eq!(resp.session_id.len(), 36);
                assert!(resp.session_id.contains('-'));
            })
            .await;
    }

    // ── Integration: ext_method through Agent trait ───────────────────────────

    #[tokio::test]
    async fn ext_method_unknown_returns_null_response() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let req = acp::ExtRequest::new("unknown/custom/method", null_params());
                let resp = agent.ext_method(req).await.unwrap();
                // Default response for unknown method is JSON null.
                assert_eq!(resp.0.get(), "null");
            })
            .await;
    }

    #[tokio::test]
    async fn ext_method_session_list_via_agent_trait() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                // Create a session first.
                let new_resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let sid = new_resp.session_id.to_string();

                // Call _session/list through the Agent trait (not dispatch directly).
                let req = acp::ExtRequest::new("_session/list", null_params());
                let ext_resp = agent.ext_method(req).await.unwrap();
                let list: super::SessionListResponse =
                    serde_json::from_str(ext_resp.0.get()).unwrap();
                assert!(list.sessions.iter().any(|s| s.session_id == sid));
            })
            .await;
    }

    #[tokio::test]
    async fn ext_method_working_dir_path_traversal_via_agent_trait() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (agent, _rx) = make_agent();
                use acp::Agent as _;
                let resp = agent
                    .new_session(acp::NewSessionRequest::new(std::path::PathBuf::from(".")))
                    .await
                    .unwrap();
                let sid = resp.session_id.to_string();

                let json = format!(r#"{{"session_id":"{sid}","path":"../../../etc"}}"#);
                let req = acp::ExtRequest::new("_agent/working_dir/update", params_json(&json));
                // Must return an error through the full Agent trait path.
                assert!(agent.ext_method(req).await.is_err());
            })
            .await;
    }
}
