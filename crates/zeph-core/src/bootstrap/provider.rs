// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::any::AnyProvider;
use zeph_llm::router::triage::{ComplexityTier, TriageRouter};

/// Error type for bootstrap / provider construction failures.
///
/// String-based variants flatten the error chain intentionally: bootstrap errors are
/// terminal (the application exits), so downcasting is not needed at this stage.
/// If a future phase requires programmatic retry on specific failures, expand these
/// variants into typed sub-errors.
#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("config error: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("provider error: {0}")]
    Provider(String),
    #[error("memory error: {0}")]
    Memory(String),
    #[error("vault init error: {0}")]
    VaultInit(crate::vault::AgeVaultError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
use zeph_llm::claude::ClaudeProvider;
use zeph_llm::compatible::CompatibleProvider;
use zeph_llm::gemini::GeminiProvider;
use zeph_llm::http::llm_client;
use zeph_llm::ollama::OllamaProvider;
use zeph_llm::openai::OpenAiProvider;
use zeph_llm::router::cascade::ClassifierMode;
use zeph_llm::router::{CascadeRouterConfig, RouterProvider};

use crate::agent::state::ProviderConfigSnapshot;
use crate::config::{Config, LlmRoutingStrategy, ProviderEntry, ProviderKind};

pub fn create_provider(config: &Config) -> Result<AnyProvider, BootstrapError> {
    create_provider_from_pool(config)
}

fn build_cascade_router_config(
    cascade_cfg: &crate::config::CascadeConfig,
    config: &Config,
) -> CascadeRouterConfig {
    let classifier_mode = match cascade_cfg.classifier_mode {
        crate::config::CascadeClassifierMode::Heuristic => ClassifierMode::Heuristic,
        crate::config::CascadeClassifierMode::Judge => ClassifierMode::Judge,
    };
    // SEC-CASCADE-01: clamp quality_threshold to [0.0, 1.0]; reject NaN/Inf.
    let raw_threshold = cascade_cfg.quality_threshold;
    let quality_threshold = if raw_threshold.is_finite() {
        raw_threshold.clamp(0.0, 1.0)
    } else {
        tracing::warn!(
            raw_threshold,
            "cascade quality_threshold is non-finite, defaulting to 0.5"
        );
        0.5
    };
    if (quality_threshold - raw_threshold).abs() > f64::EPSILON {
        tracing::warn!(
            raw_threshold,
            clamped = quality_threshold,
            "cascade quality_threshold out of range [0.0, 1.0], clamped"
        );
    }
    // SEC-CASCADE-02: clamp window_size to minimum 1 to prevent silent no-op tracking.
    let window_size = cascade_cfg.window_size.max(1);
    if window_size != cascade_cfg.window_size {
        tracing::warn!(
            raw = cascade_cfg.window_size,
            "cascade window_size=0 is invalid, clamped to 1"
        );
    }
    // Build summary provider for judge mode.
    let summary_provider = if classifier_mode == ClassifierMode::Judge {
        if let Some(model_spec) = config.llm.summary_model.as_deref() {
            match create_summary_provider(model_spec, config) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "cascade: failed to build judge provider, falling back to heuristic"
                    );
                    None
                }
            }
        } else {
            tracing::warn!(
                "cascade: classifier_mode=judge requires [llm] summary_model to \
                 be configured; falling back to heuristic"
            );
            None
        }
    } else {
        None
    };
    CascadeRouterConfig {
        quality_threshold,
        max_escalations: cascade_cfg.max_escalations,
        classifier_mode,
        window_size,
        max_cascade_tokens: cascade_cfg.max_cascade_tokens,
        summary_provider,
        cost_tiers: cascade_cfg.cost_tiers.clone(),
    }
}

/// Look up a provider entry from the pool by name (exact match on `effective_name()`) or type.
///
/// Used by quarantine, guardrail, judge, and experiment eval model resolution.
pub fn create_named_provider(name: &str, config: &Config) -> Result<AnyProvider, BootstrapError> {
    let entry = config
        .llm
        .providers
        .iter()
        .find(|e| e.effective_name() == name || e.provider_type.as_str() == name)
        .ok_or_else(|| {
            BootstrapError::Provider(format!("provider '{name}' not found in [[llm.providers]]"))
        })?;
    build_provider_from_entry(entry, config)
}

