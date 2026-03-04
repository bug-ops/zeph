// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Provider router: EMA-based and Thompson Sampling strategies.

pub mod thompson;

use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::any::AnyProvider;
use crate::ema::EmaTracker;
use crate::error::LlmError;
use crate::provider::{ChatResponse, ChatStream, LlmProvider, Message, StatusTx, ToolDefinition};

use thompson::ThompsonState;

/// Routing strategy used by [`RouterProvider`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RouterStrategy {
    /// Exponential moving average-based latency-aware ordering.
    #[default]
    Ema,
    /// Thompson Sampling with Beta distributions.
    Thompson,
}

#[derive(Debug, Clone)]
pub struct RouterProvider {
    providers: Vec<AnyProvider>,
    status_tx: Option<StatusTx>,
    ema: Option<EmaTracker>,
    provider_order: Arc<Mutex<Vec<usize>>>,
    strategy: RouterStrategy,
    thompson: Option<Arc<Mutex<ThompsonState>>>,
    /// Path for persisting Thompson state. `None` disables persistence.
    thompson_state_path: Option<std::path::PathBuf>,
}

impl RouterProvider {
    #[must_use]
    pub fn new(providers: Vec<AnyProvider>) -> Self {
        let n = providers.len();
        Self {
            providers,
            status_tx: None,
            ema: None,
            provider_order: Arc::new(Mutex::new((0..n).collect())),
            strategy: RouterStrategy::Ema,
            thompson: None,
            thompson_state_path: None,
        }
    }

    /// Enable EMA-based adaptive provider ordering.
    #[must_use]
    pub fn with_ema(mut self, alpha: f64, reorder_interval: u64) -> Self {
        self.ema = Some(EmaTracker::new(alpha, reorder_interval));
        self
    }

    /// Enable Thompson Sampling strategy.
    ///
    /// Loads existing state from `state_path` if present; falls back to uniform prior.
    #[must_use]
    pub fn with_thompson(mut self, state_path: Option<&Path>) -> Self {
        self.strategy = RouterStrategy::Thompson;
        let path = state_path.map_or_else(ThompsonState::default_path, Path::to_path_buf);
        let state = ThompsonState::load(&path);
        self.thompson = Some(Arc::new(Mutex::new(state)));
        self.thompson_state_path = Some(path);
        self
    }

    /// Persist current Thompson state to disk.
    ///
    /// No-op if Thompson strategy is not active.
    pub fn save_thompson_state(&self) {
        let (Some(thompson), Some(path)) = (&self.thompson, &self.thompson_state_path) else {
            return;
        };
        let state = thompson
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Err(e) = state.save(path) {
            tracing::warn!(error = %e, "failed to save Thompson router state");
        }
    }

    fn ordered_providers(&self) -> Vec<AnyProvider> {
        match self.strategy {
            RouterStrategy::Thompson => self.thompson_ordered_providers(),
            RouterStrategy::Ema => self.ema_ordered_providers(),
        }
    }

    fn ema_ordered_providers(&self) -> Vec<AnyProvider> {
        let order = self
            .provider_order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        order
            .iter()
            .filter_map(|&i| self.providers.get(i).cloned())
            .collect()
    }

    fn thompson_ordered_providers(&self) -> Vec<AnyProvider> {
        let Some(ref thompson) = self.thompson else {
            return self.providers.clone();
        };
        let state = thompson
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let names: Vec<String> = self.providers.iter().map(|p| p.name().to_owned()).collect();
        let selected = state.select(&names);
        // Put selected provider first, keep rest in original order.
        let mut ordered = self.providers.clone();
        if let Some(selected_name) = selected
            && let Some(pos) = ordered.iter().position(|p| p.name() == selected_name)
        {
            ordered.swap(0, pos);
        }
        ordered
    }

    fn record_outcome(&self, provider_name: &str, success: bool, latency_ms: u64) {
        match self.strategy {
            RouterStrategy::Thompson => {
                if let Some(ref thompson) = self.thompson {
                    let mut state = thompson
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    state.update(provider_name, success);
                }
            }
            RouterStrategy::Ema => {
                self.ema_record(provider_name, success, latency_ms);
            }
        }
    }

    fn ema_record(&self, provider_name: &str, success: bool, latency_ms: u64) {
        let Some(ref ema) = self.ema else {
            return;
        };
        ema.record(provider_name, success, latency_ms);
        let current_names: Vec<String> =
            self.providers.iter().map(|p| p.name().to_owned()).collect();
        if let Some(new_order_names) = ema.maybe_reorder(&current_names) {
            let name_to_idx: std::collections::HashMap<&str, usize> = self
                .providers
                .iter()
                .enumerate()
                .map(|(i, p)| (p.name(), i))
                .collect();
            let new_order: Vec<usize> = new_order_names
                .iter()
                .filter_map(|n| name_to_idx.get(n.as_str()).copied())
                .collect();
            let mut order = self
                .provider_order
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *order = new_order;
        }
    }

