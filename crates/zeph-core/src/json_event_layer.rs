// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `JsonEventLayer`: a [`crate::runtime_layer::RuntimeLayer`] that emits tool events via [`JsonEventSink`].
//!
//! Install this layer on the agent when `--json` is active. It is the *canonical*
//! emitter for `tool_call` and `tool_result` events — `JsonCliChannel` intentionally
//! no-ops its corresponding channel methods to avoid double-emission.
//!
//! All tool arguments and outputs pass through [`crate::redact::scrub_content`] before
//! emission so secrets (API keys, bearer tokens, passwords) are not written to the JSONL
//! stream.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use zeph_tools::ToolError;
use zeph_tools::executor::{ToolCall, ToolOutput};

use crate::json_event_sink::{JsonEvent, JsonEventSink};
use crate::runtime_layer::{BeforeToolResult, LayerContext, RuntimeLayer};

/// `RuntimeLayer` that forwards tool events to a [`JsonEventSink`].
pub struct JsonEventLayer {
    sink: Arc<JsonEventSink>,
}

impl JsonEventLayer {
    /// Create a new layer sharing `sink` with `JsonCliChannel`.
    #[must_use]
    pub fn new(sink: Arc<JsonEventSink>) -> Self {
        Self { sink }
    }
}

impl RuntimeLayer for JsonEventLayer {
    fn before_tool<'a>(
        &'a self,
        _ctx: &'a LayerContext<'_>,
        call: &'a ToolCall,
    ) -> Pin<Box<dyn Future<Output = BeforeToolResult> + Send + 'a>> {
        // Serialize args, scrub secrets, then re-parse so the sink receives a clean Value.
        let raw = serde_json::Value::Object(call.params.clone());
        let raw_str = raw.to_string();
        let scrubbed_str = crate::redact::scrub_content(&raw_str);
        let args_value: serde_json::Value =
            serde_json::from_str(&scrubbed_str).unwrap_or(serde_json::Value::Null);
        self.sink.emit(&JsonEvent::ToolCall {
            tool: call.tool_id.as_ref(),
            args: &args_value,
            id: call.tool_id.as_ref(),
        });
        Box::pin(std::future::ready(None))
    }

    fn after_tool<'a>(
        &'a self,
        _ctx: &'a LayerContext<'_>,
        call: &'a ToolCall,
        result: &'a Result<Option<ToolOutput>, ToolError>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        let err_str;
        let scrubbed_err;
        let scrubbed_out;
        let (output, is_error) = match result {
            Ok(Some(out)) => {
                scrubbed_out = crate::redact::scrub_content(&out.summary);
                (scrubbed_out.as_ref(), false)
            }
            Ok(None) => ("", false),
            Err(e) => {
                err_str = e.to_string();
                scrubbed_err = crate::redact::scrub_content(&err_str);
                (scrubbed_err.as_ref(), true)
            }
        };
        self.sink.emit(&JsonEvent::ToolResult {
            tool: call.tool_id.as_ref(),
            id: call.tool_id.as_ref(),
            output,
            is_error,
        });
        Box::pin(std::future::ready(()))
    }
}
