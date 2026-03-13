// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Test stub for [`McpCaller`].
//!
//! Enabled via the `mock` feature flag. Provides [`MockMcpCaller`] — an
//! in-memory stub that captures [`McpCaller::call_tool`] invocations and
//! returns pre-configured results, allowing unit tests to verify that
//! callers pass correct argument keys without a live MCP server.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use rmcp::model::{CallToolResult, Content};

use crate::caller::McpCaller;
use crate::error::McpError;

/// A recorded invocation of [`McpCaller::call_tool`].
#[derive(Debug, Clone)]
pub struct McpCall {
    pub server_id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
}

/// Configurable stub that implements [`McpCaller`] for unit tests.
///
/// Captures every `call_tool` invocation in `recorded_calls` and returns
/// responses popped from the queue in FIFO order. When the queue is
/// exhausted, `call_tool` returns `McpError::ServerNotFound`.
pub struct MockMcpCaller {
    /// Every call recorded in order, accessible for test assertions.
    pub recorded_calls: Arc<Mutex<Vec<McpCall>>>,
    pending_responses: Arc<Mutex<VecDeque<Result<CallToolResult, McpError>>>>,
    /// Server IDs returned by `list_servers`. Empty by default so that
    /// `is_available()` returns `false` unless explicitly configured.
    pub server_ids: Arc<Mutex<Vec<String>>>,
}

impl MockMcpCaller {
    #[must_use]
    pub fn new() -> Self {
        Self {
            recorded_calls: Arc::new(Mutex::new(Vec::new())),
            pending_responses: Arc::new(Mutex::new(VecDeque::new())),
            server_ids: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Register a server ID returned by `list_servers`.
    #[must_use]
    pub fn with_server(self, id: impl Into<String>) -> Self {
        self.server_ids.lock().unwrap().push(id.into());
        self
    }

    /// Queue a successful result with a single text content item.
    #[must_use]
    pub fn with_text_response(self, text: impl Into<String>) -> Self {
        let result = CallToolResult::success(vec![Content::text(text.into())]);
        self.pending_responses.lock().unwrap().push_back(Ok(result));
        self
    }

    /// Queue an error response.
    #[must_use]
    pub fn with_error_response(self, server_id: impl Into<String>) -> Self {
        self.pending_responses
            .lock()
            .unwrap()
            .push_back(Err(McpError::ServerNotFound {
                server_id: server_id.into(),
            }));
        self
    }
}

impl Default for MockMcpCaller {
    fn default() -> Self {
        Self::new()
    }
}

impl McpCaller for MockMcpCaller {
    async fn call_tool(
        &self,
        server_id: &str,
        tool_name: &str,
        args: serde_json::Value,
    ) -> Result<CallToolResult, McpError> {
        self.recorded_calls.lock().unwrap().push(McpCall {
            server_id: server_id.to_owned(),
            tool_name: tool_name.to_owned(),
            args,
        });

        let mut queue = self.pending_responses.lock().unwrap();
        queue.pop_front().unwrap_or_else(|| {
            Err(McpError::ServerNotFound {
                server_id: server_id.to_owned(),
            })
        })
    }

    async fn list_servers(&self) -> Vec<String> {
        self.server_ids.lock().unwrap().clone()
    }
}