    pub fn set_status_tx(&mut self, tx: StatusTx) {
        for p in &mut self.providers {
            p.set_status_tx(tx.clone());
        }
        self.status_tx = Some(tx);
    }

    /// Aggregate model lists from all sub-providers, deduplicating by id.
    ///
    /// Individual sub-provider errors are logged as warnings and skipped.
    ///
    /// # Errors
    ///
    /// Always succeeds (errors per-provider are swallowed).
    pub async fn list_models_remote(
        &self,
    ) -> Result<Vec<crate::model_cache::RemoteModelInfo>, LlmError> {
        let mut seen = std::collections::HashSet::new();
        let mut all = Vec::new();
        for p in &self.providers {
            match p.list_models_remote().await {
                Ok(models) => {
                    for m in models {
                        if seen.insert(m.id.clone()) {
                            all.push(m);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "router: list_models_remote sub-provider failed");
                }
            }
        }
        Ok(all)
    }
}

impl LlmProvider for RouterProvider {
    fn context_window(&self) -> Option<usize> {
        self.providers.first().and_then(LlmProvider::context_window)
    }

    fn chat(
        &self,
        messages: &[Message],
    ) -> impl std::future::Future<Output = Result<String, LlmError>> + Send {
        let providers = self.ordered_providers();
        let status_tx = self.status_tx.clone();
        let messages = messages.to_vec();
        let router = self.clone();
        Box::pin(async move {
            for p in &providers {
                let start = std::time::Instant::now();
                match p.chat(&messages).await {
                    Ok(r) => {
                        router.record_outcome(
                            p.name(),
                            true,
                            u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                        );
                        return Ok(r);
                    }
                    Err(e) => {
                        router.record_outcome(
                            p.name(),
                            false,
                            u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                        );
                        if let Some(ref tx) = status_tx {
                            let _ = tx.send(format!("router: {} failed, falling back", p.name()));
                        }
                        tracing::warn!(provider = p.name(), error = %e, "router fallback");
                    }
                }
            }
            Err(LlmError::NoProviders)
        })
    }

    fn chat_stream(
        &self,
        messages: &[Message],
    ) -> impl std::future::Future<Output = Result<ChatStream, LlmError>> + Send {
        let providers = self.ordered_providers();
        let status_tx = self.status_tx.clone();
        let messages = messages.to_vec();
        let router = self.clone();
        Box::pin(async move {
            for p in &providers {
                let start = std::time::Instant::now();
                match p.chat_stream(&messages).await {
                    Ok(r) => {
                        router.record_outcome(
                            p.name(),
                            true,
                            u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                        );
                        return Ok(r);
                    }
                    Err(e) => {
                        router.record_outcome(
                            p.name(),
                            false,
                            u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                        );
                        if let Some(ref tx) = status_tx {
                            let _ = tx.send(format!("router: {} failed, falling back", p.name()));
                        }
                        tracing::warn!(provider = p.name(), error = %e, "router stream fallback");
                    }
                }
            }
            Err(LlmError::NoProviders)
        })
    }

    fn supports_streaming(&self) -> bool {
        self.providers.iter().any(LlmProvider::supports_streaming)
    }

    fn embed(
        &self,
        text: &str,
    ) -> impl std::future::Future<Output = Result<Vec<f32>, LlmError>> + Send {
        let providers = self.ordered_providers();
        let status_tx = self.status_tx.clone();
        let text = text.to_owned();
        let router = self.clone();
        Box::pin(async move {
            for p in &providers {
                if !p.supports_embeddings() {
                    continue;
                }
                let start = std::time::Instant::now();
                match p.embed(&text).await {
                    Ok(r) => {
                        router.record_outcome(
                            p.name(),
                            true,
                            u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                        );
                        return Ok(r);
                    }
                    Err(e) => {
                        router.record_outcome(
                            p.name(),
                            false,
                            u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                        );
                        if let Some(ref tx) = status_tx {
                            let _ =
                                tx.send(format!("router: {} embed failed, falling back", p.name()));
                        }
                        tracing::warn!(provider = p.name(), error = %e, "router embed fallback");
                    }
                }
            }
            Err(LlmError::NoProviders)
        })
    }

    fn supports_embeddings(&self) -> bool {
        self.providers.iter().any(LlmProvider::supports_embeddings)
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "router"
    }

    fn supports_tool_use(&self) -> bool {
        self.providers.iter().any(LlmProvider::supports_tool_use)
    }

    fn list_models(&self) -> Vec<String> {
        self.providers
            .iter()
            .flat_map(super::provider::LlmProvider::list_models)
            .collect()
    }

