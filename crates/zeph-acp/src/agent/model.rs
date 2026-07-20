// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Model resolution and provider-construction methods for `ZephAcpAgentState`.
//!
//! Groups fuzzy model-name resolution, remote model-cache refresh, and provider
//! construction (with sampling-temperature overrides applied) so the model-switching
//! surface is isolated from slash-command dispatch and session lifecycle in [`super`].

use std::sync::Arc;

use parking_lot::RwLock;

use agent_client_protocol as acp;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::GenerationOverrides;

use super::{ZephAcpAgentState, warm_model_caches};

impl ZephAcpAgentState {
    /// Build a provider for `model_key` with `preset`'s sampling temperature applied.
    ///
    /// Shared by the `model` and `temperature` `model_config` config options so switching
    /// either one preserves the other's current setting.
    pub(crate) fn provider_with_temperature(
        &self,
        model_key: &str,
        preset: zeph_config::AcpTemperaturePreset,
    ) -> acp::Result<AnyProvider> {
        let Some(ref factory) = self.provider_factory else {
            return Err(acp::Error::internal_error().data("model switching not configured"));
        };
        let Some(provider) = factory(model_key) else {
            return Err(acp::Error::invalid_request().data("unknown model"));
        };
        Ok(provider.with_generation_overrides(GenerationOverrides {
            temperature: Some(preset.temperature()),
            ..Default::default()
        }))
    }

    /// Prime a freshly created session's `provider_override` with `temperature_preset`, so
    /// that preset is the *effective* sampling temperature from the session's very first
    /// prompt — not just the value advertised in the IDE dropdown until an explicit
    /// `session/set_config_option` call. Callers pass the configured
    /// `[acp.model_config].default_temperature_preset` for new/loaded sessions, or a preset
    /// inherited from a source session for fork/resume (#5373).
    ///
    /// No-op (leaves `provider_override` as `None`, falling back to the spawner's own
    /// provider) when model switching isn't configured (`provider_factory` unset) or
    /// `initial_model` doesn't resolve to a known provider — mirrors
    /// `provider_with_temperature`'s error cases, which are expected outside model-switching
    /// setups.
    pub(crate) fn prime_provider_override(
        &self,
        provider_override: &Arc<RwLock<Option<AnyProvider>>>,
        initial_model: &str,
        temperature_preset: zeph_config::AcpTemperaturePreset,
    ) {
        if let Ok(provider) = self.provider_with_temperature(initial_model, temperature_preset) {
            *provider_override.write() = Some(provider);
        }
    }

    pub(crate) fn resolve_model_fuzzy(&self, query: &str) -> acp::Result<String> {
        let available_models = self.available_models_snapshot();
        if available_models.iter().any(|m| m == query) {
            return Ok(query.to_owned());
        }
        let tokens: Vec<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(String::from)
            .collect();
        let candidates: Vec<&String> = available_models
            .iter()
            .filter(|m| {
                let lower = m.to_lowercase();
                tokens.iter().all(|t| lower.contains(t.as_str()))
            })
            .collect();
        match candidates.len() {
            0 => {
                let models = available_models.join(", ");
                Err(acp::Error::invalid_request()
                    .data(format!("no matching model found. Available: {models}")))
            }
            1 => Ok(candidates[0].clone()),
            _ => {
                let names: Vec<&str> = candidates.iter().map(|s| s.as_str()).collect();
                Err(acp::Error::invalid_request()
                    .data(format!("ambiguous model, candidates: {}", names.join(", "))))
            }
        }
    }

    /// Refresh the remote model cache for the session's currently active provider, then update
    /// the advertised `available_models` list.
    ///
    /// Mirrors `Agent::model_refresh_as_string` (`crates/zeph-core/src/agent/model_commands.rs`,
    /// the CLI/TUI `/model refresh` handler), which likewise refreshes only the single active
    /// provider, not every configured one. Reuses the shared [`warm_model_caches`] helper
    /// (`src/acp.rs`'s ACP-startup cache warm-up) instead of a bespoke per-provider network loop
    /// — a prior version of this method looped sequentially over every configured provider with
    /// an independent 5-second timeout each, which could block this session's `do_prompt` handler
    /// for up to 5s × N providers (#5986 critic finding M1).
    pub(crate) async fn model_refresh_as_string(
        &self,
        session_id: &acp::schema::v1::SessionId,
    ) -> String {
        let Some(ref factory) = self.provider_factory else {
            return "model switching not configured".to_owned();
        };
        let current_model = {
            let sessions = self.sessions.lock();
            let Some(entry) = sessions.get(session_id) else {
                return "session not found".to_owned();
            };
            entry.current_model.lock().clone()
        };
        let Some(provider) = factory(&current_model) else {
            return format!("unknown model: {current_model}");
        };
        let fetched = warm_model_caches(provider, self.available_models.clone()).await;
        format!("Fetched {fetched} models.")
    }
}
