// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `RuntimeLayer` trait: middleware hooks for LLM calls and tool dispatch (#2286).
//!
//! Provides interception points before and after each LLM chat call and each tool execution.
//! Layers are composed in a stack: each layer is called in order, and any layer may
//! short-circuit the actual call by returning `Some(result)` from `before_chat` or `before_tool`.
//!
//! # MVP
//!
//! No layers are registered at bootstrap — the `runtime_layers` vec in `Agent` defaults to empty,
//! making the hook loops zero-cost (no iteration, no allocation).
//!
//! Future layers (rate limiting, guardrails, cost tracking, audit logging) add themselves to
//! the vec at bootstrap without modifying the agent loop.
//!
//! # Implementation note
//!
//! Default implementations return `Box::pin(std::future::ready(...))`. This allocates once per
//! call per registered layer. For the MVP empty-vec case, no allocation occurs. Real layers
//! should keep their work minimal to avoid blocking the agent loop.

use std::future::Future;
use std::pin::Pin;

use zeph_llm::provider::{ChatResponse, Message, ToolDefinition};
use zeph_tools::ToolError;
use zeph_tools::executor::{ToolCall, ToolOutput};

/// Short-circuit result type for `before_tool`: `Some(result)` bypasses tool execution.
pub type BeforeToolResult = Option<Result<Option<ToolOutput>, ToolError>>;

/// Context available to runtime layers during interception.
#[derive(Debug)]
pub struct LayerContext<'a> {
    /// The current conversation ID, if known.
    pub conversation_id: Option<&'a str>,
    /// The agent turn counter (increments per user message).
    pub turn_number: u32,
}

/// Middleware layer that wraps LLM calls and tool dispatch.
///
/// Layers are composed in a stack; each layer may inspect, modify, or short-circuit
/// the request before passing it to the next layer (or the real executor).
///
/// All methods have default implementations that are no-ops, so implementors only need
/// to override the hooks they care about.
///
/// # Short-circuiting
///
/// Returning `Some(result)` from `before_chat` or `before_tool` bypasses the actual
/// LLM call or tool execution. Subsequent layers are still called with `after_chat` /
/// `after_tool` using the short-circuit result.
pub trait RuntimeLayer: Send + Sync {
    /// Called before an LLM chat call.
    ///
    /// Return `Some(response)` to short-circuit the actual LLM call.
    /// Return `None` to proceed normally.
    fn before_chat<'a>(
        &'a self,
        _ctx: &'a LayerContext<'_>,
        _messages: &'a [Message],
        _tools: &'a [ToolDefinition],
    ) -> Pin<Box<dyn Future<Output = Option<ChatResponse>> + Send + 'a>> {
        Box::pin(std::future::ready(None))
    }

    /// Called after an LLM chat call completes (or was short-circuited).
    fn after_chat<'a>(
        &'a self,
        _ctx: &'a LayerContext<'_>,
        _response: &'a ChatResponse,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(std::future::ready(()))
    }

    /// Called before tool execution.
    ///
    /// Return `Some(result)` to short-circuit the actual tool execution.
    /// Return `None` to proceed normally.
    fn before_tool<'a>(
        &'a self,
        _ctx: &'a LayerContext<'_>,
        _call: &'a ToolCall,
    ) -> Pin<Box<dyn Future<Output = BeforeToolResult> + Send + 'a>> {
        Box::pin(std::future::ready(None))
    }

    /// Called after tool execution completes (or was short-circuited).
    fn after_tool<'a>(
        &'a self,
        _ctx: &'a LayerContext<'_>,
        _call: &'a ToolCall,
        _result: &'a Result<Option<ToolOutput>, ToolError>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(std::future::ready(()))
    }
}

/// No-op layer that passes everything through unchanged.
///
/// Useful as a placeholder, for testing, or as a base for custom layers that only
/// need to override a subset of hooks.
pub struct NoopLayer;

