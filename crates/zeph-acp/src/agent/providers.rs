// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `providers/*` ext-method handlers for ACP sessions.
//!
//! Gated behind the `unstable-llm-providers` feature. Groups the connection-scoped provider
//! override type and the `providers/list`, `providers/set`, and `providers/disable` handlers so
//! the LLM-provider surface is isolated from the main agent dispatch in [`super`].

use std::collections::HashMap;

use agent_client_protocol as acp;

use super::ZephAcpAgentState;

/// Session-scoped provider configuration set via `providers/set`.
///
/// Overrides the global provider routing for one provider id within a single ACP session.
/// Vault-resolved API keys are never stored here — only the public routing fields.
pub(crate) struct ProviderSetOverride {
    /// Protocol type for this provider override.
    pub api_type: agent_client_protocol_schema::v1::LlmProtocol,
    /// Base URL for requests sent through this provider.
    pub base_url: String,
    /// Additional headers (e.g. routing headers, not auth secrets).
    #[allow(dead_code)]
    pub headers: HashMap<String, String>,
}

impl ZephAcpAgentState {
    /// Configure available providers for `providers/list`.
    ///
    /// Each entry is `(name, protocol)` where `name` matches a `[[llm.providers]]` entry
    /// and `protocol` is the wire type used to build the default `current` config.
    /// Vault-resolved API keys are never passed here.
    #[cfg_attr(docsrs, doc(cfg(feature = "unstable-llm-providers")))]
    #[must_use]
    pub fn with_provider_names(
        mut self,
        names: Vec<(String, agent_client_protocol_schema::v1::LlmProtocol)>,
    ) -> Self {
        self.provider_names = names;
        self
    }

    /// Dispatch `providers/*` ext methods.
    ///
    /// Returns `Some(ExtResponse)` when the method is handled, `None` when the caller should
    /// fall through to `ext_method_mcp`.
    pub(crate) fn ext_method_providers(
        &self,
        args: &acp::schema::v1::ExtRequest,
    ) -> acp::Result<Option<acp::schema::v1::ExtResponse>> {
        use agent_client_protocol_schema::v1 as schema;
        let method = args.method.as_ref();
        match method {
            "providers/list" => {
                let req: schema::ListProvidersRequest = serde_json::from_str(args.params.get())
                    .map_err(|e| acp::Error::invalid_request().data(e.to_string()))?;
                let resp = self.do_list_providers(req)?;
                let json = serde_json::to_string(&resp)
                    .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
                let raw = serde_json::value::RawValue::from_string(json)
                    .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
                Ok(Some(acp::schema::v1::ExtResponse::new(raw.into())))
            }
            "providers/set" => {
                let req: schema::SetProviderRequest = serde_json::from_str(args.params.get())
                    .map_err(|e| acp::Error::invalid_request().data(e.to_string()))?;
                let resp = self.do_set_providers(req)?;
                let json = serde_json::to_string(&resp)
                    .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
                let raw = serde_json::value::RawValue::from_string(json)
                    .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
                Ok(Some(acp::schema::v1::ExtResponse::new(raw.into())))
            }
            "providers/disable" => {
                let req: schema::DisableProviderRequest =
                    serde_json::from_str(args.params.get())
                        .map_err(|e| acp::Error::invalid_request().data(e.to_string()))?;
                let resp = self.do_disable_providers(req)?;
                let json = serde_json::to_string(&resp)
                    .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
                let raw = serde_json::value::RawValue::from_string(json)
                    .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
                Ok(Some(acp::schema::v1::ExtResponse::new(raw.into())))
            }
            _ => Ok(None),
        }
    }