    #[allow(async_fn_in_trait)]
    async fn chat_with_tools(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatResponse, LlmError> {
        let providers = self.ordered_providers();
        let messages = messages.to_vec();
        let tools = tools.to_vec();
        let status_tx = self.status_tx.clone();
        let router = self.clone();
        Box::pin(async move {
            for p in &providers {
                if !p.supports_tool_use() {
                    continue;
                }
                let start = std::time::Instant::now();
                match p.chat_with_tools(&messages, &tools).await {
                    Ok(r) => {
                        router.record_outcome(
                            p.name(),
                            true,
                            u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                        );
                        return Ok(r);
                    }
                    Err(e) => {
                        router.record_outcome(
                            p.name(),
                            false,
                            u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                        );
                        if let Some(ref tx) = status_tx {
                            let _ = tx.send(format!(
                                "router: {} tool call failed, falling back",
                                p.name()
                            ));
                        }
                        tracing::warn!(provider = p.name(), error = %e, "router tool fallback");
                    }
                }
            }
            Err(LlmError::NoProviders)
        })
        .await
    }

    fn last_cache_usage(&self) -> Option<(u64, u64)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Role;

    #[test]
    fn empty_router_name() {
        let r = RouterProvider::new(vec![]);
        assert_eq!(r.name(), "router");
    }

    #[test]
    fn empty_router_supports_nothing() {
        let r = RouterProvider::new(vec![]);
        assert!(!r.supports_streaming());
        assert!(!r.supports_embeddings());
        assert!(!r.supports_tool_use());
    }

    #[test]
    fn empty_router_context_window_none() {
        let r = RouterProvider::new(vec![]);
        assert!(r.context_window().is_none());
    }

    #[tokio::test]
    async fn empty_router_chat_returns_no_providers() {
        let r = RouterProvider::new(vec![]);
        let msgs = vec![Message::from_legacy(Role::User, "hello")];
        let err = r.chat(&msgs).await.unwrap_err();
        assert!(matches!(err, LlmError::NoProviders));
    }

    #[tokio::test]
    async fn empty_router_chat_stream_returns_no_providers() {
        let r = RouterProvider::new(vec![]);
        let msgs = vec![Message::from_legacy(Role::User, "hello")];
        let result = r.chat_stream(&msgs).await;
        assert!(matches!(result, Err(LlmError::NoProviders)));
    }

    #[tokio::test]
    async fn empty_router_embed_returns_no_providers() {
        let r = RouterProvider::new(vec![]);
        let err = r.embed("test").await.unwrap_err();
        assert!(matches!(err, LlmError::NoProviders));
    }

    #[tokio::test]
    async fn empty_router_chat_with_tools_returns_no_providers() {
        let r = RouterProvider::new(vec![]);
        let msgs = vec![Message::from_legacy(Role::User, "hello")];
        let err = r.chat_with_tools(&msgs, &[]).await.unwrap_err();
        assert!(matches!(err, LlmError::NoProviders));
    }

    #[tokio::test]
    async fn router_falls_back_on_unreachable() {
        use crate::ollama::OllamaProvider;

        let p1 = AnyProvider::Ollama(OllamaProvider::new(
            "http://127.0.0.1:1",
            "m".into(),
            "e".into(),
        ));
        let p2 = AnyProvider::Ollama(OllamaProvider::new(
            "http://127.0.0.1:2",
            "m".into(),
            "e".into(),
        ));
        let r = RouterProvider::new(vec![p1, p2]);
        let msgs = vec![Message::from_legacy(Role::User, "hello")];
        let err = r.chat(&msgs).await.unwrap_err();
        assert!(matches!(err, LlmError::NoProviders));
    }

    #[test]
    fn router_with_streaming_provider() {
        use crate::ollama::OllamaProvider;

        let p = AnyProvider::Ollama(OllamaProvider::new(
            "http://127.0.0.1:1",
            "m".into(),
            "e".into(),
        ));
        let r = RouterProvider::new(vec![p]);
        assert!(r.supports_streaming());
        assert!(r.supports_embeddings());
    }

    #[test]
    fn clone_preserves_providers() {
        use crate::ollama::OllamaProvider;

        let p = AnyProvider::Ollama(OllamaProvider::new(
            "http://127.0.0.1:1",
            "m".into(),
            "e".into(),
        ));
        let r = RouterProvider::new(vec![p]);
        let c = r.clone();
        assert_eq!(c.providers.len(), 1);
        assert_eq!(c.name(), "router");
    }

    #[test]
    fn last_cache_usage_returns_none() {
        let r = RouterProvider::new(vec![]);
        assert!(r.last_cache_usage().is_none());
    }

    #[test]
    fn thompson_strategy_is_set() {
        let r = RouterProvider::new(vec![]).with_thompson(None);
        assert_eq!(r.strategy, RouterStrategy::Thompson);
        assert!(r.thompson.is_some());
    }

    #[test]
    fn save_thompson_state_noop_without_thompson() {
        let r = RouterProvider::new(vec![]);
        r.save_thompson_state(); // should not panic
    }

    #[test]
    fn thompson_ordered_providers_empty() {
        let r = RouterProvider::new(vec![]).with_thompson(None);
        let ordered = r.ordered_providers();
        assert!(ordered.is_empty());
    }
}
