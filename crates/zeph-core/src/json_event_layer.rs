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
            id: call
                .tool_call_id
                .as_ref()
                .map(zeph_common::ToolExecutionId::as_str),
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
                (scrubbed_out.as_ref(), out.status.is_failure())
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
            id: call
                .tool_call_id
                .as_ref()
                .map(zeph_common::ToolExecutionId::as_str),
            output,
            is_error,
        });
        Box::pin(std::future::ready(()))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use crate::runtime_layer::LayerContext;

    use super::*;

    /// `Write` impl backed by a shared buffer so the test can read back what `JsonEventSink`
    /// wrote after it takes ownership of the writer.
    #[derive(Clone)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn make_call(tool_id: &str, tool_call_id: &str) -> ToolCall {
        ToolCall {
            tool_id: zeph_common::ToolName::new(tool_id),
            tool_call_id: Some(zeph_common::ToolExecutionId::new(tool_call_id).unwrap()),
            ..Default::default()
        }
    }

    fn emitted_ids(buf: &[u8], event_name: &str) -> Vec<String> {
        String::from_utf8_lossy(buf)
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|v| v["event"] == event_name)
            .map(|v| v["id"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    /// Regression for #5680: `before_tool`/`after_tool` must emit `tool_call_id` (unique per
    /// invocation) as the JSON event `id`, not `tool_id` (the tool's name, shared across every
    /// call to the same tool in a turn). Two calls to the same tool with distinct
    /// `tool_call_id`s must produce distinct `id` values in the emitted events.
    #[tokio::test]
    async fn before_and_after_tool_emit_distinct_ids_for_same_tool() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(JsonEventSink::with_writer(SharedBuffer(buf.clone())));
        let layer = JsonEventLayer::new(sink);
        let ctx = LayerContext {
            conversation_id: None,
            turn_number: 0,
        };

        let call_a = make_call("shell", "call-a");
        let call_b = make_call("shell", "call-b");
        let ok_result: Result<Option<ToolOutput>, ToolError> = Ok(None);

        layer.before_tool(&ctx, &call_a).await;
        layer.before_tool(&ctx, &call_b).await;
        layer.after_tool(&ctx, &call_a, &ok_result).await;
        layer.after_tool(&ctx, &call_b, &ok_result).await;

        let snapshot = buf.lock().unwrap().clone();
        let call_ids = emitted_ids(&snapshot, "tool_call");
        let result_ids = emitted_ids(&snapshot, "tool_result");

        assert_eq!(call_ids, vec!["call-a", "call-b"]);
        assert_eq!(result_ids, vec!["call-a", "call-b"]);
    }

    #[tokio::test]
    async fn after_tool_is_error_follows_status_is_failure() {
        use zeph_tools::ToolStatus;
        let cases = [
            (ToolStatus::Success, false),
            (ToolStatus::from_exit_code(3), true),
            (ToolStatus::Failed, true),
        ];
        for (status, expect_error) in cases {
            let buf = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::new(JsonEventSink::with_writer(SharedBuffer(buf.clone())));
            let layer = JsonEventLayer::new(sink);
            let ctx = LayerContext {
                conversation_id: None,
                turn_number: 0,
            };
            let call = make_call("shell", "call-a");
            let result: Result<Option<ToolOutput>, ToolError> = Ok(Some(ToolOutput {
                summary: "out".into(),
                status,
                ..Default::default()
            }));
            layer.after_tool(&ctx, &call, &result).await;
            let snapshot = buf.lock().unwrap().clone();
            let line = String::from_utf8_lossy(&snapshot)
                .lines()
                .next()
                .unwrap()
                .to_owned();
            let event: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(event["is_error"], expect_error, "{status:?}");
        }
    }

    #[tokio::test]
    async fn before_tool_redacts_json_escaped_secret_and_keeps_json_valid() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(JsonEventSink::with_writer(SharedBuffer(buf.clone())));
        let layer = JsonEventLayer::new(sink);
        let ctx = LayerContext {
            conversation_id: None,
            turn_number: 0,
        };
        let mut call = make_call("shell", "call-a");
        call.params.insert(
            "cmd".to_owned(),
            serde_json::json!("echo a\nsk-abc123def456 disk-usage"),
        );

        layer.before_tool(&ctx, &call).await;

        let snapshot = buf.lock().unwrap().clone();
        let line = String::from_utf8_lossy(&snapshot)
            .lines()
            .next()
            .unwrap()
            .to_owned();
        let event: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(event["args"]["cmd"], "echo a\n[REDACTED] disk-usage");
    }

    #[tokio::test]
    async fn missing_tool_call_id_emits_null_id() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(JsonEventSink::with_writer(SharedBuffer(buf.clone())));
        let layer = JsonEventLayer::new(sink);
        let ctx = LayerContext {
            conversation_id: None,
            turn_number: 0,
        };
        let call = ToolCall {
            tool_id: zeph_common::ToolName::new("shell"),
            ..Default::default()
        };
        let ok_result: Result<Option<ToolOutput>, ToolError> = Ok(None);

        layer.before_tool(&ctx, &call).await;
        layer.after_tool(&ctx, &call, &ok_result).await;

        let snapshot = buf.lock().unwrap().clone();
        let events: Vec<serde_json::Value> = String::from_utf8_lossy(&snapshot)
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .filter(|v: &serde_json::Value| {
                v["event"] == "tool_call" || v["event"] == "tool_result"
            })
            .collect();
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|v| v["id"].is_null()));
    }
}
