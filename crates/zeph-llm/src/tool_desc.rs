// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared helper for building MCP tool description strings with optional
//! output-schema hints.
//!
//! Both the Claude and `OpenAI` backends use the same logic for appending a
//! compact JSON schema hint to a tool's description field before sending it
//! to the LLM. This module houses the single implementation so neither
//! backend carries a duplicate.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

/// Build the tool description string, optionally appending an output schema hint.
///
/// When `forward` is `true` and `output_schema` is `Some`, appends a compact JSON hint
/// capped at `hint_bytes`. If the schema exceeds the budget, a stub is used and a WARN
/// is emitted once per session per tool (guarded by a global `OnceLock<Mutex<HashSet>>`).
///
/// # Arguments
///
/// - `base` — the tool's original description text.
/// - `output_schema` — optional MCP output schema to forward.
/// - `forward` — whether schema forwarding is enabled for this provider.
/// - `hint_bytes` — maximum byte length of the inlined schema JSON.
/// - `max_combined_bytes` — overall cap on the returned string; pass
///   `usize::MAX` to disable.
/// - `tool_name` — used only in warning/debug log fields.
///
/// # Note
///
/// This is a `pub(crate)` function — it is not part of the public API and cannot
/// be called from outside the `zeph-llm` crate.
pub(crate) fn build_tool_description(
    base: &str,
    output_schema: Option<&serde_json::Value>,
    forward: bool,
    hint_bytes: usize,
    max_combined_bytes: usize,
    tool_name: &str,
) -> String {
    if !forward {
        return base.to_owned();
    }
    let Some(schema) = output_schema else {
        return base.to_owned();
    };
    let compact = serde_json::to_string(schema).unwrap_or_default();
    let hint = if compact.len() > hint_bytes {
        static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
        let guard = WARNED.get_or_init(|| Mutex::new(HashSet::new()));
        let mut warned = guard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if warned.insert(tool_name.to_owned()) {
            tracing::warn!(
                tool = tool_name,
                schema_bytes = compact.len(),
                cap = hint_bytes,
                event = "mcp.output_schema.stub_used",
                "MCP output_schema hint exceeds budget — using stub"
            );
        }
        format!(
            "Output schema too large ({} bytes); details omitted.",
            compact.len()
        )
    } else {
        tracing::debug!(
            tool = tool_name,
            event = "mcp.output_schema.forwarded_to_llm",
            "MCP tool output schema forwarded to LLM description"
        );
        compact
    };
    let combined = format!("{base}\n\nExpected output schema (JSON):\n{hint}");
    if max_combined_bytes < usize::MAX && combined.len() > max_combined_bytes {
        zeph_common::text::truncate_to_bytes(&combined, max_combined_bytes).clone()
    } else {
        combined
    }
}
