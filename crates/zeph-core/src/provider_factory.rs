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
use zeph_llm::compatible::CompatibleProvider;
use zeph_llm::gemini::GeminiProvider;
use zeph_llm::http::llm_client;
use zeph_llm::ollama::OllamaProvider;
use zeph_llm::openai::OpenAiProvider;

use crate::agent::state::ProviderConfigSnapshot;
use crate::config::{Config, ProviderEntry, ProviderKind};

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
pub fn build_provider_from_entry(
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
    }
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
        OpenAiProvider::new(
            api_key,
            base_url,
            model,
            max_tokens,
            entry.embedding_model.clone(),
            entry.reasoning_effort.clone(),
        )
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
    let provider = CompatibleProvider::new(
        name.to_owned(),
        api_key,
        base_url,
        model,
        max_tokens,
        entry.embedding_model.clone(),
    )
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
    let device = select_device(&candle.device)?;
    // Floor at 1s so that inference_timeout_secs = 0 does not cause every request to
    // immediately time out.
    let inference_timeout = std::time::Duration::from_secs(candle.inference_timeout_secs.max(1));
    zeph_llm::candle_provider::CandleProvider::new_with_timeout(
        &source,
        template,
        gen_config,
        candle.embedding_repo.as_deref(),
        candle.hf_token.as_deref(),
        device,
        inference_timeout,
    )
    .map(AnyProvider::Candle)
    .map_err(|e| BootstrapError::Provider(e.to_string()))
}

/// Select the candle compute device based on a string preference.
///
/// Resolution order: `"metal"` → Metal GPU (requires `metal` feature),
/// `"cuda"` → CUDA GPU (requires `cuda` feature), `"auto"` → best available,
/// anything else → CPU.
///
/// # Errors
///
/// Returns `BootstrapError::Provider` when the requested device is not available (e.g.
/// `"metal"` requested but compiled without the `metal` feature).
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

/// Determine the effective embedding model name for the memory subsystem.
///
/// Resolution order:
/// 1. `embedding_model` from the `[[llm.providers]]` entry marked `embed = true`
/// 2. `embedding_model` from the first entry in `[[llm.providers]]`
/// 3. `[llm] embedding_model` global fallback
#[must_use]
pub fn effective_embedding_model(config: &Config) -> String {
    // Prefer a dedicated embed provider.
    if let Some(m) = config
        .llm
        .providers
        .iter()
        .find(|e| e.embed)
        .and_then(|e| e.embedding_model.as_ref())
    {
        return m.clone();
    }
    // Fall back to the first provider's embedding model.
    if let Some(m) = config
        .llm
        .providers
        .first()
        .and_then(|e| e.embedding_model.as_ref())
    {
        return m.clone();
    }
    config.llm.embedding_model.clone()
}

/// Resolve the stable embedding model name for skill-matcher collection versioning.
///
/// This uses the same entry resolution as the embedding provider itself: the entry
/// with `embed = true`, preferring its `embedding_model` field and falling back to
/// its `model` field. Using the actual provider's model name prevents the
/// `model_has_changed` check in [`zeph_memory::embedding_registry`] from triggering
/// false positives that would rebuild the `zeph_skills` collection on every startup.
///
/// Falls back to [`effective_embedding_model`] when no dedicated embed entry exists.
#[must_use]
pub fn stable_skill_embedding_model(config: &Config) -> String {
    // Find the dedicated embed entry (same lookup as `create_embedding_provider`).
    let embed_entry = config.llm.providers.iter().find(|e| e.embed).or_else(|| {
        config
            .llm
            .providers
            .iter()
            .find(|e| e.embedding_model.is_some())
    });

    if let Some(entry) = embed_entry {
        // Prefer the explicit `embedding_model` field; fall back to the `model` field.
        if let Some(em) = entry.embedding_model.as_ref().filter(|s| !s.is_empty()) {
            return em.clone();
        }
        if let Some(m) = entry.model.as_ref().filter(|s| !s.is_empty()) {
            return m.clone();
        }
    }

    // No dedicated embed entry — fall back to the general embedding model resolution.
    effective_embedding_model(config)
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "candle")]
    use super::select_device;

    #[cfg(feature = "candle")]
    #[test]
    fn select_device_cpu_default() {
        let device = select_device("cpu").unwrap();
        assert!(matches!(device, zeph_llm::candle_provider::Device::Cpu));
    }

    #[cfg(feature = "candle")]
    #[test]
    fn select_device_unknown_defaults_to_cpu() {
        let device = select_device("unknown").unwrap();
        assert!(matches!(device, zeph_llm::candle_provider::Device::Cpu));
    }

    #[cfg(all(feature = "candle", not(feature = "metal")))]
    #[test]
    fn select_device_metal_without_feature_errors() {
        let result = select_device("metal");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("metal feature"));
    }

    #[cfg(all(feature = "candle", not(feature = "cuda")))]
    #[test]
    fn select_device_cuda_without_feature_errors() {
        let result = select_device("cuda");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cuda feature"));
    }

    #[cfg(feature = "candle")]
    #[test]
    fn select_device_auto_fallback() {
        let device = select_device("auto").unwrap();
        assert!(matches!(
            device,
            zeph_llm::candle_provider::Device::Cpu
                | zeph_llm::candle_provider::Device::Cuda(_)
                | zeph_llm::candle_provider::Device::Metal(_)
        ));
    }

    use super::{effective_embedding_model, stable_skill_embedding_model};
    use crate::config::{Config, ProviderKind};
    use zeph_config::providers::ProviderEntry;

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
        assert_eq!(stable_skill_embedding_model(&config), "embed-v2");
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
            stable_skill_embedding_model(&config),
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
        assert_eq!(stable_skill_embedding_model(&config), "text-embed-3");
    }

    #[test]
    fn stable_skill_embedding_model_falls_back_to_effective_when_no_embed_entry() {
        let mut config = Config::default();
        config.llm.embedding_model = "global-embed-model".to_owned();
        // No embed=true entry, no embedding_model field set — falls back to effective_embedding_model.
        config.llm.providers = vec![make_provider_entry(false, Some("chat"), None)];
        assert_eq!(
            stable_skill_embedding_model(&config),
            effective_embedding_model(&config)
        );
    }
}
