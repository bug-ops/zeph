// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use futures::FutureExt as _;
use zeph_llm::provider::{ChatResponse, ToolDefinition};

use crate::agent::Agent;
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    /// Run `RuntimeLayer::before_chat` hooks. Returns `Ok(Some(sc))` when a layer short-circuits
    /// the LLM call, `Ok(None)` when all hooks pass through.
    #[tracing::instrument(name = "core.tool.before_chat_layers", skip_all, level = "debug", err)]
    pub(super) async fn run_before_chat_layers(
        &self,
        tool_defs: &[ToolDefinition],
    ) -> Result<Option<ChatResponse>, crate::agent::error::AgentError> {
        if self.runtime.config.layers.is_empty() {
            return Ok(None);
        }
        let conv_id_str = self
            .services
            .memory
            .persistence
            .conversation_id
            .map(|id| id.0.to_string());
        let ctx = crate::runtime_layer::LayerContext {
            conversation_id: conv_id_str.as_deref(),
            turn_number: u32::try_from(self.services.sidequest.turn_counter).unwrap_or(u32::MAX),
        };
        for layer in &self.runtime.config.layers {
            let hook_result = std::panic::AssertUnwindSafe(layer.before_chat(
                &ctx,
                &self.msg.messages,
                tool_defs,
            ))
            .catch_unwind()
            .await;
            match hook_result {
                Ok(Some(sc)) => {
                    tracing::debug!("RuntimeLayer short-circuited LLM call");
                    return Ok(Some(sc));
                }
                Ok(None) => {}
                Err(_) => tracing::warn!("RuntimeLayer::before_chat panicked, continuing"),
            }
        }
        Ok(None)
    }

    /// Run `RuntimeLayer::after_chat` hooks. Panics in hooks are logged and swallowed.
    #[tracing::instrument(name = "core.tool.after_chat_layers", skip_all, level = "debug")]
    pub(super) async fn run_after_chat_layers(&self, result: &ChatResponse) {
        if self.runtime.config.layers.is_empty() {
            return;
        }
        let conv_id_str = self
            .services
            .memory
            .persistence
            .conversation_id
            .map(|id| id.0.to_string());
        let ctx = crate::runtime_layer::LayerContext {
            conversation_id: conv_id_str.as_deref(),
            turn_number: u32::try_from(self.services.sidequest.turn_counter).unwrap_or(u32::MAX),
        };
        for layer in &self.runtime.config.layers {
            let hook_result = std::panic::AssertUnwindSafe(layer.after_chat(&ctx, result))
                .catch_unwind()
                .await;
            if hook_result.is_err() {
                tracing::warn!("RuntimeLayer::after_chat panicked, continuing");
            }
        }
    }
}
