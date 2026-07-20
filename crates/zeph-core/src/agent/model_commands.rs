// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;

use zeph_commands::ModelAccess;
use zeph_llm::provider::LlmProvider;

use super::Agent;
use crate::channel::Channel;

impl<C: crate::channel::Channel> Agent<C> {
    /// Switch the active provider to one serving `model_id`.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the model is not found.
    pub(crate) fn set_model(&mut self, model_id: &str) -> Result<(), String> {
        if model_id.is_empty() {
            return Err("model id must not be empty".to_string());
        }
        if model_id.len() > 256 {
            return Err("model id exceeds maximum length of 256 characters".to_string());
        }
        if !model_id
            .chars()
            .all(|c| c.is_ascii() && !c.is_ascii_control())
        {
            return Err("model id must contain only printable ASCII characters".to_string());
        }
        self.runtime.config.model_name = model_id.to_string();
        tracing::info!(model = model_id, "set_model called");
        Ok(())
    }

    /// Refresh the remote model cache, then return a result message.
    pub(crate) async fn model_refresh_as_string(&mut self) -> String {
        if let Some(cache_dir) = dirs::cache_dir() {
            let models_dir = cache_dir.join("zeph").join("models");
            let _ = tokio::task::spawn_blocking(move || {
                if let Ok(entries) = std::fs::read_dir(&models_dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.extension().and_then(|e| e.to_str()) == Some("json") {
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                }
            })
            .await;
        }
        match self.provider.list_models_remote().await {
            Ok(models) => format!("Fetched {} models.", models.len()),
            Err(e) => format!("Error fetching models: {e}"),
        }
    }

    /// List available models, returning a formatted string.
    pub(crate) async fn model_list_as_string(&mut self) -> String {
        let cache = zeph_llm::model_cache::ModelCache::for_slug(self.provider.name());
        let cached = if cache.is_stale_async().await {
            None
        } else {
            cache.load_async().await.unwrap_or(None)
        };
        let models = if let Some(m) = cached {
            m
        } else {
            match self.provider.list_models_remote().await {
                Ok(m) => m,
                Err(e) => return format!("Error fetching models: {e}"),
            }
        };
        if models.is_empty() {
            return "No models available.".to_owned();
        }
        let mut lines = vec!["Available models:".to_string()];
        for (i, m) in models.iter().enumerate() {
            lines.push(format!("  {}. {} ({})", i + 1, m.display_name, m.id));
        }
        lines.join("\n")
    }

    /// Switch to a different model, returning a result message.
    pub(crate) async fn model_switch_as_string(&mut self, model_id: &str) -> String {
        let cache = zeph_llm::model_cache::ModelCache::for_slug(self.provider.name());
        let known_models: Option<Vec<zeph_llm::model_cache::RemoteModelInfo>> =
            if cache.is_stale_async().await {
                match self.provider.list_models_remote().await {
                    Ok(m) if !m.is_empty() => Some(m),
                    _ => None,
                }
            } else {
                cache.load_async().await.unwrap_or(None)
            };
        let list_unavailable = known_models.is_none();
        if let Some(models) = known_models {
            if !models.iter().any(|m| m.id == model_id) {
                let mut lines = vec![format!("Unknown model '{model_id}'. Available models:")];
                for m in &models {
                    lines.push(format!("  • {} ({})", m.display_name, m.id));
                }
                return lines.join("\n");
            }
        } else {
            // Model list unavailable — proceed with a warning.
            tracing::warn!("model list unavailable, switching to '{model_id}' without validation");
        }
        match self.set_model(model_id) {
            Ok(()) => {
                let switch_msg = format!("Switched to model: {model_id}");
                if list_unavailable {
                    format!(
                        "Model list unavailable, switching anyway — verify your model name is correct.\n{switch_msg}"
                    )
                } else {
                    switch_msg
                }
            }
            Err(e) => format!("Error: {e}"),
        }
    }

    /// Handle `/model`, `/model <id>`, and `/model refresh` commands, returning a string result.
    pub(crate) async fn handle_model_command_as_string(&mut self, trimmed: &str) -> String {
        let arg = trimmed.strip_prefix("/model").map_or("", str::trim);
        if arg == "refresh" {
            self.model_refresh_as_string().await
        } else if arg.is_empty() {
            self.model_list_as_string().await
        } else {
            self.model_switch_as_string(arg).await
        }
    }
}

impl<C: Channel + Send + 'static> ModelAccess for Agent<C> {
    // ----- /caveman -----

