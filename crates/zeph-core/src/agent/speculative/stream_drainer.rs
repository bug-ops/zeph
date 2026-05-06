// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Decoding-level speculative dispatch: `SpeculativeStreamDrainer`.
//!
//! Consumes a [`ToolSseStream`] from the Claude SSE tool-use path, fires speculative
//! dispatches via [`SpeculationEngine::try_dispatch`] as soon as all required JSON fields
//! are present in the partial input buffer, and assembles a complete [`ChatResponse`] at
//! stream end — including thinking blocks.
//!
//! Mode gate: only active when `speculative.mode` is `Decoding` or `Both`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio_stream::StreamExt;
use tracing::debug;
use zeph_llm::provider::{ChatResponse, ThinkingBlock, ToolUseRequest};
use zeph_llm::sse::{ToolSseEvent, ToolSseStream};

use super::SpeculationEngine;
use super::partial_json::{PartialJsonParser, PrefixState};
use super::prediction::{Prediction, PredictionSource};

/// Drives a `ToolSseStream`, intercepts `InputJsonDelta` events for speculative dispatch,
/// and reassembles a `ChatResponse` for the normal tool-dispatch path.
///
/// `ToolBlockStart` events populate tool metadata before any `InputJsonDelta` arrives,
/// ensuring speculative dispatch can fire incrementally during streaming.
pub struct SpeculativeStreamDrainer {
    stream: ToolSseStream,
    engine: Arc<SpeculationEngine>,
    confidence_threshold: f32,
    /// Per-tool-index partial JSON parser.
    parsers: HashMap<usize, PartialJsonParser>,
}

impl SpeculativeStreamDrainer {
    /// Create a new drainer.
    ///
    /// `confidence_threshold` gates speculation. Trust level is enforced internally
    /// by `engine.try_dispatch` — no separate parameter is needed here.
    #[must_use]
    pub fn new(
        stream: ToolSseStream,
        engine: Arc<SpeculationEngine>,
        confidence_threshold: f32,
    ) -> Self {
        Self {
            stream,
            engine,
            confidence_threshold,
            parsers: HashMap::new(),
        }
    }

    /// Drain the stream, firing speculative dispatches and building the final `ChatResponse`.
    ///
    /// Returns the assembled `ChatResponse` — identical contract to
    /// `LlmProvider::chat_with_tools`. On stream error, returns the first error encountered.
    ///
    /// # Errors
    ///
    /// Returns an `LlmError` if the SSE stream signals a parse or API error.
    pub async fn drive(mut self) -> Result<ChatResponse, zeph_llm::LlmError> {
        let mut tool_calls: Vec<ToolUseRequest> = Vec::new();
        let mut thinking_blocks: Vec<ThinkingBlock> = Vec::new();
        let mut text_buf = String::new();
        // Map from tool index → (id, name) collected from ToolCallComplete.
        let mut tool_meta: HashMap<usize, (String, String)> = HashMap::new();
        // Track which tool indices have had their speculation fired.
        let mut dispatched: std::collections::HashSet<usize> = std::collections::HashSet::new();

        while let Some(event) = self.stream.next().await {
            match event {
                ToolSseEvent::ToolBlockStart { index, id, name } => {
                    // Populate tool_meta immediately at block open so InputJsonDelta handlers
                    // can look up id+name before content_block_stop fires ToolCallComplete.
                    tool_meta.insert(index, (id, name));
                }
                ToolSseEvent::InputJsonDelta { index, delta } => {
                    let parser = self.parsers.entry(index).or_default();
                    if let PrefixState::ValidPrefix {
                        known_leaves,
                        missing_required,
                    } = parser.push(&delta)
                        && missing_required.is_empty()
                        && !dispatched.contains(&index)
                        && let Some((_llm_id, name)) = tool_meta.get(&index)
                    {
                        let pred = Prediction {
                            tool_id: name.as_str().into(),
                            args: known_leaves,
                            confidence: self.confidence_threshold,
                            source: PredictionSource::StreamPartial,
                        };
                        if self
                            .engine
                            .try_dispatch(&pred, zeph_common::SkillTrustLevel::Trusted)
                        {
                            dispatched.insert(index);
                            debug!(tool = %name, index, "speculative dispatch fired from SSE delta");
                        }
                    }
                }
                ToolSseEvent::ToolCallComplete {
                    index,
                    id,
                    name,
                    full_json,
                } => {
                    // tool_meta already populated by ToolBlockStart; update in case of re-order.
                    tool_meta.insert(index, (id.clone(), name.clone()));

                    let input = serde_json::from_str(&full_json)
                        .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));

                    tool_calls.push(ToolUseRequest {
                        id,
                        name: name.into(),
                        input,
                    });
                }
                ToolSseEvent::ThinkingBlockDone(block) => {
                    thinking_blocks.push(block);
                }
                ToolSseEvent::ThinkingChunk(_) => {
                    // Pass-through; full block assembled via ThinkingBlockDone.
                }
                ToolSseEvent::ContentChunk(text) => {
                    text_buf.push_str(&text);
                }
                ToolSseEvent::Compaction(_summary) => {
                    // TODO: surface compaction summaries to the caller (follow-up).
                    tracing::debug!(
                        "compaction summary received during tool stream (not yet surfaced)"
                    );
                }
                ToolSseEvent::Error(e) => {
                    return Err(e);
                }
            }
        }

        let text = if text_buf.is_empty() {
            None
        } else {
            Some(text_buf)
        };

        if tool_calls.is_empty() {
            Ok(ChatResponse::Text(text.unwrap_or_default()))
        } else {
            Ok(ChatResponse::ToolUse {
                text,
                tool_calls,
                thinking_blocks,
            })
        }
    }
}