/// Create an `AnyProvider` for use as the summarization provider.
///
/// `model_spec` format (set via `[llm] summary_model`):
/// - `<name>` — looks up a provider by name in `[[llm.providers]]`
/// - `ollama/<model>` — Ollama shorthand: uses the ollama provider from pool with model override
/// - `claude[/<model>]`, `openai[/<model>]`, `gemini[/<model>]` — type shorthand with optional model
pub fn create_summary_provider(
    model_spec: &str,
    config: &Config,
) -> Result<AnyProvider, BootstrapError> {
    // Try direct name lookup first (e.g. "claude", "my-openai").
    if let Some(entry) = config
        .llm
        .providers
        .iter()
        .find(|e| e.effective_name() == model_spec || e.provider_type.as_str() == model_spec)
    {
        return build_provider_from_entry(entry, config);
    }

    // Handle `type/model` shorthand: override the model on a matching provider.
    if let Some(((_, model), entry)) = model_spec.split_once('/').and_then(|(b, m)| {
        config
            .llm
            .providers
            .iter()
            .find(|e| e.provider_type.as_str() == b || e.effective_name() == b)
            .map(|e| ((b, m), e))
    }) {
        let mut cloned = entry.clone();
        cloned.model = Some(model.to_owned());
        // Cap summary max_tokens at 4096 — summaries are short.
        cloned.max_tokens = Some(cloned.max_tokens.unwrap_or(4096).min(4096));
        return build_provider_from_entry(&cloned, config);
    }

    Err(BootstrapError::Provider(format!(
        "summary_model '{model_spec}' not found in [[llm.providers]]. \
         Use a provider name or 'type/model' shorthand (e.g. 'ollama/qwen3:1.7b')."
    )))
}

