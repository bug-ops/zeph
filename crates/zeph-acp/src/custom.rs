// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use agent_client_protocol as acp;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use zeph_common::SessionId;

use crate::agent::ZephAcpAgent;

// ── Constants ─────────────────────────────────────────────────────────────────

const MAX_IMPORT_EVENTS: usize = 10_000;
const MAX_SESSION_ID_LEN: usize = 128;

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct SessionListParams {}

#[derive(Serialize, Deserialize)]
pub(crate) struct SessionListEntry {
    pub session_id: SessionId,
    pub created_at: String,
    pub busy: bool,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SessionListResponse {
    pub sessions: Vec<SessionListEntry>,
}

#[derive(Deserialize)]
pub(crate) struct SessionGetParams {
    pub session_id: SessionId,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SessionEventEntry {
    pub event_type: String,
    pub payload: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SessionGetResponse {
    pub session_id: SessionId,
    pub created_at: String,
    pub busy: bool,
    pub events: Vec<SessionEventEntry>,
}

#[derive(Deserialize)]
pub(crate) struct SessionDeleteParams {
    pub session_id: SessionId,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SessionDeleteResponse {
    pub deleted: bool,
}

#[derive(Deserialize)]
pub(crate) struct SessionExportParams {
    pub session_id: SessionId,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SessionExportResponse {
    pub session_id: SessionId,
    pub events: Vec<SessionEventEntry>,
    pub exported_at: String,
}

#[derive(Deserialize)]
pub(crate) struct SessionImportParams {
    pub events: Vec<SessionEventEntry>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SessionImportResponse {
    pub session_id: SessionId,
}

#[derive(Deserialize)]
pub(crate) struct AgentToolsParams {
    #[expect(
        dead_code,
        reason = "required for JSON deserialization of the ACP ext method params"
    )]
    pub session_id: SessionId,
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
    pub session_id: SessionId,
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
    req: &'a acp::schema::v1::ExtRequest,
) -> Option<
    Pin<
        Box<
            dyn std::future::Future<Output = acp::Result<acp::schema::v1::ExtResponse>> + Send + 'a,
        >,
    >,
> {
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

fn to_ext_response<T: Serialize>(value: &T) -> acp::Result<acp::schema::v1::ExtResponse> {
    let json = serde_json::to_string(value)
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
    let raw = RawValue::from_string(json)
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
    Ok(acp::schema::v1::ExtResponse::new(Arc::from(raw)))
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
) -> acp::Result<acp::schema::v1::ExtResponse> {
    // Deprecated: use the native `list_sessions` ACP method instead.
    // This extension method returns a reduced `SessionListEntry` schema and will be removed
    // in a future release.
    tracing::warn!(
        "ext method `_session/list` is deprecated; use the native `list_sessions` ACP method"
    );
    let _: SessionListParams = parse_params(raw)?;

    // Collect in-memory session tuples while holding the borrow, then release it.
    let in_memory: Vec<(String, String, bool)> = {
        let sessions = agent.sessions.lock();
        let mut tuples = Vec::with_capacity(sessions.len());
        for (id, entry) in sessions.iter() {
            let sid = id.to_string();
            let busy = entry.output_rx.lock().is_none();
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
                    let sid = String::from(&*row.id);
                    sessions.insert(
                        sid.clone(),
                        SessionListEntry {
                            session_id: SessionId::new(sid),
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
                session_id: SessionId::new(sid),
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
) -> acp::Result<acp::schema::v1::ExtResponse> {
    let params: SessionGetParams = parse_params(raw)?;
    let sid = params.session_id.as_str();
    validate_session_id(sid)?;

    let (in_memory, created_at, busy) = {
        let sessions = agent.sessions.lock();
        if let Some(entry) = sessions.get(&acp::schema::v1::SessionId::new(sid)) {
            let busy = entry.output_rx.lock().is_none();
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
        session_id: SessionId::new(sid),
        created_at,
        busy,
        events,
    };
    to_ext_response(&resp)
}

async fn handle_session_delete(
    agent: &ZephAcpAgent,
    raw: &Arc<RawValue>,
) -> acp::Result<acp::schema::v1::ExtResponse> {
    tracing::warn!("_session/delete is deprecated, use session/delete instead");
    let params: SessionDeleteParams = parse_params(raw)?;
    validate_session_id(&params.session_id)?;

    let acp_id = acp::schema::v1::SessionId::new(params.session_id.as_str());
    let removed_memory = agent.sessions.lock().remove(&acp_id).is_some();
    if removed_memory {
        // cancel_signal already dropped with the entry; nothing extra needed.
        tracing::debug!(session_id = %params.session_id, "removed in-memory ACP session");
    }

    let removed_store = if let Some(ref store) = agent.store {
        match store.delete_acp_session_checked(&params.session_id).await {
            Ok(existed) => existed,
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
) -> acp::Result<acp::schema::v1::ExtResponse> {
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
) -> acp::Result<acp::schema::v1::ExtResponse> {
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

    to_ext_response(&SessionImportResponse {
        session_id: SessionId::new(new_id),
    })
}

#[allow(clippy::unused_async)]
async fn handle_agent_tools(
    _agent: &ZephAcpAgent,
    raw: &Arc<RawValue>,
) -> acp::Result<acp::schema::v1::ExtResponse> {
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
) -> acp::Result<acp::schema::v1::ExtResponse> {
    let params: WorkingDirUpdateParams = parse_params(raw)?;
    validate_session_id(&params.session_id)?;

    // Reject path traversal: disallow any ParentDir (..) component.
    let p = std::path::Path::new(&params.path);
    if p.components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(acp::Error::invalid_request().data("path traversal not allowed"));
    }

    let acp_id = acp::schema::v1::SessionId::new(params.session_id.as_str());
    let updated = {
        let sessions = agent.sessions.lock();
        if let Some(entry) = sessions.get(&acp_id) {
            *entry.working_dir.lock() = Some(PathBuf::from(&params.path));
            true
        } else {
            false
        }
    };

    to_ext_response(&WorkingDirUpdateResponse { updated })
}