    /// Handle `providers/list` — return all known providers without vault keys.
    ///
    /// # Errors
    ///
    /// Never fails; returns `Ok` in all cases.
    #[tracing::instrument(skip_all, name = "acp.handler.list_providers")]
    pub(crate) fn do_list_providers(
        &self,
        _req: agent_client_protocol_schema::v1::ListProvidersRequest,
    ) -> acp::Result<agent_client_protocol_schema::v1::ListProvidersResponse> {
        let disabled = self.global_disabled_providers.lock();
        let overrides = self.global_provider_overrides.lock();
        let providers: Vec<agent_client_protocol_schema::v1::ProviderInfo> = self
            .provider_names
            .iter()
            .map(|(name, protocol)| {
                let is_disabled = disabled.contains(name.as_str());
                let current = if is_disabled {
                    None
                } else if let Some(ov) = overrides.get(name.as_str()) {
                    Some(
                        agent_client_protocol_schema::v1::ProviderCurrentConfig::new(
                            ov.api_type.clone(),
                            ov.base_url.clone(),
                        ),
                    )
                } else {
                    // Default — no base_url exposed; provider is available via global config.
                    Some(
                        agent_client_protocol_schema::v1::ProviderCurrentConfig::new(
                            protocol.clone(),
                            String::new(),
                        ),
                    )
                };
                agent_client_protocol_schema::v1::ProviderInfo::new(
                    name.clone(),
                    vec![protocol.clone()],
                    false,
                    current,
                )
            })
            .collect();
        Ok(agent_client_protocol_schema::v1::ListProvidersResponse::new(providers))
    }

    /// Handle `providers/set` — store a connection-scoped provider override.
    ///
    /// The override is stored in global state (no `session_id` in the ACP schema) and
    /// takes effect on the next turn's provider resolution.
    ///
    /// # Errors
    ///
    /// Returns `invalid_params` if `req.id` is not in the registered provider list.
    #[tracing::instrument(skip_all, name = "acp.handler.set_providers")]
    pub(crate) fn do_set_providers(
        &self,
        req: agent_client_protocol_schema::v1::SetProviderRequest,
    ) -> acp::Result<agent_client_protocol_schema::v1::SetProviderResponse> {
        if !self.provider_names.iter().any(|(name, _)| name == &req.id) {
            return Err(
                acp::Error::invalid_params().data(format!("unknown provider id: {}", req.id))
            );
        }
        self.global_provider_overrides.lock().insert(
            req.id.clone(),
            ProviderSetOverride {
                api_type: req.api_type,
                base_url: req.base_url,
                headers: req.headers,
            },
        );
        tracing::debug!(provider_id = %req.id, "provider override set");
        Ok(agent_client_protocol_schema::v1::SetProviderResponse::new())
    }

