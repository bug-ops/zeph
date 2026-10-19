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
use zeph_llm::provider::{AssistantContentBuilder, ChatResponse, ToolUseRequest};
use zeph_llm::sse::{ToolSseEvent, ToolSseStream};
use zeph_llm::thinking_replay::ThinkingReplay;

use super::SpeculationEngine;
use super::partial_json::{PartialJsonParser, PrefixState};
use super::prediction::{Prediction, PredictionSource};
use crate::agent::turn_gate::DispatchGate;

/// Result of draining one tool stream.
#[derive(Debug)]
pub struct DrainedTurn {
    /// The assembled response, identical to what `chat_with_tools` would return.
    pub response: ChatResponse,
    /// Elapsed milliseconds to the first SSE event, `None` if the stream yielded none.
    pub ttft_ms: Option<u64>,
}

/// Drives a `ToolSseStream`, intercepts `InputJsonDelta` events for speculative dispatch,
/// and reassembles a `ChatResponse` for the normal tool-dispatch path.
///
/// `ToolBlockStart` events populate tool metadata before any `InputJsonDelta` arrives,
/// ensuring speculative dispatch can fire incrementally during streaming.
pub struct SpeculativeStreamDrainer {
    stream: ToolSseStream,
    engine: Arc<SpeculationEngine>,
    confidence_threshold: f32,
    /// Dispatch gates applied before any speculative execution: speculation obeys the same
    /// turn-set and `requires` rules as the normal path.
    gate: DispatchGate,
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
            gate: DispatchGate::open(),
            parsers: HashMap::new(),
        }
    }

    /// Gate speculative dispatches with the snapshot taken for the current request.
    #[must_use]
    pub fn with_gate(mut self, gate: DispatchGate) -> Self {
        self.gate = gate;
        self
    }

    /// Drain the stream, firing speculative dispatches and building the final `ChatResponse`.
    ///
    /// Returns the assembled `ChatResponse` — identical contract to
    /// `LlmProvider::chat_with_tools` — paired with the true time-to-first-token in
    /// milliseconds (issue #6549): the elapsed time from the start of this call to the first
    /// SSE event received, `None` if the stream ended without ever yielding one. This is the
    /// only production streaming path that feeds the per-message usage ledger (speculative
    /// decoding, `SpeculationMode::Decoding`/`Both`) — capturing it here, at the actual
    /// consumption point, avoids threading a shared usage tracker through `zeph-llm`'s SSE
    /// combinators just for this one call site.
    ///
    /// # Errors
    ///
    /// Returns an `LlmError` if the SSE stream signals a parse or API error.
    pub async fn drive(mut self) -> Result<DrainedTurn, zeph_llm::LlmError> {
        let mut content = AssistantContentBuilder::default();
        let mut has_tool_calls = false;
        let mut replay = ThinkingReplay::Intact;
        let mut compaction: Option<String> = None;
        // Map from tool index → (id, name) collected from ToolCallComplete.
        let mut tool_meta: HashMap<usize, (String, String)> = HashMap::new();
        // Track which tool indices have had their speculation fired.
        let mut dispatched: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let drive_start = std::time::Instant::now();
        let mut ttft_ms: Option<u64> = None;

        while let Some(event) = self.stream.next().await {
            if ttft_ms.is_none() {
                ttft_ms =
                    Some(u64::try_from(drive_start.elapsed().as_millis()).unwrap_or(u64::MAX));
            }
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
                        if self.engine.try_dispatch(
                            &pred,
                            zeph_common::SkillTrustLevel::Trusted,
                            &self.gate,
                        ) {
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

                    has_tool_calls = true;
                    content.push_tool_use(ToolUseRequest {
                        id,
                        name: name.into(),
                        input,
                    });
                }
                ToolSseEvent::ThinkingBlockDone(block) => {
                    content.push_thinking(block);
                }
                ToolSseEvent::ThinkingReplay(r) => {
                    replay = r;
                }
                ToolSseEvent::ContentChunk(text) => {
                    content.push_text(&text);
                }
                ToolSseEvent::Compaction(summary) => {
                    compaction = Some(summary);
                }
                ToolSseEvent::Error(e) => {
                    return Err(e);
                }
                // ThinkingChunk pass-through; full block assembled via ThinkingBlockDone.
                ToolSseEvent::ThinkingChunk(_) | _ => {}
            }
        }

        let content = content.build();
        let response = if has_tool_calls {
            ChatResponse::ToolUse {
                content,
                replay,
                compaction,
            }
        } else {
            ChatResponse::Text(content.text().unwrap_or_default())
        };
        Ok(DrainedTurn { response, ttft_ms })
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
    use std::assert_matches;

    #[tokio::test]
    async fn drainer_new_has_empty_parsers() {
        use std::sync::Arc;
        use zeph_config::tools::SpeculativeConfig;
        use zeph_tools::{ToolCall, ToolError, ToolExecutor, ToolOutput};

        struct NullExec;
        impl ToolExecutor for NullExec {
            fn execute(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send
            {
                std::future::ready(Ok(None))
            }
            fn execute_tool_call(
                &self,
                _: &ToolCall,
            ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send
            {
                std::future::ready(Ok(None))
            }
            fn is_tool_speculatable(&self, _: &str) -> bool {
                false
            }
            fn execute_tool_call_confirmed(
                &self,
                call: &ToolCall,
            ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send
            {
                self.execute_tool_call(call)
            }
            fn checkpoint_undo(&self, _n: usize) -> zeph_tools::CheckpointActionResult {
                zeph_tools::CheckpointActionResult::unsupported()
            }
            fn checkpoint_redo(&self) -> zeph_tools::CheckpointActionResult {
                zeph_tools::CheckpointActionResult::unsupported()
            }
            fn checkpoint_list(&self) -> zeph_tools::CheckpointListResult {
                zeph_tools::CheckpointListResult::default()
            }
            fn requires_confirmation(&self, _call: &ToolCall) -> bool {
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
            fn execute(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send
            {
                std::future::ready(Ok(None))
            }
            fn execute_tool_call(
                &self,
                _: &ToolCall,
            ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send
            {
                std::future::ready(Ok(None))
            }
            fn is_tool_speculatable(&self, _: &str) -> bool {
                self.count.fetch_add(1, Ordering::Relaxed);
                true
            }
            fn execute_tool_call_confirmed(
                &self,
                call: &ToolCall,
            ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send
            {
                self.execute_tool_call(call)
            }
            fn checkpoint_undo(&self, _n: usize) -> zeph_tools::CheckpointActionResult {
                zeph_tools::CheckpointActionResult::unsupported()
            }
            fn checkpoint_redo(&self) -> zeph_tools::CheckpointActionResult {
                zeph_tools::CheckpointActionResult::unsupported()
            }
            fn checkpoint_list(&self) -> zeph_tools::CheckpointListResult {
                zeph_tools::CheckpointListResult::default()
            }
            fn requires_confirmation(&self, _call: &ToolCall) -> bool {
                false
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
        let DrainedTurn {
            response: result, ..
        } = drainer.drive().await.unwrap();
        // Drainer assembled a ToolUse response.
        assert_matches!(result, ChatResponse::ToolUse { .. });
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
            fn execute(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send
            {
                std::future::ready(Ok(None))
            }
            fn execute_tool_call(
                &self,
                _: &ToolCall,
            ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send
            {
                std::future::ready(Ok(None))
            }
            fn is_tool_speculatable(&self, _: &str) -> bool {
                false
            }
            fn execute_tool_call_confirmed(
                &self,
                call: &ToolCall,
            ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send
            {
                self.execute_tool_call(call)
            }
            fn checkpoint_undo(&self, _n: usize) -> zeph_tools::CheckpointActionResult {
                zeph_tools::CheckpointActionResult::unsupported()
            }
            fn checkpoint_redo(&self) -> zeph_tools::CheckpointActionResult {
                zeph_tools::CheckpointActionResult::unsupported()
            }
            fn checkpoint_list(&self) -> zeph_tools::CheckpointListResult {
                zeph_tools::CheckpointListResult::default()
            }
            fn requires_confirmation(&self, _call: &ToolCall) -> bool {
                false
            }
        }
        let engine = Arc::new(super::super::SpeculationEngine::new(
            Arc::new(NullExec),
            SpeculativeConfig::default(),
        ));
        let drainer = SpeculativeStreamDrainer::new(Box::pin(tokio_stream::empty()), engine, 0.8);
        let DrainedTurn {
            response: result,
            ttft_ms,
            ..
        } = drainer.drive().await.unwrap();
        assert_matches!(result, ChatResponse::Text(s) if s.is_empty());
        assert_eq!(
            ttft_ms, None,
            "issue #6549: a stream that never yields an event has no TTFT"
        );
    }

    #[tokio::test]
    async fn drive_content_chunk_returns_text() {
        use std::sync::Arc;
        use zeph_config::tools::SpeculativeConfig;
        use zeph_tools::{ToolCall, ToolError, ToolExecutor, ToolOutput};

        struct NullExec;
        impl ToolExecutor for NullExec {
            fn execute(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send
            {
                std::future::ready(Ok(None))
            }
            fn execute_tool_call(
                &self,
                _: &ToolCall,
            ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send
            {
                std::future::ready(Ok(None))
            }
            fn is_tool_speculatable(&self, _: &str) -> bool {
                false
            }
            fn execute_tool_call_confirmed(
                &self,
                call: &ToolCall,
            ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send
            {
                self.execute_tool_call(call)
            }
            fn checkpoint_undo(&self, _n: usize) -> zeph_tools::CheckpointActionResult {
                zeph_tools::CheckpointActionResult::unsupported()
            }
            fn checkpoint_redo(&self) -> zeph_tools::CheckpointActionResult {
                zeph_tools::CheckpointActionResult::unsupported()
            }
            fn checkpoint_list(&self) -> zeph_tools::CheckpointListResult {
                zeph_tools::CheckpointListResult::default()
            }
            fn requires_confirmation(&self, _call: &ToolCall) -> bool {
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
        let DrainedTurn {
            response: result,
            ttft_ms,
            ..
        } = drainer.drive().await.unwrap();
        assert_matches!(result, ChatResponse::Text(s) if s == "Hello world");
        assert!(
            ttft_ms.is_some(),
            "issue #6549: a stream that yields at least one event must report a TTFT"
        );
    }

    #[tokio::test]
    async fn drive_tool_call_complete_returns_ordered_tool_use() {
        use std::sync::Arc;
        use zeph_config::tools::SpeculativeConfig;
        use zeph_tools::{ToolCall, ToolError, ToolExecutor, ToolOutput};

        struct NullExec;
        impl ToolExecutor for NullExec {
            fn execute(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send
            {
                std::future::ready(Ok(None))
            }
            fn execute_tool_call(
                &self,
                _: &ToolCall,
            ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send
            {
                std::future::ready(Ok(None))
            }
            fn is_tool_speculatable(&self, _: &str) -> bool {
                false
            }
            fn execute_tool_call_confirmed(
                &self,
                call: &ToolCall,
            ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send
            {
                self.execute_tool_call(call)
            }
            fn checkpoint_undo(&self, _n: usize) -> zeph_tools::CheckpointActionResult {
                zeph_tools::CheckpointActionResult::unsupported()
            }
            fn checkpoint_redo(&self) -> zeph_tools::CheckpointActionResult {
                zeph_tools::CheckpointActionResult::unsupported()
            }
            fn checkpoint_list(&self) -> zeph_tools::CheckpointListResult {
                zeph_tools::CheckpointListResult::default()
            }
            fn requires_confirmation(&self, _call: &ToolCall) -> bool {
                false
            }
        }
        let engine = Arc::new(super::super::SpeculationEngine::new(
            Arc::new(NullExec),
            SpeculativeConfig::default(),
        ));
        let thinking = |text: &str| zeph_llm::provider::ThinkingBlock::Thinking {
            thinking: text.into(),
            signature: "sig".into(),
        };
        let events = vec![
            ToolSseEvent::ThinkingReplay(ThinkingReplay::Dropped {
                first_message_index: 3,
            }),
            ToolSseEvent::ThinkingBlockDone(thinking("first")),
            ToolSseEvent::ContentChunk("pl".into()),
            ToolSseEvent::ContentChunk("an".into()),
            ToolSseEvent::ThinkingBlockDone(thinking("second")),
            ToolSseEvent::ToolCallComplete {
                index: 3,
                id: "toolu_01".into(),
                name: "bash".into(),
                full_json: r#"{"command":"ls"}"#.into(),
            },
        ];
        let drainer =
            SpeculativeStreamDrainer::new(Box::pin(tokio_stream::iter(events)), engine, 0.8);
        let DrainedTurn {
            response: result, ..
        } = drainer.drive().await.unwrap();
        let ChatResponse::ToolUse {
            content, replay, ..
        } = result
        else {
            panic!("expected ToolUse, got {result:?}");
        };
        assert_eq!(
            replay,
            ThinkingReplay::Dropped {
                first_message_index: 3
            }
        );
        assert!(matches!(
            content.blocks(),
            [
                zeph_llm::provider::AssistantBlock::Thinking(
                    zeph_llm::provider::ThinkingBlock::Thinking { thinking: a, .. }
                ),
                zeph_llm::provider::AssistantBlock::Text(t),
                zeph_llm::provider::AssistantBlock::Thinking(
                    zeph_llm::provider::ThinkingBlock::Thinking { thinking: b, .. }
                ),
                zeph_llm::provider::AssistantBlock::ToolUse(call),
            ] if a == "first" && t == "plan" && b == "second"
                && call.id == "toolu_01" && call.name == "bash"
        ));
    }

    fn streamed_call_events() -> Vec<ToolSseEvent> {
        vec![
            ToolSseEvent::ToolBlockStart {
                index: 0,
                id: "toolu_01".into(),
                name: "test".into(),
            },
            ToolSseEvent::InputJsonDelta {
                index: 0,
                delta: r#"{"command":"ls"}"#.into(),
            },
        ]
    }

    async fn drive_with_gate(gate: DispatchGate) -> Arc<super::super::SpeculationEngine> {
        use zeph_config::tools::{SpeculationMode, SpeculativeConfig};

        let engine = Arc::new(super::super::SpeculationEngine::new(
            Arc::new(super::super::tests::AlwaysOkExecutor),
            SpeculativeConfig {
                mode: SpeculationMode::Decoding,
                ..Default::default()
            },
        ));
        let drainer = SpeculativeStreamDrainer::new(
            Box::pin(tokio_stream::iter(streamed_call_events())),
            Arc::clone(&engine),
            0.0,
        )
        .with_gate(gate);
        drainer.drive().await.unwrap();
        engine
    }

    #[tokio::test]
    async fn streamed_call_is_dispatched_speculatively_when_the_gate_is_open() {
        let engine = drive_with_gate(DispatchGate::open()).await;
        assert!(
            !engine.cache.is_empty(),
            "precondition: the call is speculatable"
        );
    }

    #[tokio::test]
    async fn streamed_call_outside_the_turn_set_is_not_dispatched() {
        use crate::agent::turn_gate::{RequiresGate, TurnToolSet};

        let engine = drive_with_gate(DispatchGate::new(
            TurnToolSet::default(),
            RequiresGate::open(),
        ))
        .await;
        assert!(engine.cache.is_empty());
    }

    #[tokio::test]
    async fn stream_compaction_rides_on_the_response_and_only_on_that_one() {
        use zeph_config::tools::SpeculativeConfig;

        let drive = |events: Vec<ToolSseEvent>| async move {
            let engine = Arc::new(super::super::SpeculationEngine::new(
                Arc::new(super::super::tests::AlwaysOkExecutor),
                SpeculativeConfig::default(),
            ));
            SpeculativeStreamDrainer::new(Box::pin(tokio_stream::iter(events)), engine, 0.8)
                .drive()
                .await
                .unwrap()
                .response
        };
        let call = || ToolSseEvent::ToolCallComplete {
            index: 0,
            id: "t".into(),
            name: "bash".into(),
            full_json: "{}".into(),
        };

        let with = drive(vec![ToolSseEvent::Compaction("summary".into()), call()]).await;
        let ChatResponse::ToolUse { compaction, .. } = with else {
            panic!("expected ToolUse");
        };
        assert_eq!(compaction.as_deref(), Some("summary"));

        let without = drive(vec![call()]).await;
        let ChatResponse::ToolUse { compaction, .. } = without else {
            panic!("expected ToolUse");
        };
        assert_eq!(compaction, None, "a stream without compaction carries none");
    }

    #[tokio::test]
    async fn drive_error_event_propagates() {
        use std::sync::Arc;
        use zeph_config::tools::SpeculativeConfig;
        use zeph_tools::{ToolCall, ToolError, ToolExecutor, ToolOutput};

        struct NullExec;
        impl ToolExecutor for NullExec {
            fn execute(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send
            {
                std::future::ready(Ok(None))
            }
            fn execute_tool_call(
                &self,
                _: &ToolCall,
            ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send
            {
                std::future::ready(Ok(None))
            }
            fn is_tool_speculatable(&self, _: &str) -> bool {
                false
            }
            fn execute_tool_call_confirmed(
                &self,
                call: &ToolCall,
            ) -> impl std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send
            {
                self.execute_tool_call(call)
            }
            fn checkpoint_undo(&self, _n: usize) -> zeph_tools::CheckpointActionResult {
                zeph_tools::CheckpointActionResult::unsupported()
            }
            fn checkpoint_redo(&self) -> zeph_tools::CheckpointActionResult {
                zeph_tools::CheckpointActionResult::unsupported()
            }
            fn checkpoint_list(&self) -> zeph_tools::CheckpointListResult {
                zeph_tools::CheckpointListResult::default()
            }
            fn requires_confirmation(&self, _call: &ToolCall) -> bool {
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