/// Wraps `engine.try_commit` with a 2-second timeout cap (critic M4).
///
/// The TTL deadline (up to 30 s) is too long for the tool dispatch hot path.
/// This helper caps the wait to avoid blocking normal execution unnecessarily.
pub async fn try_commit_with_timeout(
    engine: &SpeculationEngine,
    call: &zeph_tools::ToolCall,
) -> Option<Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>> {
    const COMMIT_TIMEOUT: Duration = Duration::from_secs(2);
    match tokio::time::timeout(COMMIT_TIMEOUT, engine.try_commit(call)).await {
        Ok(result) => result,
        Err(_elapsed) => {
            debug!(tool_id = %call.tool_id, "speculative try_commit timed out after 2s");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn drainer_new_has_empty_parsers() {
        use std::sync::Arc;
        use zeph_config::tools::SpeculativeConfig;
        use zeph_tools::{ToolCall, ToolError, ToolExecutor, ToolOutput};

        struct NullExec;
        impl ToolExecutor for NullExec {
            async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
                Ok(None)
            }
            async fn execute_tool_call(
                &self,
                _: &ToolCall,
            ) -> Result<Option<ToolOutput>, ToolError> {
                Ok(None)
            }
            fn is_tool_speculatable(&self, _: &str) -> bool {
                false
            }
        }
        let engine = Arc::new(super::super::SpeculationEngine::new(
            Arc::new(NullExec),
            SpeculativeConfig::default(),
        ));
        let drainer = SpeculativeStreamDrainer::new(Box::pin(tokio_stream::empty()), engine, 0.8);
        assert!(drainer.parsers.is_empty());
    }

    /// Verifies the BUG-2 fix: `ToolBlockStart` populates `tool_meta` before any
    /// `InputJsonDelta` arrives, allowing speculative dispatch to fire on the first
    /// complete-args delta rather than waiting until `ToolCallComplete` (block stop).
    #[tokio::test]
    async fn tool_block_start_enables_incremental_dispatch() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use zeph_config::tools::{SpeculationMode, SpeculativeConfig};
        use zeph_tools::{ToolCall, ToolError, ToolExecutor, ToolOutput};

        struct SpyExec {
            count: Arc<AtomicUsize>,
        }
        impl ToolExecutor for SpyExec {
            async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
                Ok(None)
            }
            async fn execute_tool_call(
                &self,
                _: &ToolCall,
            ) -> Result<Option<ToolOutput>, ToolError> {
                Ok(None)
            }
            fn is_tool_speculatable(&self, _: &str) -> bool {
                self.count.fetch_add(1, Ordering::Relaxed);
                true
            }
        }

        // Executor that records how many times is_tool_speculatable returns true.
        let dispatch_count = Arc::new(AtomicUsize::new(0));
        let dispatch_count_clone = Arc::clone(&dispatch_count);

        let config = SpeculativeConfig {
            mode: SpeculationMode::Decoding,
            ..Default::default()
        };
        let engine = Arc::new(super::super::SpeculationEngine::new(
            Arc::new(SpyExec {
                count: dispatch_count_clone,
            }),
            config,
        ));

        // Sequence: ToolBlockStart (id+name known) → InputJsonDelta (complete args) → ToolCallComplete.
        // Without the fix, tool_meta is empty when InputJsonDelta arrives → dispatch never fires.
        let events = vec![
            ToolSseEvent::ToolBlockStart {
                index: 0,
                id: "toolu_01".into(),
                name: "bash".into(),
            },
            ToolSseEvent::InputJsonDelta {
                index: 0,
                delta: r#"{"command":"ls"}"#.into(),
            },
            ToolSseEvent::ToolCallComplete {
                index: 0,
                id: "toolu_01".into(),
                name: "bash".into(),
                full_json: r#"{"command":"ls"}"#.into(),
            },
        ];

        let drainer =
            SpeculativeStreamDrainer::new(Box::pin(tokio_stream::iter(events)), engine, 0.0);
        let result = drainer.drive().await.unwrap();
        // Drainer assembled a ToolUse response.
        assert!(matches!(result, ChatResponse::ToolUse { .. }));
        // SpyExec.is_tool_speculatable was called at least once (dispatch was attempted).
        assert!(
            dispatch_count.load(Ordering::Relaxed) > 0,
            "dispatch should have been attempted"
        );
    }

    #[tokio::test]
    async fn drive_empty_stream_returns_text_empty() {
        use std::sync::Arc;
        use zeph_config::tools::SpeculativeConfig;
        use zeph_tools::{ToolCall, ToolError, ToolExecutor, ToolOutput};

        struct NullExec;
        impl ToolExecutor for NullExec {
            async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
                Ok(None)
            }
            async fn execute_tool_call(
                &self,
                _: &ToolCall,
            ) -> Result<Option<ToolOutput>, ToolError> {
                Ok(None)
            }
            fn is_tool_speculatable(&self, _: &str) -> bool {
                false
            }
        }
        let engine = Arc::new(super::super::SpeculationEngine::new(
            Arc::new(NullExec),
            SpeculativeConfig::default(),
        ));
        let drainer = SpeculativeStreamDrainer::new(Box::pin(tokio_stream::empty()), engine, 0.8);
        let result = drainer.drive().await.unwrap();
        assert!(matches!(result, ChatResponse::Text(s) if s.is_empty()));
    }

    #[tokio::test]
    async fn drive_content_chunk_returns_text() {
        use std::sync::Arc;
        use zeph_config::tools::SpeculativeConfig;
        use zeph_tools::{ToolCall, ToolError, ToolExecutor, ToolOutput};

        struct NullExec;
        impl ToolExecutor for NullExec {
            async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
                Ok(None)
            }
            async fn execute_tool_call(
                &self,
                _: &ToolCall,
            ) -> Result<Option<ToolOutput>, ToolError> {
                Ok(None)
            }
            fn is_tool_speculatable(&self, _: &str) -> bool {
                false
            }
        }
        let engine = Arc::new(super::super::SpeculationEngine::new(
            Arc::new(NullExec),
            SpeculativeConfig::default(),
        ));
        let events = vec![ToolSseEvent::ContentChunk("Hello world".into())];
        let drainer =
            SpeculativeStreamDrainer::new(Box::pin(tokio_stream::iter(events)), engine, 0.8);
        let result = drainer.drive().await.unwrap();
        assert!(matches!(result, ChatResponse::Text(s) if s == "Hello world"));
    }

    #[tokio::test]
    async fn drive_tool_call_complete_returns_tool_use() {
        use std::sync::Arc;
        use zeph_config::tools::SpeculativeConfig;
        use zeph_tools::{ToolCall, ToolError, ToolExecutor, ToolOutput};

        struct NullExec;
        impl ToolExecutor for NullExec {
            async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
                Ok(None)
            }
            async fn execute_tool_call(
                &self,
                _: &ToolCall,
            ) -> Result<Option<ToolOutput>, ToolError> {
                Ok(None)
            }
            fn is_tool_speculatable(&self, _: &str) -> bool {
                false
            }
        }
        let engine = Arc::new(super::super::SpeculationEngine::new(
            Arc::new(NullExec),
            SpeculativeConfig::default(),
        ));
        let events = vec![ToolSseEvent::ToolCallComplete {
            index: 0,
            id: "toolu_01".into(),
            name: "bash".into(),
            full_json: r#"{"command":"ls"}"#.into(),
        }];
        let drainer =
            SpeculativeStreamDrainer::new(Box::pin(tokio_stream::iter(events)), engine, 0.8);
        let result = drainer.drive().await.unwrap();
        match result {
            ChatResponse::ToolUse { tool_calls, .. } => {
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].id, "toolu_01");
                assert_eq!(tool_calls[0].name, "bash");
            }
            other @ ChatResponse::Text(_) => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn drive_error_event_propagates() {
        use std::sync::Arc;
        use zeph_config::tools::SpeculativeConfig;
        use zeph_tools::{ToolCall, ToolError, ToolExecutor, ToolOutput};

        struct NullExec;
        impl ToolExecutor for NullExec {
            async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
                Ok(None)
            }
            async fn execute_tool_call(
                &self,
                _: &ToolCall,
            ) -> Result<Option<ToolOutput>, ToolError> {
                Ok(None)
            }
            fn is_tool_speculatable(&self, _: &str) -> bool {
                false
            }
        }
        let engine = Arc::new(super::super::SpeculationEngine::new(
            Arc::new(NullExec),
            SpeculativeConfig::default(),
        ));
        let events = vec![ToolSseEvent::Error(zeph_llm::LlmError::SseParse(
            "boom".into(),
        ))];
        let drainer =
            SpeculativeStreamDrainer::new(Box::pin(tokio_stream::iter(events)), engine, 0.8);
        let result = drainer.drive().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("boom"));
    }
}
