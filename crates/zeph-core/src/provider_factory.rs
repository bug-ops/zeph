// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure provider factory helpers: build `AnyProvider` instances from config entries.
//!
//! This module contains configuration-to-provider transformation functions that are
//! used by internal `zeph-core` subsystems (skills, tools, autodream, session config).
//! They are intentionally separated from bootstrap orchestration logic so that provider
//! construction can be reasoned about and tested independently of startup sequencing.

use zeph_llm::any::AnyProvider;
use zeph_llm::claude::ClaudeProvider;
#[cfg(feature = "cocoon")]
use zeph_llm::cocoon::{CocoonClient, CocoonProvider};
use zeph_llm::compatible::CompatibleProvider;
use zeph_llm::gemini::GeminiProvider;
#[cfg(feature = "gonka")]
use zeph_llm::gonka::endpoints::{EndpointPool, GonkaEndpoint};
#[cfg(feature = "gonka")]
use zeph_llm::gonka::{GonkaProvider, RequestSigner};
use zeph_llm::http::llm_client;
use zeph_llm::ollama::OllamaProvider;
use zeph_llm::openai::OpenAiProvider;
#[cfg(feature = "gonka")]
use zeroize::Zeroizing;

use crate::agent::state::ProviderConfigSnapshot;
#[cfg(feature = "candle")]
use crate::config::{CandleDevice, CandleInlineConfig, CandleSource};
use crate::config::{Config, ProviderEntry, ProviderKind};

