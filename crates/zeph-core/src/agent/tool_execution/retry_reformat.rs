// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Retry and parameter-reformat post-dispatch phases.
//!
//! Covers transient-error retry with backoff (`handle_retry_phase`) and LLM-based
//! parameter-reformat-and-retry for `InvalidParameters`/`TypeMismatch` failures
//! (`handle_reformat_phase`, `reformat_tool_call`, issue #5453). Split out of `tier_loop.rs` —
//! see that module for the orchestration entry point that calls into these phases.

use tracing::Instrument;
use zeph_llm::provider::{Message, Role};
use zeph_tools::executor::ToolCall;

use super::retry_backoff_ms;
use crate::agent::Agent;
use crate::channel::Channel;

/// Per-call timeout for the parameter-reformat LLM call (#5453) when
/// `tools.retry.max_retry_duration_secs` is `0` ("no phase budget limit") — that value must not
/// be reused directly as a per-call timeout (critic finding M1: `max(1)` on `0` collapsed to a
/// 1-second timeout that always failed, silently disabling the whole feature).
const REFORMAT_DEFAULT_TIMEOUT_SECS: u64 = 30;

impl<C: Channel> Agent<C> {
    /// Returns `Ok(true)` if the user cancelled the turn during this phase.
    #[tracing::instrument(name = "core.tool.handle_retry_phase", skip_all, level = "debug", err)]
    pub(super) async fn handle_retry_phase(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        calls: &[ToolCall],
        tool_results: &mut [Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>],
        max_retries: usize,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<bool, crate::agent::error::AgentError> {
        if max_retries == 0 {
            return Ok(false);
        }
        let max_retry_duration_secs = self.tool_orchestrator.max_retry_duration_secs;
        let retry_base_ms = self.tool_orchestrator.retry_base_ms;
        let retry_max_ms = self.tool_orchestrator.retry_max_ms;
        for idx in 0..tool_results.len() {
            if cancel.is_cancelled() {
                self.cancel_tool_batch(tool_calls, "tool execution cancelled by user")
                    .await?;
                return Ok(true);
            }
            let is_transient = matches!(
                tool_results[idx],
                Err(ref e) if e.kind() == zeph_tools::ErrorKind::Transient
            );
            if !is_transient {
                continue;
            }
            let tc = &tool_calls[idx];
            if !self
                .tool_executor
                .is_tool_retryable_erased(tc.name.as_str())
            {
                continue;
            }
            let call = &calls[idx];
            let mut attempt = 0_usize;
            let retry_start = std::time::Instant::now();
            let result = loop {
                let exec_result = tokio::select! {
                    r = self.tool_executor.execute_tool_call_erased(call).instrument(
                        tracing::info_span!("tool_exec_retry", tool_name = %tc.name, idx = %tc.id)
                    ) => r,
                    () = cancel.cancelled() => {
                        self.cancel_tool_batch(tool_calls, "tool retry cancelled by user")
                            .await?;
                        return Ok(true);
                    }
                };
                match exec_result {
                    Err(ref e)
                        if e.kind() == zeph_tools::ErrorKind::Transient
                            && attempt < max_retries =>
                    {
                        let elapsed_secs = retry_start.elapsed().as_secs();
                        if max_retry_duration_secs > 0 && elapsed_secs >= max_retry_duration_secs {
                            tracing::warn!(
                                tool = %tc.name, elapsed_secs, max_retry_duration_secs,
                                "tool retry budget exceeded, aborting retries"
                            );
                            break exec_result;
                        }
                        attempt += 1;
                        let delay_ms = retry_backoff_ms(attempt - 1, retry_base_ms, retry_max_ms);
                        tracing::warn!(
                            tool = %tc.name, attempt, delay_ms, error = %e,
                            "transient tool error, retrying with backoff"
                        );
                        self.channel
                            .send_status_best_effort(&format!("Retrying {}...", tc.name))
                            .await;
                        // Interruptible backoff sleep: cancelled if agent shuts down.
                        tokio::select! {
                            () = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {}
                            () = cancel.cancelled() => {
                                self.cancel_tool_batch(
                                    tool_calls,
                                    "retry backoff interrupted by cancellation",
                                )
                                .await?;
                                return Ok(true);
                            }
                        }
                        self.channel.send_status_best_effort("").await;
                        // NOTE: retry re-executions are NOT recorded in repeat-detection (CRIT-3).
                    }
                    result => break result,
                }
            };
            tool_results[idx] = result;
        }
        Ok(false)
    }

    /// Returns `Ok(true)` if the user cancelled the turn during this phase.
    #[tracing::instrument(
        name = "core.tool.handle_reformat_phase",
        skip_all,
        level = "debug",
        err
    )]
    pub(super) async fn handle_reformat_phase(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        calls: &[ToolCall],
        tool_results: &mut [Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>],
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<bool, crate::agent::error::AgentError> {
        if self
            .tool_orchestrator
            .parameter_reformat_provider
            .is_empty()
        {
            return Ok(false);
        }
        // Budget covers the whole reformat phase (all tool calls needing reformat this round),
        // matching the retry phase's `retry_start`/`max_retry_duration_secs` accounting.
        let budget_secs = self.tool_orchestrator.max_retry_duration_secs;
        let reformat_start = std::time::Instant::now();
        for idx in 0..tool_results.len() {
            if cancel.is_cancelled() {
                self.cancel_tool_batch(tool_calls, "parameter reformat phase cancelled by user")
                    .await?;
                return Ok(true);
            }
            let needs_reformat = matches!(
                tool_results[idx],
                Err(ref e) if e.category().needs_parameter_reformat()
            );
            if !needs_reformat {
                continue;
            }
            let tc = &tool_calls[idx];
            if budget_secs > 0 && reformat_start.elapsed().as_secs() >= budget_secs {
                tracing::warn!(tool = %tc.name, "parameter reformat budget exhausted, skipping");
                continue;
            }
            let error_message = tool_results[idx]
                .as_ref()
                .err()
                .map(std::string::ToString::to_string)
                .unwrap_or_default();

            self.channel
                .send_status_best_effort(&format!("Reformatting parameters for {}...", tc.name))
                .await;
            let new_result = self
                .reformat_tool_call(&calls[idx], tc, &error_message, cancel)
                .await;
            self.channel.send_status_best_effort("").await;

            if cancel.is_cancelled() {
                self.cancel_tool_batch(tool_calls, "parameter reformat phase cancelled by user")
                    .await?;
                return Ok(true);
            }

            if let Some(result) = new_result {
                if let Err(ref e) = result
                    && let Some(ref d) = self.runtime.debug.debug_dumper
                {
                    d.dump_tool_error(tc.name.as_str(), e);
                }
                tool_results[idx] = result;
            }
        }
        Ok(false)
    }

    /// LLM-based single-shot reformat-and-retry for a tool call that failed with an
    /// `InvalidParameters`/`TypeMismatch` error (issue #5453).
    ///
    /// Resolves `tools.retry.parameter_reformat_provider` from `[[llm.providers]]` via
    /// [`Agent::resolve_pool_entry_provider`]. Unlike most other background-provider call sites
    /// (which use [`Agent::resolve_background_provider`] and fall back to the primary provider
    /// on any resolution failure), a *configured* name — one the provider-pool registry was
    /// actually wired to recognize — must not silently substitute the primary provider when it
    /// fails to resolve: that would mask the original tool error behind a "corrected" call made
    /// with the wrong model (#5600, #5478). This method no-ops instead (keeps the original tool
    /// error) whenever the registry is wired but the name is absent from `provider_pool`, the
    /// matched entry fails to build, or no `provider_config_snapshot` is available. It only
    /// falls back to [`Agent::resolve_background_provider`]'s legacy convention when the
    /// provider-pool registry itself was never wired for this `Agent` at all — which
    /// `zeph_config::providers::validate_pool` guarantees cannot happen for a real, fully
    /// constructed production agent (see [`Agent::resolve_pool_entry_provider`]'s doc comment).
    ///
    /// Returns `None` when the provider is unresolvable, the provider call fails, times out, or
    /// returns arguments that do not parse as a JSON object — the caller keeps the original error
    /// result unchanged.
    async fn reformat_tool_call(
        &mut self,
        call: &ToolCall,
        tc: &zeph_llm::provider::ToolUseRequest,
        error_message: &str,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Option<Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>> {
        #[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
        struct ReformattedArguments {
            /// Corrected JSON arguments object for the failed tool call.
            arguments: serde_json::Value,
        }

        let Some(schema) = self
            .tool_executor
            .tool_definitions_erased()
            .into_iter()
            .find(|d| d.id.as_ref() == tc.name.as_str())
            .map(|d| d.schema)
        else {
            tracing::warn!(tool = %tc.name, "parameter reformat: tool schema not found, skipping");
            return None;
        };

        let provider_name = self.tool_orchestrator.parameter_reformat_provider.clone();
        let provider = match self.resolve_pool_entry_provider(&provider_name) {
            super::super::learning::PoolProviderResolution::Resolved(p) => *p,
            super::super::learning::PoolProviderResolution::RegistryNotWired => {
                self.resolve_background_provider(&provider_name)
            }
            super::super::learning::PoolProviderResolution::Unresolvable => {
                tracing::warn!(
                    tool = %tc.name,
                    provider = %provider_name,
                    "parameter reformat: configured provider unresolvable, keeping original error"
                );
                return None;
            }
        };

        let original_args = serde_json::Value::Object(call.params.clone());
        let prompt = format!(
            "A tool call failed parameter validation. Propose corrected arguments as a JSON \
             object under the `arguments` key.\n\n\
             Tool: {}\nJSON schema:\n{}\n\nOriginal arguments:\n{}\n\nError: {error_message}",
            tc.name,
            serde_json::to_string_pretty(&schema).unwrap_or_default(),
            serde_json::to_string_pretty(&original_args).unwrap_or_default(),
        );
        let messages = [Message::from_legacy(Role::User, prompt)];

        // `max_retry_duration_secs == 0` means "no phase budget limit" (see the `budget_secs > 0`
        // check in `handle_reformat_phase`), but a per-LLM-call timeout must never be 0 or
        // collapse to 1s for that same value — that previously made `max_retry_duration_secs = 0`
        // ("unlimited") silently disable the whole feature via an always-timing-out 1s call
        // (critic finding M1). Use the configured budget as the per-call timeout only when it is a
        // real bound; fall back to a fixed sane default otherwise.
        let timeout_secs = match self.tool_orchestrator.max_retry_duration_secs {
            0 => REFORMAT_DEFAULT_TIMEOUT_SECS,
            secs => secs,
        };
        let reformat = tokio::select! {
            r = tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                provider.chat_typed_erased::<ReformattedArguments>(&messages),
            ) => match r {
                Ok(Ok(reformat)) => reformat,
                Ok(Err(e)) => {
                    tracing::warn!(tool = %tc.name, error = %e, "parameter reformat: provider call failed");
                    return None;
                }
                Err(_) => {
                    tracing::warn!(tool = %tc.name, timeout_secs, "parameter reformat: provider call timed out");
                    return None;
                }
            },
            () = cancel.cancelled() => return None,
        };

        let serde_json::Value::Object(corrected_params) = reformat.arguments else {
            tracing::warn!(
                tool = %tc.name,
                "parameter reformat: corrected arguments were not a JSON object, skipping retry"
            );
            return None;
        };

        let mut retry_call = call.clone();
        retry_call.params = corrected_params;

        tokio::select! {
            r = self.tool_executor.execute_tool_call_erased(&retry_call).instrument(
                tracing::info_span!("tool_exec_reformat", tool_name = %tc.name)
            ) => Some(r),
            () = cancel.cancelled() => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod reformat_phase_tests {
        use zeph_tools::registry::{InvocationHint, ToolDef};

        use super::*;
        use crate::agent::agent_tests::*;

        fn test_tool_def() -> ToolDef {
            ToolDef {
                id: "test_tool".into(),
                description: "a test tool".into(),
                schema: schemars::Schema::default(),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
                server_id: None,
            }
        }

        fn invalid_params_error() -> zeph_tools::ToolError {
            zeph_tools::ToolError::InvalidParams {
                message: "path must be a string".to_owned(),
            }
        }

        fn bad_tool_call() -> ToolCall {
            let mut params = serde_json::Map::new();
            params.insert("path".to_owned(), serde_json::json!(123));
            ToolCall {
                tool_id: zeph_common::ToolName::new("test_tool"),
                params,
                ..Default::default()
            }
        }

        fn tool_use_request() -> zeph_llm::provider::ToolUseRequest {
            zeph_llm::provider::ToolUseRequest {
                id: "call-1".to_owned(),
                name: "test_tool".to_owned().into(),
                input: serde_json::json!({"path": 123}),
            }
        }

        #[tokio::test]
        async fn retries_with_corrected_arguments_on_success() {
            let provider = mock_provider(vec![r#"{"arguments":{"path":"/corrected"}}"#.into()]);
            let channel = MockChannel::new(vec![]);
            let registry = create_test_registry();
            let executor = MockToolExecutor::new(vec![Ok(Some(zeph_tools::ToolOutput {
                tool_name: "test_tool".to_owned().into(),
                summary: "done".to_owned(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
                ..Default::default()
            }))])
            .with_definitions(vec![test_tool_def()]);
            let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
            agent.tool_orchestrator.parameter_reformat_provider = "fast".to_owned();

            let tool_calls = vec![tool_use_request()];
            let calls = vec![bad_tool_call()];
            let mut tool_results: Vec<
                Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
            > = vec![Err(invalid_params_error())];
            let cancel = tokio_util::sync::CancellationToken::new();

            agent
                .handle_reformat_phase(&tool_calls, &calls, &mut tool_results, &cancel)
                .await
                .unwrap();

            let output = tool_results
                .remove(0)
                .expect("reformat retry should succeed")
                .expect("tool output should be present");
            assert_eq!(output.summary, "done");
        }

        #[tokio::test]
        async fn keeps_original_error_when_provider_returns_malformed_json() {
            let provider = mock_provider(vec!["not json at all".into()]);
            let channel = MockChannel::new(vec![]);
            let registry = create_test_registry();
            let executor =
                MockToolExecutor::new(vec![Ok(None)]).with_definitions(vec![test_tool_def()]);
            let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
            agent.tool_orchestrator.parameter_reformat_provider = "fast".to_owned();

            let tool_calls = vec![tool_use_request()];
            let calls = vec![bad_tool_call()];
            let mut tool_results: Vec<
                Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
            > = vec![Err(invalid_params_error())];
            let cancel = tokio_util::sync::CancellationToken::new();

            agent
                .handle_reformat_phase(&tool_calls, &calls, &mut tool_results, &cancel)
                .await
                .unwrap();

            assert!(
                matches!(
                    tool_results[0],
                    Err(zeph_tools::ToolError::InvalidParams { .. })
                ),
                "original error must be preserved on parse failure"
            );
        }

        #[tokio::test]
        async fn keeps_original_error_when_tool_schema_unknown() {
            let provider = mock_provider(vec![r#"{"arguments":{"path":"/corrected"}}"#.into()]);
            let channel = MockChannel::new(vec![]);
            let registry = create_test_registry();
            // No `with_definitions` — schema lookup for "test_tool" fails.
            let executor = MockToolExecutor::new(vec![Ok(None)]);
            let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
            agent.tool_orchestrator.parameter_reformat_provider = "fast".to_owned();

            let tool_calls = vec![tool_use_request()];
            let calls = vec![bad_tool_call()];
            let mut tool_results: Vec<
                Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
            > = vec![Err(invalid_params_error())];
            let cancel = tokio_util::sync::CancellationToken::new();

            agent
                .handle_reformat_phase(&tool_calls, &calls, &mut tool_results, &cancel)
                .await
                .unwrap();

            assert!(
                matches!(
                    tool_results[0],
                    Err(zeph_tools::ToolError::InvalidParams { .. })
                ),
                "original error must be preserved when schema is unknown"
            );
        }

        #[tokio::test]
        async fn is_a_noop_when_provider_not_configured() {
            let provider = mock_provider(vec![r#"{"arguments":{"path":"/corrected"}}"#.into()]);
            let channel = MockChannel::new(vec![]);
            let registry = create_test_registry();
            let executor =
                MockToolExecutor::new(vec![Ok(None)]).with_definitions(vec![test_tool_def()]);
            let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
            // parameter_reformat_provider left empty (default) — FR: disabled means no LLM call.

            let tool_calls = vec![tool_use_request()];
            let calls = vec![bad_tool_call()];
            let mut tool_results: Vec<
                Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
            > = vec![Err(invalid_params_error())];
            let cancel = tokio_util::sync::CancellationToken::new();

            agent
                .handle_reformat_phase(&tool_calls, &calls, &mut tool_results, &cancel)
                .await
                .unwrap();

            assert!(
                matches!(
                    tool_results[0],
                    Err(zeph_tools::ToolError::InvalidParams { .. })
                ),
                "reformat must not run when parameter_reformat_provider is empty"
            );
        }

        // Regression test: when the provider-pool registry was never wired for this `Agent`
        // (empty `provider_pool`, no `provider_config_snapshot` — the state of a lightweight
        // test/bootstrap agent that never called `with_provider_pool`, never a real production
        // agent since `validate_pool` rejects an empty `[[llm.providers]]` list at config-load
        // time), `reformat_tool_call` still falls back to the primary provider, matching every
        // other `resolve_background_provider` call site (e.g. `compress_provider`). Only once
        // the registry IS wired does an unresolvable name become a real misconfiguration (see
        // `is_a_noop_when_registry_wired_but_name_absent_from_pool` below).
        #[tokio::test]
        async fn falls_back_to_primary_when_registry_never_wired() {
            let provider = mock_provider(vec![r#"{"arguments":{"path":"/corrected"}}"#.into()]);
            let channel = MockChannel::new(vec![]);
            let registry = create_test_registry();
            let executor = MockToolExecutor::new(vec![Ok(Some(zeph_tools::ToolOutput {
                tool_name: "test_tool".to_owned().into(),
                summary: "done".to_owned(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
                ..Default::default()
            }))])
            .with_definitions(vec![test_tool_def()]);
            let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
            // Non-empty name, but provider_pool is empty — name cannot resolve.
            agent.tool_orchestrator.parameter_reformat_provider = "unregistered".to_owned();

            let tool_calls = vec![tool_use_request()];
            let calls = vec![bad_tool_call()];
            let mut tool_results: Vec<
                Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
            > = vec![Err(invalid_params_error())];
            let cancel = tokio_util::sync::CancellationToken::new();

            agent
                .handle_reformat_phase(&tool_calls, &calls, &mut tool_results, &cancel)
                .await
                .unwrap();

            let output = tool_results
                .remove(0)
                .expect("reformat should run using the primary provider fallback")
                .expect("tool output should be present");
            assert_eq!(output.summary, "done");
        }

        /// Minimal `ProviderConfigSnapshot` fixture shared by the registry-wired regression
        /// tests below — all `None`/empty since the fields under test don't need real secrets.
        fn empty_snapshot() -> crate::agent::state::ProviderConfigSnapshot {
            crate::agent::state::ProviderConfigSnapshot {
                claude_api_key: None,
                openai_api_key: None,
                gemini_api_key: None,
                compatible_api_keys: std::collections::HashMap::new(),
                llm_request_timeout_secs: 30,
                embedding_model: String::new(),
                gonka_private_key: None,
                gonka_address: None,
                cocoon_access_hash: None,
            }
        }

        // Regression test for #5478: once the provider-pool registry IS wired (non-empty
        // `provider_pool`, the state every real production `Agent` is in per #5450), a
        // configured `parameter_reformat_provider` name that does not match any entry is a real
        // misconfiguration and must no-op (keep the original tool error) rather than silently
        // reformatting with the primary provider.
        #[tokio::test]
        async fn is_a_noop_when_registry_wired_but_name_absent_from_pool() {
            let provider = mock_provider(vec![r#"{"arguments":{"path":"/corrected"}}"#.into()]);
            let channel = MockChannel::new(vec![]);
            let registry = create_test_registry();
            let executor =
                MockToolExecutor::new(vec![Ok(None)]).with_definitions(vec![test_tool_def()]);

            // The pool is wired (non-empty), but registers a different name than the one
            // configured below, so "unregistered" still cannot resolve.
            let other_entry = crate::config::ProviderEntry {
                provider_type: crate::config::ProviderKind::Ollama,
                name: Some("other".into()),
                ..Default::default()
            };
            let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
                .with_provider_pool(vec![other_entry], empty_snapshot());
            agent.tool_orchestrator.parameter_reformat_provider = "unregistered".to_owned();

            let tool_calls = vec![tool_use_request()];
            let calls = vec![bad_tool_call()];
            let mut tool_results: Vec<
                Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
            > = vec![Err(invalid_params_error())];
            let cancel = tokio_util::sync::CancellationToken::new();

            agent
                .handle_reformat_phase(&tool_calls, &calls, &mut tool_results, &cancel)
                .await
                .unwrap();

            assert!(
                matches!(
                    tool_results[0],
                    Err(zeph_tools::ToolError::InvalidParams { .. })
                ),
                "reformat must no-op when the registry is wired but the configured name is \
                 absent from the pool, not silently fall back to the primary provider"
            );
        }

        // Regression test for #5600: a configured `parameter_reformat_provider` name that IS
        // present in `provider_pool` but whose provider construction fails (e.g. a required
        // secret is missing from the config snapshot) must no-op — unlike the registry-not-wired
        // case above, this must not silently fall back to the primary provider.
        #[tokio::test]
        async fn is_a_noop_when_registry_wired_but_provider_build_fails() {
            let provider = mock_provider(vec![r#"{"arguments":{"path":"/corrected"}}"#.into()]);
            let channel = MockChannel::new(vec![]);
            let registry = create_test_registry();
            let executor =
                MockToolExecutor::new(vec![Ok(None)]).with_definitions(vec![test_tool_def()]);

            // "broken" is present in provider_pool, but is a Claude entry with no API key in
            // the snapshot, so `build_provider_for_switch` fails at resolve time.
            let broken_entry = crate::config::ProviderEntry {
                provider_type: crate::config::ProviderKind::Claude,
                name: Some("broken".into()),
                ..Default::default()
            };
            let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
                .with_provider_pool(vec![broken_entry], empty_snapshot());
            agent.tool_orchestrator.parameter_reformat_provider = "broken".to_owned();

            let tool_calls = vec![tool_use_request()];
            let calls = vec![bad_tool_call()];
            let mut tool_results: Vec<
                Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
            > = vec![Err(invalid_params_error())];
            let cancel = tokio_util::sync::CancellationToken::new();

            agent
                .handle_reformat_phase(&tool_calls, &calls, &mut tool_results, &cancel)
                .await
                .unwrap();

            assert!(
                matches!(
                    tool_results[0],
                    Err(zeph_tools::ToolError::InvalidParams { .. })
                ),
                "reformat must no-op when the in-pool provider fails to build, not silently \
                 fall back to the primary provider"
            );
        }

        #[tokio::test]
        async fn replaces_original_error_when_retry_still_fails() {
            // FR-004: the retried outcome MUST replace tool_results[idx] even when the retry
            // itself fails — the reformat phase gives up cleanly after a single attempt rather
            // than looping, and never leaves the pre-reformat error in place.
            let provider = mock_provider(vec![r#"{"arguments":{"path":"/still-bad"}}"#.into()]);
            let channel = MockChannel::new(vec![]);
            let registry = create_test_registry();
            let executor = MockToolExecutor::new(vec![Err(zeph_tools::ToolError::InvalidParams {
                message: "still not a valid path".to_owned(),
            })])
            .with_definitions(vec![test_tool_def()]);
            let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
            agent.tool_orchestrator.parameter_reformat_provider = "fast".to_owned();

            let tool_calls = vec![tool_use_request()];
            let calls = vec![bad_tool_call()];
            let mut tool_results: Vec<
                Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
            > = vec![Err(invalid_params_error())];
            let cancel = tokio_util::sync::CancellationToken::new();

            agent
                .handle_reformat_phase(&tool_calls, &calls, &mut tool_results, &cancel)
                .await
                .unwrap();

            match &tool_results[0] {
                Err(zeph_tools::ToolError::InvalidParams { message }) => {
                    assert_eq!(
                        message, "still not a valid path",
                        "the retried failure must replace the original error, not leave the \
                         pre-reformat error in place"
                    );
                }
                other => {
                    panic!("expected the retried failure to replace the original, got {other:?}")
                }
            }
        }

        #[tokio::test]
        async fn budget_exhausted_skips_remaining_calls_in_same_phase() {
            // Regression test for the timing bug: `reformat_start` used to be recreated
            // immediately before each per-call elapsed check, making the budget guard an
            // effective no-op. It is now created once before the loop, so real time consumed
            // by an earlier reformat call in the same phase counts against later calls' budget.
            let provider = mock_provider(vec![
                r#"{"arguments":{"path":"/corrected"}}"#.into(),
                r#"{"arguments":{"path":"/corrected"}}"#.into(),
            ]);
            let channel = MockChannel::new(vec![]);
            let registry = create_test_registry();
            let executor = MockToolExecutor::new(vec![Ok(Some(zeph_tools::ToolOutput {
                tool_name: "test_tool".to_owned().into(),
                summary: "done".to_owned(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
                ..Default::default()
            }))])
            .with_definitions(vec![test_tool_def()])
            // Consumes >1s of wall time on the first (only) dispatched retry, so the second
            // tool call's budget check — using the same `reformat_start` — sees the whole-phase
            // budget already exhausted.
            .with_delay(1_100);
            let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
            agent.tool_orchestrator.parameter_reformat_provider = "fast".to_owned();
            agent.tool_orchestrator.max_retry_duration_secs = 1;

            let tool_calls = vec![tool_use_request(), tool_use_request()];
            let calls = vec![bad_tool_call(), bad_tool_call()];
            let mut tool_results: Vec<
                Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
            > = vec![Err(invalid_params_error()), Err(invalid_params_error())];
            let cancel = tokio_util::sync::CancellationToken::new();

            agent
                .handle_reformat_phase(&tool_calls, &calls, &mut tool_results, &cancel)
                .await
                .unwrap();

            assert!(
                tool_results[0].is_ok(),
                "first call is within budget and should be reformatted successfully"
            );
            assert!(
                matches!(
                    tool_results[1],
                    Err(zeph_tools::ToolError::InvalidParams { .. })
                ),
                "second call must be skipped once the whole-phase budget is exhausted by the \
                 first call's real elapsed time"
            );
        }
    }
}