#[cfg(feature = "candle")]
pub fn select_device(
    preference: &str,
) -> Result<zeph_llm::candle_provider::Device, BootstrapError> {
    match preference {
        "metal" => {
            #[cfg(feature = "metal")]
            return zeph_llm::candle_provider::Device::new_metal(0)
                .map_err(|e| BootstrapError::Provider(e.to_string()));
            #[cfg(not(feature = "metal"))]
            return Err(BootstrapError::Provider(
                "candle compiled without metal feature".into(),
            ));
        }
        "cuda" => {
            #[cfg(feature = "cuda")]
            return zeph_llm::candle_provider::Device::new_cuda(0)
                .map_err(|e| BootstrapError::Provider(e.to_string()));
            #[cfg(not(feature = "cuda"))]
            return Err(BootstrapError::Provider(
                "candle compiled without cuda feature".into(),
            ));
        }
        "auto" => {
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
        _ => Ok(zeph_llm::candle_provider::Device::Cpu),
    }
}

#[cfg(feature = "candle")]
fn build_candle_provider(
    source: zeph_llm::candle_provider::loader::ModelSource,
    candle_cfg: &crate::config::CandleConfig,
    device_pref: &str,
) -> Result<AnyProvider, BootstrapError> {
    let template =
        zeph_llm::candle_provider::template::ChatTemplate::parse_str(&candle_cfg.chat_template);
    let gen_config = zeph_llm::candle_provider::generate::GenerationConfig {
        temperature: candle_cfg.generation.temperature,
        top_p: candle_cfg.generation.top_p,
        top_k: candle_cfg.generation.top_k,
        max_tokens: candle_cfg.generation.capped_max_tokens(),
        seed: candle_cfg.generation.seed,
        repeat_penalty: candle_cfg.generation.repeat_penalty,
        repeat_last_n: candle_cfg.generation.repeat_last_n,
    };
    let device = select_device(device_pref)?;
    zeph_llm::candle_provider::CandleProvider::new(
        &source,
        template,
        gen_config,
        candle_cfg.embedding_repo.as_deref(),
        device,
    )
    .map(AnyProvider::Candle)
    .map_err(|e| BootstrapError::Provider(e.to_string()))
}

/// Build an `AnyProvider` from a `ProviderEntry` using a runtime config snapshot.
///
/// Called by the `/provider <name>` slash command to switch providers at runtime without
/// requiring the full `Config`. Router and Orchestrator provider kinds are not supported
/// for runtime switching — they require the full provider pool to be re-initialized.
///
/// # Errors
///
/// Returns `BootstrapError::Provider` when the provider kind is unsupported for runtime
/// switching, a required secret is missing, or the entry is misconfigured.
pub fn build_provider_for_switch(
    entry: &ProviderEntry,
    snapshot: &ProviderConfigSnapshot,
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
    config.timeouts.llm_request_timeout_secs = snapshot.llm_request_timeout_secs;
    config
        .llm
        .embedding_model
        .clone_from(&snapshot.embedding_model);
    build_provider_from_entry(entry, &config)
}

/// Build an `AnyProvider` from a unified `ProviderEntry` (new `[[llm.providers]]` format).
///
/// All provider-specific fields come from `entry`; the global `config` is used only for
/// secrets and timeout settings.
///
/// # Errors
///
/// Returns `BootstrapError::Provider` when a required secret is missing or an entry is
/// misconfigured (e.g. compatible provider without a name).
#[allow(clippy::too_many_lines)]
pub fn build_provider_from_entry(
    entry: &ProviderEntry,
    config: &Config,
) -> Result<AnyProvider, BootstrapError> {
    match entry.provider_type {
        ProviderKind::Ollama => {
            let base_url = entry
                .base_url
                .as_deref()
                .unwrap_or("http://localhost:11434");
            let model = entry.model.as_deref().unwrap_or("qwen3:8b").to_owned();
            let embed = entry
                .embedding_model
                .clone()
                .unwrap_or_else(|| config.llm.embedding_model.clone());
            let tool_use = entry.tool_use;
            let mut provider = OllamaProvider::new(base_url, model, embed).with_tool_use(tool_use);
            if let Some(ref vm) = entry.vision_model {
                provider = provider.with_vision_model(vm.clone());
            }
            Ok(AnyProvider::Ollama(provider))
        }
        ProviderKind::Claude => {
            let api_key = config
                .secrets
                .claude_api_key
                .as_ref()
                .ok_or_else(|| {
                    BootstrapError::Provider("ZEPH_CLAUDE_API_KEY not found in vault".into())
                })?
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
                .with_server_compaction(entry.server_compaction);
            Ok(AnyProvider::Claude(provider))
        }
        ProviderKind::OpenAi => {
            let api_key = config
                .secrets
                .openai_api_key
                .as_ref()
                .ok_or_else(|| {
                    BootstrapError::Provider("ZEPH_OPENAI_API_KEY not found in vault".into())
                })?
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
                OpenAiProvider::new(
                    api_key,
                    base_url,
                    model,
                    max_tokens,
                    entry.embedding_model.clone(),
                    entry.reasoning_effort.clone(),
                )
                .with_client(llm_client(config.timeouts.llm_request_timeout_secs)),
            ))
        }
        ProviderKind::Gemini => {
            let api_key = config
                .secrets
                .gemini_api_key
                .as_ref()
                .ok_or_else(|| {
                    BootstrapError::Provider("ZEPH_GEMINI_API_KEY not found in vault".into())
                })?
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
            Ok(AnyProvider::Gemini(provider))
        }
        ProviderKind::Compatible => {
            let name = entry.name.as_deref().ok_or_else(|| {
                BootstrapError::Provider(
                    "compatible provider requires 'name' field in [[llm.providers]]".into(),
                )
            })?;
            let base_url = entry.base_url.clone().ok_or_else(|| {
                BootstrapError::Provider(format!(
                    "compatible provider '{name}' requires 'base_url'"
                ))
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
            Ok(AnyProvider::Compatible(CompatibleProvider::new(
                name.to_owned(),
                api_key,
                base_url,
                model,
                max_tokens,
                entry.embedding_model.clone(),
            )))
        }
        #[cfg(feature = "candle")]
        ProviderKind::Candle => {
            let candle = entry.candle.as_ref().ok_or_else(|| {
                BootstrapError::Provider(
                    "candle provider requires 'candle' section in [[llm.providers]]".into(),
                )
            })?;
            let source = match candle.source.as_str() {
                "local" => zeph_llm::candle_provider::loader::ModelSource::Local {
                    path: std::path::PathBuf::from(&candle.local_path),
                },
                _ => zeph_llm::candle_provider::loader::ModelSource::HuggingFace {
                    repo_id: entry
                        .model
                        .clone()
                        .unwrap_or_else(|| config.llm.effective_model().to_owned()),
                    filename: candle.filename.clone(),
                },
            };
            let candle_cfg_adapter = crate::config::CandleConfig {
                source: candle.source.clone(),
                local_path: candle.local_path.clone(),
                filename: candle.filename.clone(),
                chat_template: candle.chat_template.clone(),
                device: candle.device.clone(),
                embedding_repo: candle.embedding_repo.clone(),
                generation: candle.generation.clone(),
            };
            build_candle_provider(source, &candle_cfg_adapter, &candle.device)
        }
        #[cfg(not(feature = "candle"))]
        ProviderKind::Candle => Err(BootstrapError::Provider(
            "candle feature is not enabled".into(),
        )),
    }
}

/// Build the primary `AnyProvider` from the new `[[llm.providers]]` pool.
///
/// When `[llm] routing` is set to a non-None strategy, all providers in the pool are
/// initialized and wrapped in a `RouterProvider` with the appropriate strategy.
/// When routing is `None`, selects the provider marked `default = true` (or the first
/// entry) and falls back to subsequent entries on initialization failure.
#[allow(clippy::too_many_lines)]
fn create_provider_from_pool(config: &Config) -> Result<AnyProvider, BootstrapError> {
    let pool = &config.llm.providers;

    // Empty pool → default Ollama on localhost.
    if pool.is_empty() {
        let base_url = config.llm.effective_base_url();
        let model = config.llm.effective_model();
        let embed = &config.llm.embedding_model;
        return Ok(AnyProvider::Ollama(OllamaProvider::new(
            base_url,
            model.to_owned(),
            embed.clone(),
        )));
    }

    match config.llm.routing {
        LlmRoutingStrategy::None => build_single_provider_from_pool(pool, config),
        LlmRoutingStrategy::Ema => {
            let providers = build_all_pool_providers(pool, config)?;
            let raw_alpha = config.llm.router_ema_alpha;
            let alpha = raw_alpha.clamp(f64::MIN_POSITIVE, 1.0);
            if (alpha - raw_alpha).abs() > f64::EPSILON {
                tracing::warn!(
                    raw_alpha,
                    clamped = alpha,
                    "router_ema_alpha out of range [MIN_POSITIVE, 1.0], clamped"
                );
            }
            Ok(AnyProvider::Router(Box::new(
                RouterProvider::new(providers).with_ema(alpha, config.llm.router_reorder_interval),
            )))
        }
        LlmRoutingStrategy::Thompson => {
            let providers = build_all_pool_providers(pool, config)?;
            let state_path = config
                .llm
                .router
                .as_ref()
                .and_then(|r| r.thompson_state_path.as_deref())
                .map(std::path::Path::new);
            Ok(AnyProvider::Router(Box::new(
                RouterProvider::new(providers).with_thompson(state_path),
            )))
        }
        LlmRoutingStrategy::Cascade => {
            let providers = build_all_pool_providers(pool, config)?;
            let cascade_cfg = config
                .llm
                .router
                .as_ref()
                .and_then(|r| r.cascade.clone())
                .unwrap_or_default();
            let router_cascade_cfg = build_cascade_router_config(&cascade_cfg, config);
            Ok(AnyProvider::Router(Box::new(
                RouterProvider::new(providers).with_cascade(router_cascade_cfg),
            )))
        }
        LlmRoutingStrategy::Task => {
            // Task-based routing is not yet implemented; fall back to single provider.
            tracing::warn!(
                "routing = \"task\" is not yet implemented; \
                 falling back to single provider from pool"
            );
            build_single_provider_from_pool(pool, config)
        }
        LlmRoutingStrategy::Triage => build_triage_provider(pool, config),
    }
}

/// Initialize all providers in the pool, skipping those that fail with a warning.
/// Returns an error if no provider could be initialized.
fn build_all_pool_providers(
    pool: &[ProviderEntry],
    config: &Config,
) -> Result<Vec<AnyProvider>, BootstrapError> {
    let mut providers = Vec::new();
    for entry in pool {
        match build_provider_from_entry(entry, config) {
            Ok(p) => providers.push(p),
            Err(e) => {
                tracing::warn!(
                    provider = entry.name.as_deref().unwrap_or("?"),
                    error = %e,
                    "skipping pool provider during routing initialization"
                );
            }
        }
    }
    if providers.is_empty() {
        return Err(BootstrapError::Provider(
            "routing enabled but no providers in [[llm.providers]] could be initialized".into(),
        ));
    }
    Ok(providers)
}

/// Build a `TriageRouter`-backed `AnyProvider` from the pool.
///
/// Reads `[llm.complexity_routing]` config and constructs tier providers by name lookup.
/// If `bypass_single_provider = true` and all configured tiers resolve to the same provider,
/// returns a single provider instead of wrapping in a `TriageRouter`.
fn build_triage_provider(
    pool: &[crate::config::ProviderEntry],
    config: &crate::config::Config,
) -> Result<AnyProvider, BootstrapError> {
    let cr = config.llm.complexity_routing.as_ref().ok_or_else(|| {
        BootstrapError::Provider(
            "routing = \"triage\" requires [llm.complexity_routing] section".into(),
        )
    })?;

    // Resolve triage classification provider.
    let default_triage_name = pool
        .first()
        .map(crate::config::ProviderEntry::effective_name)
        .unwrap_or_default();
    let triage_prov_name = cr
        .triage_provider
        .as_deref()
        .unwrap_or(default_triage_name.as_str());
    let triage_provider = create_named_provider(triage_prov_name, config).map_err(|e| {
        BootstrapError::Provider(format!(
            "triage_provider '{triage_prov_name}' not found in [[llm.providers]]: {e}"
        ))
    })?;

    // Build tier provider list. Tiers not configured in the mapping are skipped.
    let tier_config: [(ComplexityTier, Option<&str>); 4] = [
        (ComplexityTier::Simple, cr.tiers.simple.as_deref()),
        (ComplexityTier::Medium, cr.tiers.medium.as_deref()),
        (ComplexityTier::Complex, cr.tiers.complex.as_deref()),
        (ComplexityTier::Expert, cr.tiers.expert.as_deref()),
    ];

    // Collect (tier, config_name, provider) triples.
    // Bypass detection compares config names (not provider.name()) to correctly distinguish
    // two pool entries using the same provider type (e.g., two Claude configs for Haiku + Opus).
    let mut tier_providers: Vec<(ComplexityTier, AnyProvider)> = Vec::new();
    let mut tier_config_names: Vec<&str> = Vec::new();
    for (tier, maybe_name) in &tier_config {
        let Some(name) = maybe_name else { continue };
        match create_named_provider(name, config) {
            Ok(p) => {
                tier_providers.push((*tier, p));
                tier_config_names.push(name);
            }
            Err(e) => {
                tracing::warn!(
                    tier = tier.as_str(),
                    provider = name,
                    error = %e,
                    "triage: skipping tier provider (not found in pool)"
                );
            }
        }
    }

    if tier_providers.is_empty() {
        // No tiers configured — fall through to single provider.
        tracing::warn!(
            "triage routing: no tier providers configured, \
             falling back to single provider"
        );
        return build_single_provider_from_pool(pool, config);
    }

    // bypass_single_provider: if all tiers reference the same config entry name, skip triage.
    if cr.bypass_single_provider
        && let Some(first_name) = tier_config_names
            .first()
            .copied()
            .filter(|&n| tier_config_names.iter().all(|m| *m == n))
    {
        tracing::debug!(
            provider = first_name,
            "triage routing: all tiers map to same config entry, bypassing triage"
        );
        return build_single_provider_from_pool(pool, config);
    }

    let router = TriageRouter::new(
        triage_provider,
        tier_providers,
        cr.triage_timeout_secs,
        cr.max_triage_tokens,
    );
    Ok(AnyProvider::Triage(Box::new(router)))
}

/// Pick the default (or first) provider from the pool with fallback on failure.
fn build_single_provider_from_pool(
    pool: &[ProviderEntry],
    config: &Config,
) -> Result<AnyProvider, BootstrapError> {
    let primary_idx = pool.iter().position(|e| e.default).unwrap_or(0);
    let primary = &pool[primary_idx];
    match build_provider_from_entry(primary, config) {
        Ok(p) => Ok(p),
        Err(e) => {
            let name = primary.name.as_deref().unwrap_or("primary");
            tracing::warn!(provider = name, error = %e, "primary provider failed, trying next");
            for (i, entry) in pool.iter().enumerate() {
                if i == primary_idx {
                    continue;
                }
                match build_provider_from_entry(entry, config) {
                    Ok(p) => return Ok(p),
                    Err(e2) => {
                        tracing::warn!(
                            provider = entry.name.as_deref().unwrap_or("?"),
                            error = %e2,
                            "fallback provider failed"
                        );
                    }
                }
            }
            Err(BootstrapError::Provider(format!(
                "all providers in [[llm.providers]] failed to initialize; first error: {e}"
            )))
        }
    }
}