#[non_exhaustive]
/// Error type for provider construction failures.
///
/// String-based variants flatten the error chain intentionally: bootstrap errors are
/// terminal (the application exits), so downcasting is not needed at this stage.
/// If a future phase requires programmatic retry on specific failures, expand these
/// variants into typed sub-errors.
#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    /// Configuration validation failed.
    #[error("config error: {0}")]
    Config(#[from] crate::config::ConfigError),
    /// Provider construction failed (missing secrets, unsupported kind, etc.).
    #[error("provider error: {0}")]
    Provider(String),
    /// Memory subsystem initialization failed.
    #[error("memory error: {0}")]
    Memory(String),
    /// Age vault initialization failed.
    #[error("vault init error: {0}")]
    VaultInit(crate::vault::AgeVaultError),
    /// I/O error during bootstrap.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Build an `AnyProvider` from a `ProviderEntry` using a runtime config snapshot.
///
/// Called by the `/provider <name>` slash command, and by other runtime provider-resolution
/// paths (`Agent::resolve_background_provider`, `Agent::build_supervisor`, autodream, magic
/// docs) to switch providers at runtime without requiring the full `Config`. Router and
/// Orchestrator provider kinds are not supported for runtime switching — they require the
/// full provider pool to be re-initialized.
///
/// `secret_registry`, when `Some`, wraps the built provider so every outbound `chat*` call
/// masks registered secrets from message text (#5437) — pass the live agent's
/// `self.services.security.secret_registry.as_ref()` from any runtime call site.
///
/// # Errors
///
/// Returns `BootstrapError::Provider` when the provider kind is unsupported for runtime
/// switching, a required secret is missing, or the entry is misconfigured.
pub fn build_provider_for_switch(
    entry: &ProviderEntry,
    snapshot: &ProviderConfigSnapshot,
    secret_registry: Option<&std::sync::Arc<zeph_sanitizer::secret_mask::SecretMaskRegistry>>,
) -> Result<AnyProvider, BootstrapError> {
    use zeph_common::secret::Secret;
    // Reconstruct a minimal Config from the snapshot so we can reuse build_provider_from_entry.
    // Only fields read by build_provider_from_entry are populated; everything else uses defaults.
    // Secrets are stored as plain strings in the snapshot because Secret does not implement Clone.
    let mut config = Config::default();
    config.secrets.claude_api_key = snapshot.claude_api_key.as_deref().map(Secret::new);
    config.secrets.openai_api_key = snapshot.openai_api_key.as_deref().map(Secret::new);
    config.secrets.gemini_api_key = snapshot.gemini_api_key.as_deref().map(Secret::new);
    config.secrets.compatible_api_keys = snapshot
        .compatible_api_keys
        .iter()
        .map(|(k, v)| (k.clone(), Secret::new(v.as_str())))
        .collect();
    config.secrets.gonka_private_key = snapshot
        .gonka_private_key
        .as_ref()
        .map(|z| Secret::new(z.as_str()));
    config.secrets.gonka_address = snapshot.gonka_address.as_deref().map(Secret::new);
    config.secrets.cocoon_access_hash = snapshot.cocoon_access_hash.as_deref().map(Secret::new);
    config.timeouts.llm_request_timeout_secs = snapshot.llm_request_timeout_secs;
    config
        .llm
        .embedding_model
        .clone_from(&snapshot.embedding_model);
    build_provider_from_entry(entry, &config, secret_registry)
}

/// Build an `AnyProvider` from a unified `ProviderEntry` (new `[[llm.providers]]` format).
///
/// All provider-specific fields come from `entry`; the global `config` is used only for
/// secrets and timeout settings.
///
/// `secret_registry`, when `Some`, wraps the built provider via [`AnyProvider::masked`] so
/// every outbound `chat*`/`chat_with_tools*` call masks registered secrets from message text
/// before the request leaves the process (#5437) — this is the single construction-time choke
/// point for every `AnyProvider` the bootstrap/runtime layer builds. Bootstrap-time callers
/// (the `AppBuilder::build_*_provider` family) pass `None` here and rely on
/// `Agent::with_secret_registry` to retroactively wrap every already-set provider field once
/// the registry is known; runtime callers that resolve/switch providers on a live `Agent`
/// (`build_provider_for_switch`) pass the live registry directly since it is already known.
///
/// # Errors
///
/// Returns `BootstrapError::Provider` when a required secret is missing or an entry is
/// misconfigured (e.g. compatible provider without a name).
pub fn build_provider_from_entry(
    entry: &ProviderEntry,
    config: &Config,
    secret_registry: Option<&std::sync::Arc<zeph_sanitizer::secret_mask::SecretMaskRegistry>>,
) -> Result<AnyProvider, BootstrapError> {
    let provider = build_provider_from_entry_inner(entry, config)?;
    Ok(match secret_registry {
        Some(registry) => provider.masked(std::sync::Arc::clone(registry)
            as std::sync::Arc<dyn zeph_llm::masking::OutboundMasker>),
        None => provider,
    })
}

fn build_provider_from_entry_inner(
    entry: &ProviderEntry,
    config: &Config,
) -> Result<AnyProvider, BootstrapError> {
    match entry.provider_type {
        ProviderKind::Ollama => Ok(build_ollama_provider(entry, config)),
        ProviderKind::Claude => build_claude_provider(entry, config),
        ProviderKind::OpenAi => build_openai_provider(entry, config),
        ProviderKind::Gemini => build_gemini_provider(entry, config),
        ProviderKind::Compatible => build_compatible_provider(entry, config),
        #[cfg(feature = "candle")]
        ProviderKind::Candle => build_candle_provider(entry, config),
        #[cfg(not(feature = "candle"))]
        ProviderKind::Candle => Err(BootstrapError::Provider(
            "candle feature is not enabled".into(),
        )),
        #[cfg(feature = "gonka")]
        ProviderKind::Gonka => build_gonka_provider(entry, config),
        #[cfg(not(feature = "gonka"))]
        ProviderKind::Gonka => Err(BootstrapError::Provider(
            "gonka feature is not enabled; rebuild with --features gonka".into(),
        )),
        #[cfg(feature = "cocoon")]
        ProviderKind::Cocoon => build_cocoon_provider(entry, config),
        #[cfg(not(feature = "cocoon"))]
        ProviderKind::Cocoon => Err(BootstrapError::Provider(
            "cocoon feature is not enabled; rebuild with --features cocoon".into(),
        )),
        _ => Err(BootstrapError::Provider(format!(
            "unknown provider kind: {:?}",
            entry.provider_type
        ))),
    }
}

/// Resolve a provider by name from `config.llm.providers`, falling back to `primary` when
/// `name` is empty, not found, or fails to build.
///
/// The Agent-free counterpart to `Agent::resolve_background_provider` (`crates/zeph-core/src/
/// agent/learning/arise.rs`) — that method looks up an *already-built* provider from the live
/// `Agent`'s `provider_pool` cache; this one builds fresh from `config.llm.providers` via
/// [`build_provider_from_entry`] for callers that run *before* an `Agent` (and its pool) exists
/// — e.g. resume-time condensation at CLI/ACP/`zeph serve` construction sites (spec-068
/// architect ruling D-13, spec §8.1). Fresh-build cost is a non-issue here: this only runs on
/// session resume, not the hot per-turn path.
///
/// Runs before any `Agent`/`SecretMaskRegistry` exists, so the built provider is never
/// masked (#5437 residual gap — session-resume condensation is not on the hot per-turn path;
/// tracked as a known limitation rather than blocking this construction-time choke point).
#[must_use]
pub fn resolve_named_provider(config: &Config, primary: &AnyProvider, name: &str) -> AnyProvider {
    if name.is_empty() {
        return primary.clone();
    }
    let Some(entry) = config
        .llm
        .providers
        .iter()
        .find(|e| e.effective_name().eq_ignore_ascii_case(name))
    else {
        tracing::warn!(
            provider = name,
            "provider not found in [[llm.providers]], falling back to primary"
        );
        return primary.clone();
    };
    match build_provider_from_entry(entry, config, None) {
        Ok(provider) => provider,
        Err(e) => {
            tracing::warn!(error = %e, provider = name, "failed to build named provider, falling back to primary");
            primary.clone()
        }
    }
}

/// Build the [`zeph_session::LlmCondenser`] + token counter D-13's Agent-free resume-time
/// condensation needs, shared by every construction-time session-open path (CLI `sessions
/// resume`, ACP `spawn_acp_agent`, `zeph serve`'s `hydrate_session_sink`) so they cannot drift
/// from each other on `[session.condense]` field mapping — the same divergence risk D-10 named
/// for the hydration pipeline itself, applied here to condenser construction.
///
/// Returns the condenser plus a standalone `Arc<TokenCounterAdapter>` for
/// [`zeph_agent_persistence::resume_budget_fraction`]'s own token-counting need (distinct from
/// the counter embedded in the condenser's `SummarizationDeps`, which the LLM-summarization path
/// uses) — cheap to construct twice: [`zeph_memory::TokenCounter::new`] is backed by a
/// process-scoped `OnceLock`, so only the first call anywhere in the process pays for loading
/// the BPE tokenizer.
#[must_use]
pub fn build_resume_condenser(
    config: &Config,
    primary_provider: &AnyProvider,
) -> (
    zeph_session::LlmCondenser,
    std::sync::Arc<zeph_agent_context::memory_backend::TokenCounterAdapter>,
) {
    let condense_config = &config.session.condense;
    let condense_provider = resolve_named_provider(
        config,
        primary_provider,
        condense_config.condense_provider.as_str(),
    );
    let token_counter_adapter = std::sync::Arc::new(
        zeph_agent_context::memory_backend::TokenCounterAdapter::new(std::sync::Arc::new(
            zeph_memory::TokenCounter::new(),
        )),
    );
    let condenser = zeph_session::LlmCondenser::new(
        zeph_context::summarization::SummarizationDeps {
            provider: condense_provider,
            llm_timeout: std::time::Duration::from_secs(config.timeouts.llm_seconds),
            token_counter: std::sync::Arc::new(
                zeph_agent_context::memory_backend::TokenCounterAdapter::new(std::sync::Arc::new(
                    zeph_memory::TokenCounter::new(),
                )),
            ),
            structured_summaries: config.memory.structured_summaries,
            on_anchored_summary: None,
        },
        condense_config.threshold,
        condense_config.keep_recent,
    );
    (condenser, token_counter_adapter)
}

fn build_ollama_provider(entry: &ProviderEntry, config: &Config) -> AnyProvider {
    let base_url = entry
        .base_url
        .as_deref()
        .unwrap_or("http://localhost:11434");
    let model = entry.model.as_deref().unwrap_or("qwen3:8b").to_owned();
    let embed = entry
        .embedding_model
        .clone()
        .unwrap_or_else(|| config.llm.embedding_model.clone());
    let mut provider = OllamaProvider::new(base_url, model, embed);
    if let Some(ref vm) = entry.vision_model {
        provider = provider.with_vision_model(vm.clone());
    }
    if config.mcp.forward_output_schema {
        tracing::debug!(
            "mcp.forward_output_schema is enabled but Ollama does not support \
             output schema forwarding; setting ignored for this provider"
        );
    }
    AnyProvider::Ollama(provider)
}

fn build_claude_provider(
    entry: &ProviderEntry,
    config: &Config,
) -> Result<AnyProvider, BootstrapError> {
    let api_key = config
        .secrets
        .claude_api_key
        .as_ref()
        .ok_or_else(|| BootstrapError::Provider("ZEPH_CLAUDE_API_KEY not found in vault".into()))?
        .expose()
        .to_owned();
    let model = entry
        .model
        .clone()
        .unwrap_or_else(|| "claude-haiku-4-5-20251001".to_owned());
    let max_tokens = entry.max_tokens.unwrap_or(4096);
    let provider = ClaudeProvider::new(api_key, model, max_tokens)
        .with_client(llm_client(config.timeouts.llm_request_timeout_secs))
        .with_extended_context(entry.enable_extended_context)
        .with_thinking_opt(entry.thinking.clone())
        .map_err(|e| BootstrapError::Provider(format!("invalid thinking config: {e}")))?
        .with_server_compaction(entry.server_compaction)
        .with_prompt_cache_ttl(entry.prompt_cache_ttl)
        .with_stream_limits(config.llm.stream_limits.clone())
        .with_output_schema_forwarding(
            config.mcp.forward_output_schema,
            config.mcp.output_schema_hint_bytes,
            config.mcp.max_description_bytes,
        );
    tracing::info!(
        forward = config.mcp.forward_output_schema,
        "mcp.output_schema.forwarding_configured"
    );
    Ok(AnyProvider::Claude(provider))
}

fn build_openai_provider(
    entry: &ProviderEntry,
    config: &Config,
) -> Result<AnyProvider, BootstrapError> {
    let api_key = config
        .secrets
        .openai_api_key
        .as_ref()
        .ok_or_else(|| BootstrapError::Provider("ZEPH_OPENAI_API_KEY not found in vault".into()))?
        .expose()
        .to_owned();
    let base_url = entry
        .base_url
        .clone()
        .unwrap_or_else(|| "https://api.openai.com/v1".to_owned());
    let model = entry
        .model
        .clone()
        .unwrap_or_else(|| "gpt-4o-mini".to_owned());
    let max_tokens = entry.max_tokens.unwrap_or(4096);
    Ok(AnyProvider::OpenAi(
        OpenAiProvider::new(zeph_llm::OpenAiConfig {
            api_key,
            base_url,
            model,
            max_tokens,
            embedding_model: entry.embedding_model.clone(),
            reasoning_effort: entry.reasoning_effort.clone(),
            context_window: None,
            completion_tokens_param: None,
        })
        .with_client(llm_client(config.timeouts.llm_request_timeout_secs))
        .with_output_schema_forwarding(
            config.mcp.forward_output_schema,
            config.mcp.output_schema_hint_bytes,
            config.mcp.max_description_bytes,
        ),
    ))
}

fn build_gemini_provider(
    entry: &ProviderEntry,
    config: &Config,
) -> Result<AnyProvider, BootstrapError> {
    let api_key = config
        .secrets
        .gemini_api_key
        .as_ref()
        .ok_or_else(|| BootstrapError::Provider("ZEPH_GEMINI_API_KEY not found in vault".into()))?
        .expose()
        .to_owned();
    let model = entry
        .model
        .clone()
        .unwrap_or_else(|| "gemini-2.0-flash".to_owned());
    let max_tokens = entry.max_tokens.unwrap_or(8192);
    let base_url = entry
        .base_url
        .clone()
        .unwrap_or_else(|| "https://generativelanguage.googleapis.com".to_owned());
    let mut provider = GeminiProvider::new(api_key, model, max_tokens)
        .with_base_url(base_url)
        .with_client(llm_client(config.timeouts.llm_request_timeout_secs));
    if let Some(ref em) = entry.embedding_model {
        provider = provider.with_embedding_model(em.clone());
    }
    if let Some(level) = entry.thinking_level {
        provider = provider.with_thinking_level(level);
    }
    if let Some(budget) = entry.thinking_budget {
        provider = provider
            .with_thinking_budget(budget)
            .map_err(|e| BootstrapError::Provider(e.to_string()))?;
    }
    if let Some(include) = entry.include_thoughts {
        provider = provider.with_include_thoughts(include);
    }
    if config.mcp.forward_output_schema {
        tracing::debug!(
            "mcp.forward_output_schema is enabled but Gemini does not support \
             output schema forwarding; setting ignored for this provider"
        );
    }
    Ok(AnyProvider::Gemini(provider))
}

fn build_compatible_provider(
    entry: &ProviderEntry,
    config: &Config,
) -> Result<AnyProvider, BootstrapError> {
    let name = entry.name.as_deref().ok_or_else(|| {
        BootstrapError::Provider(
            "compatible provider requires 'name' field in [[llm.providers]]".into(),
        )
    })?;
    let base_url = entry.base_url.clone().ok_or_else(|| {
        BootstrapError::Provider(format!("compatible provider '{name}' requires 'base_url'"))
    })?;
    let model = entry.model.clone().unwrap_or_default();
    let api_key = entry.api_key.clone().unwrap_or_else(|| {
        config
            .secrets
            .compatible_api_keys
            .get(name)
            .map(|s| s.expose().to_owned())
            .unwrap_or_default()
    });
    let max_tokens = entry.max_tokens.unwrap_or(4096);
    let provider = CompatibleProvider::new(zeph_llm::CompatibleConfig {
        provider_name: name.to_owned(),
        api_key,
        base_url,
        model,
        max_tokens,
        embedding_model: entry.embedding_model.clone(),
        completion_tokens_param: None,
    })
    .with_output_schema_forwarding(
        config.mcp.forward_output_schema,
        config.mcp.output_schema_hint_bytes,
        config.mcp.max_description_bytes,
    );
    tracing::info!(
        forward = config.mcp.forward_output_schema,
        provider = name,
        "mcp.output_schema.forwarding_configured"
    );
    Ok(AnyProvider::Compatible(provider))
}

#[cfg(feature = "gonka")]
fn build_gonka_provider(
    entry: &ProviderEntry,
    config: &Config,
) -> Result<AnyProvider, BootstrapError> {
    let _span = tracing::info_span!("core.provider_factory.build_gonka").entered();

    let private_key_hex: Zeroizing<String> = Zeroizing::new(
        config
            .secrets
            .gonka_private_key
            .as_ref()
            .ok_or_else(|| {
                BootstrapError::Provider(
                    "ZEPH_GONKA_PRIVATE_KEY not found in vault; set it with: zeph vault set ZEPH_GONKA_PRIVATE_KEY <hex>".into(),
                )
            })?
            .expose()
            .to_owned(),
    );

    let chain_prefix = entry.effective_gonka_chain_prefix().to_owned();
    let signer = RequestSigner::from_hex(&private_key_hex, &chain_prefix)
        .map_err(|e| BootstrapError::Provider(format!("invalid Gonka private key: {e}")))?;

    if let Some(ref configured_address) = config.secrets.gonka_address {
        let configured = configured_address.expose().to_lowercase();
        let derived = signer.address().to_lowercase();
        if configured != derived {
            return Err(BootstrapError::Provider(format!(
                "ZEPH_GONKA_ADDRESS does not match address derived from private key \
                 (configured: {configured}, derived: {derived})"
            )));
        }
    } else {
        tracing::info!(
            address = signer.address(),
            "Gonka: using address derived from private key (ZEPH_GONKA_ADDRESS not set)"
        );
    }

    if entry.gonka_nodes.is_empty() {
        return Err(BootstrapError::Provider(
            "Gonka provider entry must have at least one node in gonka_nodes".into(),
        ));
    }

    let endpoints: Vec<GonkaEndpoint> = entry
        .gonka_nodes
        .iter()
        .map(|n| GonkaEndpoint {
            base_url: n.url.clone(),
            address: n.address.clone(),
        })
        .collect();

    let pool = EndpointPool::new(endpoints).map_err(|e| {
        BootstrapError::Provider(format!("failed to build Gonka endpoint pool: {e}"))
    })?;

    let model = entry.model.clone().unwrap_or_else(|| "gpt-4o".to_owned());
    let max_tokens = entry.max_tokens.unwrap_or(4096);
    let timeout = std::time::Duration::from_secs(config.timeouts.llm_request_timeout_secs);

    let provider = GonkaProvider::new(zeph_llm::gonka::GonkaConfig {
        signer: std::sync::Arc::new(signer),
        pool: std::sync::Arc::new(pool),
        model,
        max_tokens,
        embedding_model: entry.embedding_model.clone(),
        timeout,
    });

    Ok(AnyProvider::Gonka(provider))
}

/// Build a [`CocoonProvider`] from a `[[llm.providers]]` entry.
///
/// Resolves the access hash from the age vault when `cocoon_access_hash` is `Some(_)` in the
/// entry. If the vault key is absent an explicit, actionable error is returned.
///
/// # Errors
///
/// Returns [`BootstrapError::Provider`] when the vault key `ZEPH_COCOON_ACCESS_HASH` is
/// expected (field is `Some`) but not present in the resolved secrets.
#[cfg(feature = "cocoon")]
fn build_cocoon_provider(
    entry: &ProviderEntry,
    config: &Config,
) -> Result<AnyProvider, BootstrapError> {
    let _span = tracing::info_span!("core.provider_factory.build_cocoon").entered();

    let base_url = entry
        .cocoon_client_url
        .as_deref()
        .unwrap_or("http://localhost:10000");

    // Validate URL at construction time (MINOR-3): warn if not localhost.
    if !base_url.starts_with("http://localhost")
        && !base_url.starts_with("http://127.0.0.1")
        && !base_url.starts_with("http://[::1]")
        && !base_url.starts_with("https://localhost")
        && !base_url.starts_with("https://127.0.0.1")
        && !base_url.starts_with("https://[::1]")
    {
        tracing::warn!(
            url = base_url,
            "cocoon_client_url points to a non-localhost host; \
             ensure this is intentional (expected sidecar on localhost)"
        );
    }

    if entry
        .cocoon_access_hash
        .as_deref()
        .is_some_and(|v| !v.is_empty())
    {
        tracing::warn!(
            "cocoon_access_hash in config file appears to contain a raw value; \
             this field should be empty — the actual hash must be stored in the vault: \
             zeph vault set ZEPH_COCOON_ACCESS_HASH <hash>"
        );
    }

    let access_hash = if entry.cocoon_access_hash.is_some() {
        let hash = config
            .secrets
            .cocoon_access_hash
            .as_ref()
            .ok_or_else(|| {
                BootstrapError::Provider(
                    "ZEPH_COCOON_ACCESS_HASH not found in vault; set it with: \
                     zeph vault set ZEPH_COCOON_ACCESS_HASH <hash>"
                        .into(),
                )
            })?
            .expose()
            .to_owned();
        Some(hash)
    } else {
        None
    };

    let timeout = std::time::Duration::from_secs(config.timeouts.llm_request_timeout_secs);
    let client = std::sync::Arc::new(CocoonClient::new(base_url, access_hash, timeout));

    let model = entry
        .model
        .clone()
        .unwrap_or_else(|| "Qwen/Qwen3-0.6B".to_owned());
    let max_tokens = entry.max_tokens.unwrap_or(4096);
    let provider = CocoonProvider::new(model, max_tokens, entry.embedding_model.clone(), client);

    Ok(AnyProvider::Cocoon(provider))
}

/// Spawn an advisory health-check for all Cocoon providers that have `cocoon_health_check = true`.
///
/// Registers each check as a one-shot supervised task so it is observable via
/// [`TaskSupervisor::snapshot`] and abortable on shutdown. Failures are logged at `warn` level
/// and never propagated — the check is purely advisory.
///
/// Call this once after [`build_provider_from_entry`] has succeeded for all providers, passing
/// the session-level supervisor. The function is a no-op when `cocoon` feature is not enabled
/// or no provider has `cocoon_health_check = true`.
#[cfg(feature = "cocoon")]
pub fn spawn_cocoon_health_checks(
    providers: &[&ProviderEntry],
    config: &Config,
    supervisor: &std::sync::Arc<zeph_common::TaskSupervisor>,
) {
    for entry in providers {
        if entry.provider_type != ProviderKind::Cocoon || !entry.cocoon_health_check {
            continue;
        }
        let base_url = entry
            .cocoon_client_url
            .as_deref()
            .unwrap_or("http://localhost:10000")
            .to_owned();
        let access_hash = config
            .secrets
            .cocoon_access_hash
            .as_ref()
            .map(|s| s.expose().to_owned());
        let timeout = std::time::Duration::from_secs(config.timeouts.llm_request_timeout_secs);
        let client = std::sync::Arc::new(CocoonClient::new(&base_url, access_hash, timeout));
        supervisor.spawn(zeph_common::task_supervisor::TaskDescriptor {
            name: "core.provider_factory.cocoon_health_check",
            restart: zeph_common::task_supervisor::RestartPolicy::RunOnce,
            factory: move || {
                let client = client.clone();
                async move {
                match client.health_check().await {
                    Ok(h) => {
                        tracing::info!(
                            proxy_connected = h.proxy_connected,
                            worker_count = h.worker_count,
                            "cocoon sidecar health check passed"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "cocoon sidecar health check failed; \
                             inference requests will return LlmError::Unavailable until the sidecar is running"
                        );
                    }
                }
                }
            },
        });
    }
}

/// Pure data resolved from a `[[llm.providers]]` Candle entry, prior to the fallible device
/// selection and model-loading steps in [`build_candle_provider`].
///
/// Split out so the config → loader-args mapping (including SHA-256 threading) is unit-testable
/// without touching the network-calling `CandleProvider::new_with_timeout`.
#[cfg(feature = "candle")]
struct CandleLoadParams {
    source: zeph_llm::candle_provider::loader::ModelSource,
    template: zeph_llm::candle_provider::template::ChatTemplate,
    gen_config: zeph_llm::candle_provider::generate::GenerationConfig,
    embedding_repo: Option<String>,
    embedding_sha256: Option<String>,
    hf_token: Option<String>,
    inference_timeout: std::time::Duration,
}

#[cfg(feature = "candle")]
fn resolve_candle_load_params(
    entry: &ProviderEntry,
    candle: &CandleInlineConfig,
    config: &Config,
) -> CandleLoadParams {
    let source = match candle.source {
        CandleSource::Local => zeph_llm::candle_provider::loader::ModelSource::Local {
            path: std::path::PathBuf::from(&candle.local_path),
        },
        CandleSource::Huggingface => zeph_llm::candle_provider::loader::ModelSource::HuggingFace {
            repo_id: entry
                .model
                .clone()
                .unwrap_or_else(|| config.llm.effective_model().to_owned()),
            filename: candle.filename.clone(),
            sha256: candle.chat_model_sha256.clone(),
        },
    };
    let template =
        zeph_llm::candle_provider::template::ChatTemplate::parse_str(&candle.chat_template);
    let gen_config = zeph_llm::candle_provider::generate::GenerationConfig {
        temperature: candle.generation.temperature,
        top_p: candle.generation.top_p,
        top_k: candle.generation.top_k,
        max_tokens: candle.generation.capped_max_tokens(),
        seed: candle.generation.seed,
        repeat_penalty: candle.generation.repeat_penalty,
        repeat_last_n: candle.generation.repeat_last_n,
    };
    // Floor at 1s so that inference_timeout_secs = 0 does not cause every request to
    // immediately time out.
    let inference_timeout = std::time::Duration::from_secs(candle.inference_timeout_secs.max(1));
    CandleLoadParams {
        source,
        template,
        gen_config,
        embedding_repo: candle.embedding_repo.clone(),
        embedding_sha256: candle.embedding_model_sha256.clone(),
        hf_token: candle.hf_token.clone(),
        inference_timeout,
    }
}

#[cfg(feature = "candle")]
fn build_candle_provider(
    entry: &ProviderEntry,
    config: &Config,
) -> Result<AnyProvider, BootstrapError> {
    let candle = entry.candle.as_ref().ok_or_else(|| {
        BootstrapError::Provider(
            "candle provider requires 'candle' section in [[llm.providers]]".into(),
        )
    })?;
    let params = resolve_candle_load_params(entry, candle, config);
    let device = select_device(candle.device)?;
    zeph_llm::candle_provider::CandleProvider::new_with_timeout(
        &params.source,
        params.template,
        params.gen_config,
        params.embedding_repo.as_deref(),
        params.embedding_sha256.as_deref(),
        params.hf_token.as_deref(),
        device,
        params.inference_timeout,
    )
    .map(AnyProvider::Candle)
    .map_err(|e| BootstrapError::Provider(e.to_string()))
}

/// Select the candle compute device from a [`CandleDevice`] config value.
///
/// # Errors
///
/// Returns `BootstrapError::Provider` when the requested device is not available (e.g.
/// `CandleDevice::Metal` requested but compiled without the `metal` feature).
#[cfg(feature = "candle")]
pub fn select_device(
    preference: CandleDevice,
) -> Result<zeph_llm::candle_provider::Device, BootstrapError> {
    match preference {
        CandleDevice::Metal => {
            #[cfg(feature = "metal")]
            return zeph_llm::candle_provider::Device::new_metal(0)
                .map_err(|e| BootstrapError::Provider(e.to_string()));
            #[cfg(not(feature = "metal"))]
            return Err(BootstrapError::Provider(
                "candle compiled without metal feature".into(),
            ));
        }
        CandleDevice::Cuda => {
            #[cfg(feature = "cuda")]
            return zeph_llm::candle_provider::Device::new_cuda(0)
                .map_err(|e| BootstrapError::Provider(e.to_string()));
            #[cfg(not(feature = "cuda"))]
            return Err(BootstrapError::Provider(
                "candle compiled without cuda feature".into(),
            ));
        }
        CandleDevice::Cpu => Ok(zeph_llm::candle_provider::Device::Cpu),
        CandleDevice::Auto => {
            #[cfg(feature = "metal")]
            if let Ok(device) = zeph_llm::candle_provider::Device::new_metal(0) {
                return Ok(device);
            }
            #[cfg(feature = "cuda")]
            if let Ok(device) = zeph_llm::candle_provider::Device::new_cuda(0) {
                return Ok(device);
            }
            Ok(zeph_llm::candle_provider::Device::Cpu)
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "candle")]
    use super::select_device;
    #[cfg(feature = "candle")]
    use crate::config::CandleDevice;
    #[cfg(feature = "candle")]
    use std::assert_matches;

    #[cfg(feature = "candle")]
    #[test]
    fn select_device_cpu_default() {
        let device = select_device(CandleDevice::Cpu).unwrap();
        assert_matches!(device, zeph_llm::candle_provider::Device::Cpu);
    }

    #[cfg(all(feature = "candle", not(feature = "metal")))]
    #[test]
    fn select_device_metal_without_feature_errors() {
        let result = select_device(CandleDevice::Metal);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("metal feature"));
    }

    #[cfg(all(feature = "candle", not(feature = "cuda")))]
    #[test]
    fn select_device_cuda_without_feature_errors() {
        let result = select_device(CandleDevice::Cuda);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cuda feature"));
    }

    // --- sha256 config threading (issues #5692/#5690 follow-up: guards against a future
    // field-drop regression in `resolve_candle_load_params`) ---

    #[cfg(feature = "candle")]
    #[test]
    fn resolve_candle_load_params_threads_chat_and_embedding_sha256() {
        use super::resolve_candle_load_params;
        use crate::config::{CandleInlineConfig, Config};
        use zeph_config::providers::ProviderEntry;

        let candle = CandleInlineConfig {
            chat_model_sha256: Some("deadbeef".into()),
            embedding_repo: Some("org/embed-model".into()),
            embedding_model_sha256: Some("cafef00d".into()),
            ..CandleInlineConfig::default()
        };
        let entry = ProviderEntry {
            model: Some("org/chat-model".into()),
            candle: Some(candle.clone()),
            ..ProviderEntry::default()
        };
        let config = Config::default();

        let params = resolve_candle_load_params(&entry, &candle, &config);

        if let zeph_llm::candle_provider::loader::ModelSource::HuggingFace { sha256, .. } =
            params.source
        {
            assert_eq!(sha256.as_deref(), Some("deadbeef"));
        } else {
            panic!("expected HuggingFace source for CandleSource::default()")
        }
        assert_eq!(params.embedding_sha256.as_deref(), Some("cafef00d"));
    }

    #[cfg(feature = "candle")]
    #[test]
    fn resolve_candle_load_params_sha256_absent_by_default() {
        use super::resolve_candle_load_params;
        use crate::config::{CandleInlineConfig, Config};
        use zeph_config::providers::ProviderEntry;

        let candle = CandleInlineConfig::default();
        let entry = ProviderEntry {
            model: Some("org/chat-model".into()),
            candle: Some(candle.clone()),
            ..ProviderEntry::default()
        };
        let config = Config::default();

        let params = resolve_candle_load_params(&entry, &candle, &config);

        if let zeph_llm::candle_provider::loader::ModelSource::HuggingFace { sha256, .. } =
            params.source
        {
            assert!(sha256.is_none());
        } else {
            panic!("expected HuggingFace source for CandleSource::default()")
        }
        assert!(params.embedding_sha256.is_none());
    }

    use super::{build_provider_from_entry, resolve_named_provider};
    use crate::config::{Config, ProviderKind};
    use zeph_config::providers::ProviderEntry;
    use zeph_llm::LlmProvider;

    #[cfg(feature = "gonka")]
    mod gonka_tests {
        use super::*;
        use zeph_common::secret::Secret;
        use zeph_config::GonkaNode;
        use zeph_llm::LlmProvider;

        fn gonka_entry_with_nodes(nodes: Vec<GonkaNode>) -> ProviderEntry {
            ProviderEntry {
                provider_type: ProviderKind::Gonka,
                name: Some("gonka".into()),
                model: Some("gpt-4o".into()),
                gonka_nodes: nodes,
                ..ProviderEntry::default()
            }
        }

        fn valid_nodes() -> Vec<GonkaNode> {
            vec![GonkaNode {
                url: "https://node1.gonka.ai".into(),
                address: "gonka1w508d6qejxtdg4y5r3zarvary0c5xw7k2gsyg6".into(),
                name: Some("node1".into()),
            }]
        }

        const VALID_PRIV_KEY: &str =
            "0000000000000000000000000000000000000000000000000000000000000001";

        #[test]
        fn build_gonka_provider_missing_key_returns_error() {
            let entry = gonka_entry_with_nodes(valid_nodes());
            let config = Config::default();
            let result = build_provider_from_entry(&entry, &config, None);
            assert!(result.is_err());
            let msg = result.unwrap_err().to_string();
            assert!(
                msg.contains("ZEPH_GONKA_PRIVATE_KEY"),
                "error must mention missing key: {msg}"
            );
        }

        #[test]
        fn build_gonka_provider_empty_nodes_returns_error() {
            let entry = gonka_entry_with_nodes(vec![]);
            let mut config = Config::default();
            config.secrets.gonka_private_key = Some(Secret::new(VALID_PRIV_KEY));
            let result = build_provider_from_entry(&entry, &config, None);
            assert!(result.is_err());
            let msg = result.unwrap_err().to_string();
            assert!(
                msg.contains("gonka_nodes") || msg.contains("node"),
                "error must mention empty nodes: {msg}"
            );
        }

        #[test]
        fn build_gonka_provider_address_mismatch_returns_error() {
            let entry = gonka_entry_with_nodes(valid_nodes());
            let mut config = Config::default();
            config.secrets.gonka_private_key = Some(Secret::new(VALID_PRIV_KEY));
            config.secrets.gonka_address =
                Some(Secret::new("gonka1wrongaddress000000000000000000000000000"));
            let result = build_provider_from_entry(&entry, &config, None);
            assert!(result.is_err());
            let msg = result.unwrap_err().to_string();
            assert!(
                msg.contains("does not match"),
                "error must mention address mismatch: {msg}"
            );
        }

        #[test]
        fn build_gonka_provider_happy_path() {
            let entry = gonka_entry_with_nodes(valid_nodes());
            let mut config = Config::default();
            config.secrets.gonka_private_key = Some(Secret::new(VALID_PRIV_KEY));
            let result = build_provider_from_entry(&entry, &config, None);
            assert!(result.is_ok(), "expected Ok, got: {:?}", result.err());
            let provider = result.unwrap();
            assert_eq!(provider.name(), "gonka");
        }
    }

    fn make_provider_entry(
        embed: bool,
        model: Option<&str>,
        embedding_model: Option<&str>,
    ) -> ProviderEntry {
        ProviderEntry {
            provider_type: ProviderKind::Ollama,
            embed,
            model: model.map(str::to_owned),
            embedding_model: embedding_model.map(str::to_owned),
            ..ProviderEntry::default()
        }
    }

    #[test]
    fn stable_skill_embedding_model_prefers_embedding_model_field() {
        let mut config = Config::default();
        config.llm.providers = vec![make_provider_entry(
            true,
            Some("chat-model"),
            Some("embed-v2"),
        )];
        assert_eq!(config.llm.stable_skill_embedding_model(), "embed-v2");
    }

    #[test]
    fn stable_skill_embedding_model_falls_back_to_model_field() {
        let mut config = Config::default();
        config.llm.providers = vec![make_provider_entry(
            true,
            Some("nomic-embed-text-v2-moe:latest"),
            None,
        )];
        assert_eq!(
            config.llm.stable_skill_embedding_model(),
            "nomic-embed-text-v2-moe:latest"
        );
    }

    #[test]
    fn stable_skill_embedding_model_finds_embed_flag_entry() {
        let mut config = Config::default();
        config.llm.providers = vec![
            make_provider_entry(false, Some("chat-model"), None),
            make_provider_entry(true, Some("embed-model"), Some("text-embed-3")),
        ];
        assert_eq!(config.llm.stable_skill_embedding_model(), "text-embed-3");
    }

    #[test]
    fn stable_skill_embedding_model_falls_back_to_effective_when_no_embed_entry() {
        let mut config = Config::default();
        config.llm.embedding_model = "global-embed-model".to_owned();
        // No embed=true entry, no embedding_model field set — falls back to effective_embedding_model.
        config.llm.providers = vec![make_provider_entry(false, Some("chat"), None)];
        assert_eq!(
            config.llm.stable_skill_embedding_model(),
            config.llm.effective_embedding_model()
        );
    }

    #[test]
    fn resolve_named_provider_empty_name_falls_back_to_primary_silently() {
        let config = Config::default();
        let entry = make_provider_entry(false, Some("primary-model"), None);
        let primary = build_provider_from_entry(&entry, &config, None).unwrap();
        let resolved = resolve_named_provider(&config, &primary, "");
        assert_eq!(resolved.name(), primary.name());
    }

    #[test]
    fn resolve_named_provider_unmatched_name_falls_back_to_primary_with_warn() {
        let config = Config::default(); // no [[llm.providers]] named "fast"
        let entry = make_provider_entry(false, Some("primary-model"), None);
        let primary = build_provider_from_entry(&entry, &config, None).unwrap();
        let resolved = resolve_named_provider(&config, &primary, "fast");
        assert_eq!(resolved.name(), primary.name());
    }

    #[cfg(feature = "cocoon")]
    mod cocoon_tests {
        use super::*;

        fn cocoon_entry(access_hash: Option<&str>) -> ProviderEntry {
            ProviderEntry {
                provider_type: ProviderKind::Cocoon,
                name: Some("cocoon".into()),
                model: Some("Qwen/Qwen3-0.6B".into()),
                cocoon_client_url: Some("http://localhost:10000".into()),
                cocoon_access_hash: access_hash.map(str::to_owned),
                cocoon_health_check: false,
                ..ProviderEntry::default()
            }
        }

        /// `cocoon_access_hash = Some("")` sentinel with no vault key must return an error.
        #[test]
        fn cocoon_access_hash_gate_vault_miss_errors() {
            let entry = cocoon_entry(Some(""));
            let config = Config::default(); // secrets.cocoon_access_hash = None
            let result = build_provider_from_entry(&entry, &config, None);
            assert!(
                result.is_err(),
                "expected error when vault key is absent but sentinel is set"
            );
            let err_str = result.unwrap_err().to_string();
            assert!(
                err_str.contains("ZEPH_COCOON_ACCESS_HASH"),
                "error should mention the vault key: {err_str}"
            );
        }

        /// `cocoon_access_hash = None` must succeed without touching the vault (health check off).
        #[test]
        fn cocoon_no_access_hash_gate_succeeds_without_vault() {
            let entry = cocoon_entry(None);
            let config = Config::default();
            let result = build_provider_from_entry(&entry, &config, None);
            assert!(
                result.is_ok(),
                "expected success when no access hash requested: {:?}",
                result.err()
            );
        }
    }
}
