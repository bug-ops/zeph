// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Cross-crate sink trait for LLM request/response debug-dump instrumentation.
//!
//! `zeph-core` owns the concrete debug-dump writer (`DebugDumper`) and the top-level
//! agent loop's `--debug-dump` wiring, but `zeph-subagent` cannot depend on `zeph-core`
//! (the dependency runs the other way). [`DebugDumpSink`] is the minimal contract that
//! lets `zeph-core` hand a dump-writer handle down into `zeph-subagent`'s agent loop —
//! via `SpawnContext`/`AgentLoopArgs` — so sub-agent LLM calls are captured through the
//! same pipeline as top-level calls (#6391).

use crate::provider::{ChatResponse, Message, ToolDefinition};

/// Receives LLM request/response pairs for debug-dump instrumentation.
///
/// Implemented by `zeph-core`'s `DebugDumper`. Callers pair each [`dump_request`] with a
/// [`dump_response`] using the returned id.
///
/// [`dump_request`]: DebugDumpSink::dump_request
/// [`dump_response`]: DebugDumpSink::dump_response
pub trait DebugDumpSink: Send + Sync {
    /// Returns `true` when the active dump format does not need `provider_request` built
    /// (e.g. Trace format, which records spans instead of numbered files) — callers can
    /// skip the (non-free) request serialization in that case.
    fn is_trace_format(&self) -> bool;

    /// Dump the outgoing request. Returns an id that must be passed to [`dump_response`]
    /// to correlate the pair.
    ///
    /// [`dump_response`]: DebugDumpSink::dump_response
    fn dump_request(
        &self,
        model_name: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
        provider_request: serde_json::Value,
    ) -> u32;

    /// Dump the response paired with a prior [`dump_request`] call.
    ///
    /// [`dump_request`]: DebugDumpSink::dump_request
    fn dump_response(&self, id: u32, response: &ChatResponse);
}