    /// Handle `providers/disable` — mark a provider as disabled for this connection.
    ///
    /// Takes effect on the next turn's provider resolution.
    ///
    /// # Errors
    ///
    /// Always succeeds; returns `DisableProviderResponse`.
    #[tracing::instrument(skip_all, name = "acp.handler.disable_providers")]
    pub(crate) fn do_disable_providers(
        &self,
        req: agent_client_protocol_schema::v1::DisableProviderRequest,
    ) -> acp::Result<agent_client_protocol_schema::v1::DisableProviderResponse> {
        let id = req.id;
        tracing::debug!(provider_id = %id, "provider disabled");
        self.global_disabled_providers.lock().insert(id);
        Ok(agent_client_protocol_schema::v1::DisableProviderResponse::new())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agent_client_protocol_schema::v1 as schema;

    use crate::agent::{AgentSpawner, ZephAcpAgentState};

    fn make_state() -> ZephAcpAgentState {
        let spawner: AgentSpawner = Arc::new(|_ch, _ctx, _sc| Box::pin(async {}));
        ZephAcpAgentState::new(spawner, 4, 1800, None).with_provider_names(vec![
            ("openai".to_owned(), schema::LlmProtocol::OpenAi),
            ("claude".to_owned(), schema::LlmProtocol::Anthropic),
        ])
    }

    #[test]
    fn list_providers_returns_all_registered() {
        let state = make_state();
        let resp = state
            .do_list_providers(schema::ListProvidersRequest::new())
            .unwrap();
        assert_eq!(resp.providers.len(), 2);
        let ids: Vec<&str> = resp.providers.iter().map(|p| p.id.as_str()).collect();
        assert!(ids.contains(&"openai"));
        assert!(ids.contains(&"claude"));
    }

    #[test]
    fn list_providers_empty_when_none_registered() {
        let spawner: AgentSpawner = Arc::new(|_ch, _ctx, _sc| Box::pin(async {}));
        let state = ZephAcpAgentState::new(spawner, 4, 1800, None).with_provider_names(vec![]);
        let resp = state
            .do_list_providers(schema::ListProvidersRequest::new())
            .unwrap();
        assert!(resp.providers.is_empty());
    }

    #[test]
    fn protocol_type_reflected_in_default_current_config() {
        let state = make_state();
        let resp = state
            .do_list_providers(schema::ListProvidersRequest::new())
            .unwrap();
        let openai = resp.providers.iter().find(|p| p.id == "openai").unwrap();
        let current = openai
            .current
            .as_ref()
            .expect("openai must have current config");
        assert_eq!(
            current.api_type,
            schema::LlmProtocol::OpenAi,
            "openai provider must report OpenAi protocol"
        );
        let claude = resp.providers.iter().find(|p| p.id == "claude").unwrap();
        let current = claude
            .current
            .as_ref()
            .expect("claude must have current config");
        assert_eq!(
            current.api_type,
            schema::LlmProtocol::Anthropic,
            "claude provider must report Anthropic protocol"
        );
    }

    #[test]
    fn disable_provider_hides_current_config_in_list() {
        let state = make_state();
        state
            .do_disable_providers(schema::DisableProviderRequest::new("openai"))
            .unwrap();
        let resp = state
            .do_list_providers(schema::ListProvidersRequest::new())
            .unwrap();
        let openai = resp.providers.iter().find(|p| p.id == "openai").unwrap();
        assert!(
            openai.current.is_none(),
            "disabled provider must have no current config"
        );
        let claude = resp.providers.iter().find(|p| p.id == "claude").unwrap();
        assert!(
            claude.current.is_some(),
            "non-disabled provider must still have current config"
        );
    }

    #[test]
    fn disable_unknown_provider_succeeds() {
        let state = make_state();
        state
            .do_disable_providers(schema::DisableProviderRequest::new("nonexistent"))
            .unwrap();
    }

    #[test]
    fn set_provider_unknown_id_returns_error() {
        let state = make_state();
        let err = state
            .do_set_providers(
                schema::SetProviderRequest::new(
                    "unknown_provider",
                    schema::LlmProtocol::OpenAi,
                    "https://evil.example.com",
                )
                .headers(std::collections::HashMap::new()),
            )
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown provider id"),
            "expected 'unknown provider id' in error, got: {msg}"
        );
    }

    #[test]
    fn set_provider_override_appears_in_list() {
        let state = make_state();
        state
            .do_set_providers(
                schema::SetProviderRequest::new(
                    "openai",
                    schema::LlmProtocol::OpenAi,
                    "https://custom.example.com",
                )
                .headers(std::collections::HashMap::new()),
            )
            .unwrap();
        let resp = state
            .do_list_providers(schema::ListProvidersRequest::new())
            .unwrap();
        let openai = resp.providers.iter().find(|p| p.id == "openai").unwrap();
        let current = openai.current.as_ref().expect("override must be present");
        assert_eq!(current.base_url, "https://custom.example.com");
    }

    #[test]
    fn disable_after_set_clears_current_config() {
        let state = make_state();
        state
            .do_set_providers(
                schema::SetProviderRequest::new(
                    "openai",
                    schema::LlmProtocol::OpenAi,
                    "https://custom.example.com",
                )
                .headers(std::collections::HashMap::new()),
            )
            .unwrap();
        state
            .do_disable_providers(schema::DisableProviderRequest::new("openai"))
            .unwrap();
        let resp = state
            .do_list_providers(schema::ListProvidersRequest::new())
            .unwrap();
        let openai = resp.providers.iter().find(|p| p.id == "openai").unwrap();
        assert!(
            openai.current.is_none(),
            "provider disabled after set must have no current config"
        );
    }
}