    fn handle_caveman<'a>(
        &'a mut self,
        arg: &'a str,
    ) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
        Box::pin(async move {
            let active = &mut self.services.session.caveman_active;
            match arg.trim() {
                "on" | "enable" => {
                    *active = true;
                    "caveman: on".to_owned()
                }
                "off" | "disable" => {
                    *active = false;
                    "caveman: off".to_owned()
                }
                "status" => {
                    if *active {
                        "caveman: on".to_owned()
                    } else {
                        "caveman: off".to_owned()
                    }
                }
                _ => {
                    *active = !*active;
                    if *active {
                        "caveman: on".to_owned()
                    } else {
                        "caveman: off".to_owned()
                    }
                }
            }
        })
    }

    // ----- /model, /provider -----

    fn handle_model<'a>(
        &'a mut self,
        arg: &'a str,
    ) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
        Box::pin(async move {
            let input = if arg.is_empty() {
                "/model".to_owned()
            } else {
                format!("/model {arg}")
            };
            self.handle_model_command_as_string(&input).await
        })
    }

    fn handle_provider<'a>(
        &'a mut self,
        arg: &'a str,
    ) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
        Box::pin(async move { self.handle_provider_command_as_string(arg).await })
    }

    // ----- /think-tokens, /reasoning-effort -----

    fn handle_think_tokens<'a>(
        &'a mut self,
        arg: &'a str,
    ) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
        Box::pin(async move {
            let arg = arg.trim();
            let provider_name = self.provider.name().to_owned();
            if arg.is_empty() {
                return match self.provider.current_thinking_budget() {
                    Some(n) => format!("think-tokens: {n} (provider: {provider_name})"),
                    None => format!("think-tokens: off (provider: {provider_name})"),
                };
            }

            let budget = match zeph_commands::handlers::think_tokens::parse_token_budget(arg) {
                Ok(b) => b,
                Err(e) => return format!("think-tokens: {e}"),
            };

            // Captured before the mutation so the cross-override note (Claude's Extended and
            // Adaptive thinking share one config field) only fires when this call actually
            // cleared a previously active reasoning-effort level.
            let had_reasoning_effort = self.provider.current_reasoning_effort().is_some();
            match self.provider.set_thinking_budget(budget) {
                Ok(()) => {
                    let mut msg = match budget {
                        Some(n) => format!("think-tokens: set to {n} (provider: {provider_name})"),
                        None => format!("think-tokens: disabled (provider: {provider_name})"),
                    };
                    if had_reasoning_effort && self.provider.current_reasoning_effort().is_none() {
                        msg.push_str(
                            " Note: this overrides the previously set reasoning-effort level \
                             — Claude's Extended and Adaptive thinking share one config field.",
                        );
                    }
                    if let Some(advisory) = self.provider.capability_delegation_advisory() {
                        let _ = write!(msg, " Note: {advisory}.");
                    }
                    msg
                }
                Err(zeph_llm::LlmError::ModelCapabilityMismatch { provider, message }) => {
                    format!("provider `{provider}` {message}")
                }
                Err(e) => format!("think-tokens: {e}"),
            }
        })
    }

    fn handle_reasoning_effort<'a>(
        &'a mut self,
        arg: &'a str,
    ) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
        Box::pin(async move {
            let arg = arg.trim();
            let provider_name = self.provider.name().to_owned();
            if arg.is_empty() {
                return match self.provider.current_reasoning_effort() {
                    Some(e) => format!("reasoning-effort: {e} (provider: {provider_name})"),
                    None => format!("reasoning-effort: off (provider: {provider_name})"),
                };
            }

            let effort: zeph_llm::any::ReasoningEffort = match arg.parse() {
                Ok(e) => e,
                Err(e) => return format!("reasoning-effort: {e}"),
            };

            // Captured before the mutation — see the matching comment in handle_think_tokens.
            let had_thinking_budget = self.provider.current_thinking_budget().is_some();
            match self.provider.apply_reasoning_effort(effort) {
                Ok(()) => {
                    let mut msg = format!(
                        "reasoning-effort: set to {} (provider: {provider_name})",
                        effort.as_str()
                    );
                    if had_thinking_budget && self.provider.current_thinking_budget().is_none() {
                        msg.push_str(
                            " Note: this overrides the previously set thinking-token budget \
                             — Claude's Extended and Adaptive thinking share one config field.",
                        );
                    }
                    if let Some(advisory) = self.provider.capability_delegation_advisory() {
                        let _ = write!(msg, " Note: {advisory}.");
                    }
                    msg
                }
                Err(zeph_llm::LlmError::ModelCapabilityMismatch { provider, message }) => {
                    format!("provider `{provider}` {message}")
                }
                Err(e) => format!("reasoning-effort: {e}"),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use super::*;

    // ── /think-tokens, /reasoning-effort (#3098) ─────────────────────────

    fn claude_agent() -> Agent<MockChannel> {
        let provider = zeph_llm::any::AnyProvider::Claude(zeph_llm::claude::ClaudeProvider::new(
            "key".into(),
            "claude-sonnet-5".into(),
            4096,
        ));
        Agent::new(
            provider,
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
    }

    #[tokio::test]
    async fn handle_think_tokens_empty_arg_displays_off_by_default() {
        let mut agent = claude_agent();
        let out = agent.handle_think_tokens("").await;
        assert!(out.contains("off"), "{out}");
        assert!(out.contains("claude"), "{out}");
    }

    #[tokio::test]
    async fn handle_think_tokens_sets_and_displays_budget() {
        let mut agent = claude_agent();
        let set = agent.handle_think_tokens("8k").await;
        assert!(set.contains("8000"), "{set}");

        let show = agent.handle_think_tokens("").await;
        assert!(show.contains("8000"), "{show}");
    }

    #[tokio::test]
    async fn handle_think_tokens_off_disables() {
        let mut agent = claude_agent();
        agent.handle_think_tokens("8k").await;
        let out = agent.handle_think_tokens("off").await;
        assert!(out.contains("disabled"), "{out}");
        assert!(agent.provider.current_thinking_budget().is_none());
    }

    #[tokio::test]
    async fn handle_think_tokens_invalid_parse_returns_error_no_mutation() {
        let mut agent = claude_agent();
        let out = agent.handle_think_tokens("1.2.3k").await;
        assert!(out.contains("think-tokens"), "{out}");
        assert!(agent.provider.current_thinking_budget().is_none());
    }

    #[tokio::test]
    async fn handle_think_tokens_unsupported_provider_returns_explicit_message() {
        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        let out = agent.handle_think_tokens("8k").await;
        assert!(out.contains("does not support"), "{out}");
        assert!(out.contains("mock"), "{out}");
    }

    #[tokio::test]
    async fn handle_think_tokens_cross_override_note_when_reasoning_effort_was_active() {
        let mut agent = claude_agent();
        agent.handle_reasoning_effort("high").await;
        let out = agent.handle_think_tokens("8k").await;
        assert!(
            out.contains("overrides the previously set reasoning-effort"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn handle_reasoning_effort_empty_arg_displays_off_by_default() {
        let mut agent = claude_agent();
        let out = agent.handle_reasoning_effort("").await;
        assert!(out.contains("off"), "{out}");
    }

    #[tokio::test]
    async fn handle_reasoning_effort_sets_and_displays_level() {
        let mut agent = claude_agent();
        let set = agent.handle_reasoning_effort("high").await;
        assert!(set.contains("high"), "{set}");

        let show = agent.handle_reasoning_effort("").await;
        assert!(show.contains("high"), "{show}");
    }

    #[tokio::test]
    async fn handle_reasoning_effort_invalid_parse_returns_error_no_mutation() {
        let mut agent = claude_agent();
        let out = agent.handle_reasoning_effort("minimal").await;
        assert!(out.contains("reasoning-effort"), "{out}");
        assert!(agent.provider.current_reasoning_effort().is_none());
    }

    #[tokio::test]
    async fn handle_reasoning_effort_unsupported_provider_returns_explicit_message() {
        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        let out = agent.handle_reasoning_effort("high").await;
        assert!(out.contains("does not support"), "{out}");
    }

    #[tokio::test]
    async fn handle_reasoning_effort_cross_override_note_when_think_tokens_was_active() {
        let mut agent = claude_agent();
        agent.handle_think_tokens("8k").await;
        let out = agent.handle_reasoning_effort("high").await;
        assert!(
            out.contains("overrides the previously set thinking-token budget"),
            "{out}"
        );
    }
}
