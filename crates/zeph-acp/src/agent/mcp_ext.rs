// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `_agent/mcp/*` ext-method handlers for ACP sessions.
//!
//! Groups the `_agent/mcp/list`, `_agent/mcp/add`, and `_agent/mcp/remove` handlers so the
//! MCP server-management surface is isolated from the main agent dispatch logic in
//! [`super`].

use agent_client_protocol as acp;
use zeph_mcp::manager::ServerEntry;

use super::ZephAcpAgentState;

#[derive(serde::Deserialize)]
struct McpRemoveParams {
    id: String,
}

impl ZephAcpAgentState {
    pub(crate) async fn ext_method_mcp(
        &self,
        args: &acp::schema::v1::ExtRequest,
    ) -> acp::Result<acp::schema::v1::ExtResponse> {
        let method = args.method.as_ref();
        match method {
            "_agent/mcp/list" => {
                let Some(ref manager) = self.mcp_manager else {
                    return Err(acp::Error::internal_error().data("MCP manager not configured"));
                };
                let servers = manager.list_servers().await;
                let json = serde_json::to_string(&servers).map_err(|e| {
                    tracing::error!(error = %e, "failed to serialize MCP server list");
                    acp::Error::internal_error().data("internal error")
                })?;
                let raw: Box<serde_json::value::RawValue> =
                    serde_json::value::RawValue::from_string(json).map_err(|e| {
                        tracing::error!(error = %e, "failed to build MCP list response");
                        acp::Error::internal_error().data("internal error")
                    })?;
                Ok(acp::schema::v1::ExtResponse::new(raw.into()))
            }
            "_agent/mcp/add" => {
                let Some(ref manager) = self.mcp_manager else {
                    return Err(acp::Error::internal_error().data("MCP manager not configured"));
                };
                let entry: ServerEntry = serde_json::from_str(args.params.get())
                    .map_err(|e| acp::Error::invalid_request().data(e.to_string()))?;
                let tools = manager.add_server(&entry).await.map_err(|e| {
                    tracing::error!(error = %e, "failed to add MCP server");
                    acp::Error::internal_error().data("internal error")
                })?;
                let json = serde_json::json!({ "added": entry.id, "tools": tools.len() });
                let raw =
                    serde_json::value::RawValue::from_string(json.to_string()).map_err(|e| {
                        tracing::error!(error = %e, "failed to build MCP add response");
                        acp::Error::internal_error().data("internal error")
                    })?;
                Ok(acp::schema::v1::ExtResponse::new(raw.into()))
            }
            "_agent/mcp/remove" => {
                let Some(ref manager) = self.mcp_manager else {
                    return Err(acp::Error::internal_error().data("MCP manager not configured"));
                };
                let params: McpRemoveParams = serde_json::from_str(args.params.get())
                    .map_err(|e| acp::Error::invalid_request().data(e.to_string()))?;
                manager.remove_server(&params.id).await.map_err(|e| {
                    tracing::error!(error = %e, "failed to remove MCP server");
                    acp::Error::internal_error().data("internal error")
                })?;
                let raw = serde_json::value::RawValue::from_string(
                    serde_json::json!({ "removed": params.id }).to_string(),
                )
                .map_err(|e| {
                    tracing::error!(error = %e, "failed to build MCP remove response");
                    acp::Error::internal_error().data("internal error")
                })?;
                Ok(acp::schema::v1::ExtResponse::new(raw.into()))
            }
            _ => Ok(acp::schema::v1::ExtResponse::new(
                serde_json::value::RawValue::NULL.to_owned().into(),
            )),
        }
    }
}
