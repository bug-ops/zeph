// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Shared fixtures for sub-agent acceptance tests.

use std::pin::Pin;
use std::sync::Arc;

use zeph_tools::ToolCall;
use zeph_tools::executor::{ErasedToolExecutor, ToolError, ToolOutput};
use zeph_tools::registry::{InvocationHint, ToolDef};

type ToolFuture<'a> =
    Pin<Box<dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a>>;

/// Executor exposing a fixed set of tool ids; every call succeeds.
pub struct StubExecutor {
    tools: Vec<&'static str>,
}

impl ErasedToolExecutor for StubExecutor {
    fn execute_erased<'a>(&'a self, _response: &'a str) -> ToolFuture<'a> {
        Box::pin(std::future::ready(Ok(None)))
    }

    fn execute_confirmed_erased<'a>(&'a self, _response: &'a str) -> ToolFuture<'a> {
        Box::pin(std::future::ready(Ok(None)))
    }

    fn tool_definitions_erased(&self) -> Vec<ToolDef> {
        self.tools
            .iter()
            .map(|id| ToolDef {
                id: (*id).into(),
                description: "stub".into(),
                schema: schemars::Schema::default(),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
                server_id: None,
            })
            .collect()
    }

    fn execute_tool_call_erased<'a>(&'a self, call: &'a ToolCall) -> ToolFuture<'a> {
        Box::pin(std::future::ready(Ok(Some(ToolOutput {
            tool_name: call.tool_id.clone(),
            summary: "ok".into(),
            blocks_executed: 1,
            ..Default::default()
        }))))
    }

    fn is_tool_retryable_erased(&self, _tool_id: &str) -> bool {
        false
    }

    fn requires_confirmation_erased(&self, _call: &ToolCall) -> bool {
        false
    }

    fn execute_tool_call_confirmed_erased<'a>(&'a self, call: &'a ToolCall) -> ToolFuture<'a> {
        self.execute_tool_call_erased(call)
    }

    fn checkpoint_undo_erased(&self, _n: usize) -> zeph_tools::CheckpointActionResult {
        zeph_tools::CheckpointActionResult::unsupported()
    }

    fn checkpoint_redo_erased(&self) -> zeph_tools::CheckpointActionResult {
        zeph_tools::CheckpointActionResult::unsupported()
    }

    fn checkpoint_list_erased(&self) -> zeph_tools::CheckpointListResult {
        zeph_tools::CheckpointListResult::default()
    }

    fn is_tool_speculatable_erased(&self, _tool_id: &str) -> bool {
        false
    }
}

/// Stub executor exposing exactly `tools`.
pub fn stub_box(tools: &[&'static str]) -> Arc<dyn ErasedToolExecutor> {
    Arc::new(StubExecutor {
        tools: tools.to_vec(),
    })
}

/// Structured tool call for `tool_id` with no params.
pub fn call(tool_id: &str) -> ToolCall {
    ToolCall {
        tool_id: tool_id.into(),
        params: serde_json::Map::default(),
        attribution: zeph_tools::CallAttribution::default(),
        context: None,
        tool_call_id: None,
    }
}