impl RuntimeLayer for NoopLayer {}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_llm::provider::Role;

    struct CountingLayer {
        before_chat_calls: std::sync::atomic::AtomicU32,
        after_chat_calls: std::sync::atomic::AtomicU32,
    }

    impl CountingLayer {
        fn new() -> Self {
            Self {
                before_chat_calls: std::sync::atomic::AtomicU32::new(0),
                after_chat_calls: std::sync::atomic::AtomicU32::new(0),
            }
        }
    }

    impl RuntimeLayer for CountingLayer {
        fn before_chat<'a>(
            &'a self,
            _ctx: &'a LayerContext<'_>,
            _messages: &'a [Message],
            _tools: &'a [ToolDefinition],
        ) -> Pin<Box<dyn Future<Output = Option<ChatResponse>> + Send + 'a>> {
            self.before_chat_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::pin(std::future::ready(None))
        }

        fn after_chat<'a>(
            &'a self,
            _ctx: &'a LayerContext<'_>,
            _response: &'a ChatResponse,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
            self.after_chat_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::pin(std::future::ready(()))
        }
    }

    #[test]
    fn noop_layer_compiles_and_is_runtime_layer() {
        // Compile-time test: NoopLayer must implement RuntimeLayer.
        fn assert_runtime_layer<T: RuntimeLayer>() {}
        assert_runtime_layer::<NoopLayer>();
    }

    #[tokio::test]
    async fn noop_layer_before_chat_returns_none() {
        let layer = NoopLayer;
        let ctx = LayerContext {
            conversation_id: None,
            turn_number: 0,
        };
        let result = layer.before_chat(&ctx, &[], &[]).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn noop_layer_before_tool_returns_none() {
        let layer = NoopLayer;
        let ctx = LayerContext {
            conversation_id: None,
            turn_number: 0,
        };
        let call = ToolCall {
            tool_id: "shell".into(),
            params: serde_json::Map::new(),
        };
        let result = layer.before_tool(&ctx, &call).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn layer_hooks_are_called() {
        use std::sync::Arc;
        let layer = Arc::new(CountingLayer::new());
        let ctx = LayerContext {
            conversation_id: Some("conv-1"),
            turn_number: 3,
        };
        let resp = ChatResponse::Text("hello".into());

        let _ = layer.before_chat(&ctx, &[], &[]).await;
        layer.after_chat(&ctx, &resp).await;

        assert_eq!(
            layer
                .before_chat_calls
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(
            layer
                .after_chat_calls
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[tokio::test]
    async fn short_circuit_layer_returns_response() {
        struct ShortCircuitLayer;
        impl RuntimeLayer for ShortCircuitLayer {
            fn before_chat<'a>(
                &'a self,
                _ctx: &'a LayerContext<'_>,
                _messages: &'a [Message],
                _tools: &'a [ToolDefinition],
            ) -> Pin<Box<dyn Future<Output = Option<ChatResponse>> + Send + 'a>> {
                Box::pin(std::future::ready(Some(ChatResponse::Text(
                    "short-circuited".into(),
                ))))
            }
        }

        let layer = ShortCircuitLayer;
        let ctx = LayerContext {
            conversation_id: None,
            turn_number: 0,
        };
        let result = layer.before_chat(&ctx, &[], &[]).await;
        assert!(matches!(result, Some(ChatResponse::Text(ref s)) if s == "short-circuited"));
    }

    // Verify that Role is accessible from zeph_llm imports (ensures crate boundary is correct).
    #[test]
    fn message_from_legacy_compiles() {
        let _msg = Message::from_legacy(Role::User, "hello");
    }

    /// Two layers registered in order [A, B]: `before_chat` must be called A then B,
    /// and `after_chat` must be called A then B (forward order for both in MVP's loop).
    #[tokio::test]
    async fn multiple_layers_called_in_registration_order() {
        use std::sync::{Arc, Mutex};

        struct OrderLayer {
            id: u32,
            log: Arc<Mutex<Vec<String>>>,
        }
        impl RuntimeLayer for OrderLayer {
            fn before_chat<'a>(
                &'a self,
                _ctx: &'a LayerContext<'_>,
                _messages: &'a [Message],
                _tools: &'a [ToolDefinition],
            ) -> Pin<Box<dyn Future<Output = Option<ChatResponse>> + Send + 'a>> {
                let entry = format!("before_{}", self.id);
                self.log.lock().unwrap().push(entry);
                Box::pin(std::future::ready(None))
            }

            fn after_chat<'a>(
                &'a self,
                _ctx: &'a LayerContext<'_>,
                _response: &'a ChatResponse,
            ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
                let entry = format!("after_{}", self.id);
                self.log.lock().unwrap().push(entry);
                Box::pin(std::future::ready(()))
            }
        }

        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let layer_a = OrderLayer {
            id: 1,
            log: Arc::clone(&log),
        };
        let layer_b = OrderLayer {
            id: 2,
            log: Arc::clone(&log),
        };

        let ctx = LayerContext {
            conversation_id: None,
            turn_number: 0,
        };
        let resp = ChatResponse::Text("ok".into());

        layer_a.before_chat(&ctx, &[], &[]).await;
        layer_b.before_chat(&ctx, &[], &[]).await;
        layer_a.after_chat(&ctx, &resp).await;
        layer_b.after_chat(&ctx, &resp).await;

        let events = log.lock().unwrap().clone();
        assert_eq!(
            events,
            vec!["before_1", "before_2", "after_1", "after_2"],
            "hooks must fire in registration order"
        );
    }

    /// `after_chat` must receive the short-circuit response produced by `before_chat`.
    #[tokio::test]
    async fn after_chat_receives_short_circuit_response() {
        use std::sync::{Arc, Mutex};

        struct CapturingAfter {
            captured: Arc<Mutex<Option<String>>>,
        }
        impl RuntimeLayer for CapturingAfter {
            fn after_chat<'a>(
                &'a self,
                _ctx: &'a LayerContext<'_>,
                response: &'a ChatResponse,
            ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
                if let ChatResponse::Text(t) = response {
                    *self.captured.lock().unwrap() = Some(t.clone());
                }
                Box::pin(std::future::ready(()))
            }
        }

        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let layer = CapturingAfter {
            captured: Arc::clone(&captured),
        };
        let ctx = LayerContext {
            conversation_id: None,
            turn_number: 0,
        };

        // Simulate: before_chat short-circuits; caller passes result to after_chat.
        let sc_response = ChatResponse::Text("short-circuit".into());
        layer.after_chat(&ctx, &sc_response).await;

        let got = captured.lock().unwrap().clone();
        assert_eq!(
            got.as_deref(),
            Some("short-circuit"),
            "after_chat must receive the short-circuit response"
        );
    }

    /// Two layers registered in order [A, B]: `before_tool` must fire A then B,
    /// and `after_tool` must fire A then B (forward order for both).
    #[tokio::test]
    async fn multi_layer_before_after_tool_ordering() {
        use std::sync::{Arc, Mutex};

        struct ToolOrderLayer {
            id: u32,
            log: Arc<Mutex<Vec<String>>>,
        }
        impl RuntimeLayer for ToolOrderLayer {
            fn before_tool<'a>(
                &'a self,
                _ctx: &'a LayerContext<'_>,
                _call: &'a ToolCall,
            ) -> Pin<Box<dyn Future<Output = BeforeToolResult> + Send + 'a>> {
                self.log
                    .lock()
                    .unwrap()
                    .push(format!("before_tool_{}", self.id));
                Box::pin(std::future::ready(None))
            }

            fn after_tool<'a>(
                &'a self,
                _ctx: &'a LayerContext<'_>,
                _call: &'a ToolCall,
                _result: &'a Result<Option<ToolOutput>, ToolError>,
            ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
                self.log
                    .lock()
                    .unwrap()
                    .push(format!("after_tool_{}", self.id));
                Box::pin(std::future::ready(()))
            }
        }

        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let layer_a = ToolOrderLayer {
            id: 1,
            log: Arc::clone(&log),
        };
        let layer_b = ToolOrderLayer {
            id: 2,
            log: Arc::clone(&log),
        };

        let ctx = LayerContext {
            conversation_id: None,
            turn_number: 0,
        };
        let call = ToolCall {
            tool_id: "shell".into(),
            params: serde_json::Map::new(),
        };
        let result: Result<Option<ToolOutput>, ToolError> = Ok(None);

        layer_a.before_tool(&ctx, &call).await;
        layer_b.before_tool(&ctx, &call).await;
        layer_a.after_tool(&ctx, &call, &result).await;
        layer_b.after_tool(&ctx, &call, &result).await;

        let events = log.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![
                "before_tool_1",
                "before_tool_2",
                "after_tool_1",
                "after_tool_2"
            ],
            "tool hooks must fire in registration order"
        );
    }

    /// `NoopLayer` `after_tool` returns `()` without errors.
    #[tokio::test]
    async fn noop_layer_after_tool_returns_unit() {
        use zeph_tools::executor::ToolOutput;

        let layer = NoopLayer;
        let ctx = LayerContext {
            conversation_id: None,
            turn_number: 0,
        };
        let call = ToolCall {
            tool_id: "shell".into(),
            params: serde_json::Map::new(),
        };
        let result: Result<Option<ToolOutput>, zeph_tools::ToolError> = Ok(None);
        layer.after_tool(&ctx, &call, &result).await;
        // No assertion needed — the test verifies it compiles and doesn't panic.
    }
}
