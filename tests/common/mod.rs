// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared fixtures for root integration tests.

use zeph_tools::registry::{InvocationHint, ToolDef};

/// Definitions for the single `bash` tool the mock providers call.
///
/// The native tool loop only accepts calls to tools the executor advertises, so test
/// executors that handle `bash` must return this from `tool_definitions`.
pub fn bash_tool_defs() -> Vec<ToolDef> {
    vec![ToolDef {
        id: "bash".into(),
        description: "test tool".into(),
        schema: serde_json::from_value(serde_json::Value::Bool(true))
            .expect("`true` is a valid JSON Schema"),
        invocation: InvocationHint::ToolCall,
        output_schema: None,
        server_id: None,
    }]
}
