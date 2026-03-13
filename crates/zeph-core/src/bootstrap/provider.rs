// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use anyhow::{Context, bail};
use zeph_llm::any::AnyProvider;
use zeph_llm::claude::ClaudeProvider;
use zeph_llm::compatible::CompatibleProvider;
use zeph_llm::gemini::GeminiProvider;
use zeph_llm::http::llm_client;
use zeph_llm::ollama::OllamaProvider;
use zeph_llm::openai::OpenAiProvider;
use zeph_llm::router::RouterProvider;

use crate::config::{Config, ProviderKind};

#[allow(clippy::too_many_lines)]
pub fn create_provider(config: &Config) -> anyhow::Result<AnyProvider> {
    match config.llm.provider {
        ProviderKind::Ollama | ProviderKind::Claude => {
            create_named_provider(config.llm.provider.as_str(), config)
        }
        ProviderKind::OpenAi => create_named_provider("openai", config),
        ProviderKind::Gemini => create_named_provider("gemini", config),
        ProviderKind::Compatible => create_named_provider("compatible", config),
        #[cfg(feature = "candle")]
        ProviderKind::Candle => {
            let candle_cfg = config
                .llm
                .candle
                .as_ref()
                .context("llm.candle config section required for candle provider")?;

            let source = match candle_cfg.source.as_str() {
                "local" => zeph_llm::candle_provider::loader::ModelSource::Local {
                    path: std::path::PathBuf::from(&candle_cfg.local_path),
                },
                _ => zeph_llm::candle_provider::loader::ModelSource::HuggingFace {
                    repo_id: config.llm.model.clone(),
                    filename: candle_cfg.filename.clone(),
                },
            };

            let template = zeph_llm::candle_provider::template::ChatTemplate::parse_str(
                &candle_cfg.chat_template,
            );
            let gen_config = zeph_llm::candle_provider::generate::GenerationConfig {
                temperature: candle_cfg.generation.temperature,
                top_p: candle_cfg.generation.top_p,
                top_k: candle_cfg.generation.top_k,
                max_tokens: candle_cfg.generation.capped_max_tokens(),
                seed: candle_cfg.generation.seed,
                repeat_penalty: candle_cfg.generation.repeat_penalty,
                repeat_last_n: candle_cfg.generation.repeat_last_n,
            };

            let device = select_device(&candle_cfg.device)?;

            let provider = zeph_llm::candle_provider::CandleProvider::new(
                &source,
                template,
                gen_config,
                candle_cfg.embedding_repo.as_deref(),
                device,
            )?;
            Ok(AnyProvider::Candle(provider))
        }
        ProviderKind::Orchestrator => {
            let orch = build_orchestrator(config)?;
            Ok(AnyProvider::Orchestrator(Box::new(orch)))
        }
        ProviderKind::Router => {
            let router_cfg = config
                .llm
                .router
                .as_ref()
                .context("llm.router config section required for router provider")?;

            let mut providers = Vec::new();
            for name in &router_cfg.chain {
                match create_named_provider(name, config) {
                    Ok(p) => providers.push(p),
                    Err(e) => {
                        tracing::warn!(
                            provider = name.as_str(),
                            error = %e,
                            "skipping router chain provider (will initialize on demand if needed)"
                        );
                    }
                }
            }
            if providers.is_empty() {
                bail!(
                    "router chain is empty: none of [{}] could be initialized",
                    router_cfg.chain.join(", ")
                );
            }
            let router = if router_cfg.strategy == crate::config::RouterStrategyConfig::Thompson {
                let state_path = router_cfg
                    .thompson_state_path
                    .as_deref()
                    .map(std::path::Path::new);
                RouterProvider::new(providers).with_thompson(state_path)
            } else if config.llm.router_ema_enabled {
                let raw_alpha = config.llm.router_ema_alpha;
                let alpha = raw_alpha.clamp(f64::MIN_POSITIVE, 1.0);
                if (alpha - raw_alpha).abs() > f64::EPSILON {
                    tracing::warn!(
                        raw_alpha,
                        clamped = alpha,
                        "router_ema_alpha out of range [MIN_POSITIVE, 1.0], clamped"
                    );
                }
                RouterProvider::new(providers).with_ema(alpha, config.llm.router_reorder_interval)
            } else {
                RouterProvider::new(providers)
            };
            Ok(AnyProvider::Router(Box::new(router)))
        }
        #[cfg(not(feature = "candle"))]
        ProviderKind::Candle => bail!("candle feature is not enabled"),
    }
}

#[allow(clippy::too_many_lines)]
pub fn create_named_provider(name: &str, config: &Config) -> anyhow::Result<AnyProvider> {
    match name {
        "ollama" => {
            let tool_use = config.llm.ollama.as_ref().is_some_and(|c| c.tool_use);
            let mut provider = OllamaProvider::new(
                &config.llm.base_url,
                config.llm.model.clone(),
                config.llm.embedding_model.clone(),
            )
            .with_tool_use(tool_use);
            if let Some(ref vm) = config.llm.vision_model {
                provider = provider.with_vision_model(vm.clone());
            }
            Ok(AnyProvider::Ollama(provider))
        }
        "claude" => {
            let cloud = config
                .llm
                .cloud
                .as_ref()
                .context("llm.cloud config section required for Claude provider")?;
            let api_key = config
                .secrets
                .claude_api_key
                .as_ref()
                .context("ZEPH_CLAUDE_API_KEY not found in vault")?
                .expose()
                .to_owned();
            let provider = ClaudeProvider::new(api_key, cloud.model.clone(), cloud.max_tokens)
                .with_client(llm_client(config.timeouts.llm_request_timeout_secs))
                .with_extended_context(cloud.enable_extended_context)
                .with_thinking_opt(cloud.thinking.clone())
                .map_err(|e| anyhow::anyhow!("invalid thinking config: {e}"))?
                .with_server_compaction(cloud.server_compaction);
            Ok(AnyProvider::Claude(provider))
        }
        "openai" => {
            let openai_cfg = config
                .llm
                .openai
                .as_ref()
                .context("llm.openai config section required for OpenAI provider")?;
            let api_key = config
                .secrets
                .openai_api_key
                .as_ref()
                .context("ZEPH_OPENAI_API_KEY not found in vault")?
                .expose()
                .to_owned();
            Ok(AnyProvider::OpenAi(
                OpenAiProvider::new(
                    api_key,
                    openai_cfg.base_url.clone(),
                    openai_cfg.model.clone(),
                    openai_cfg.max_tokens,
                    openai_cfg.embedding_model.clone(),
                    openai_cfg.reasoning_effort.clone(),
                )
                .with_client(llm_client(config.timeouts.llm_request_timeout_secs)),
            ))
        }
        "gemini" => {
            let gemini_cfg = config
                .llm
                .gemini
                .as_ref()
                .context("llm.gemini config section required for Gemini provider")?;
            let api_key = config
                .secrets
                .gemini_api_key
                .as_ref()
                .context("ZEPH_GEMINI_API_KEY not found in vault")?
                .expose()
                .to_owned();
            let mut provider =
                GeminiProvider::new(api_key, gemini_cfg.model.clone(), gemini_cfg.max_tokens)
                    .with_base_url(gemini_cfg.base_url.clone())
                    .with_client(llm_client(config.timeouts.llm_request_timeout_secs));
            if let Some(ref em) = gemini_cfg.embedding_model {
                provider = provider.with_embedding_model(em.clone());
            }
            Ok(AnyProvider::Gemini(provider))
        }
        other => {
            if let Some(entries) = &config.llm.compatible {
                let entry = if other == "compatible" {
                    entries.first()
                } else {
                    entries.iter().find(|e| e.name == other)
                };
                if let Some(entry) = entry {
                    let has_key = entry.api_key.is_some()
                        || config.secrets.compatible_api_keys.contains_key(&entry.name)
                        || is_local_endpoint(&entry.base_url);
                    if !has_key {
                        bail!(
                            "ZEPH_COMPATIBLE_{}_API_KEY required for '{}' \
                             (set api_key in config, vault secret, or use a local endpoint)",
                            entry.name.to_uppercase(),
                            entry.name
                        );
                    }
                    // Resolve key: config field > vault secret > empty for local.
                    let api_key = entry.api_key.clone().unwrap_or_else(|| {
                        config
                            .secrets
                            .compatible_api_keys
                            .get(&entry.name)
                            .map(|s| s.expose().to_owned()) // lgtm[rust/cleartext-logging]
                            .unwrap_or_default()
                    });
                    return Ok(AnyProvider::Compatible(CompatibleProvider::new(
                        entry.name.clone(),
                        api_key,
                        entry.base_url.clone(),
                        entry.model.clone(),
                        entry.max_tokens,
                        entry.embedding_model.clone(),
                    )));
                }
            }
            bail!("unknown provider: {other}")
        }
    }
}

/// Create an `AnyProvider` for use as the summarization provider.
///
/// `model_spec` format (set via `[llm] summary_model`):
/// - `ollama/<model>` — Ollama at the configured `base_url`, e.g. `ollama/qwen3:1.7b`
/// - `claude` or `claude/<model>` — Claude API; requires `ZEPH_CLAUDE_API_KEY`
/// - `openai` or `openai/<model>` — OpenAI-compatible API; requires `ZEPH_OPENAI_API_KEY`
/// - `compatible/<name>` — named entry from `[[llm.compatible]]`
/// - `candle` — local candle model (requires `[llm.candle]` config; feature-gated)
#[allow(clippy::too_many_lines)]
pub fn create_summary_provider(model_spec: &str, config: &Config) -> anyhow::Result<AnyProvider> {
    let (backend, model_override) = if let Some((b, m)) = model_spec.split_once('/') {
        (b, Some(m))
    } else {
        (model_spec, None)
    };

    match backend {
        "ollama" => {
            let model =
                model_override.context("ollama summary_model requires format 'ollama/<model>'")?;
            Ok(AnyProvider::Ollama(OllamaProvider::new(
                &config.llm.base_url,
                model.to_owned(),
                String::new(),
            )))
        }
        "claude" => {
            let api_key = config
                .secrets
                .claude_api_key
                .as_ref()
                .context("ZEPH_CLAUDE_API_KEY required for claude summary provider")?
                .expose()
                .to_owned();
            let cloud = config.llm.cloud.as_ref();
            let model = model_override
                .map(str::to_owned)
                .or_else(|| cloud.map(|c| c.model.clone()))
                .unwrap_or_else(|| "claude-haiku-4-5-20251001".to_owned());
            // Cap summary max_tokens at 4096 — summaries are short.
            let max_tokens = cloud.map_or(4096, |c| c.max_tokens.min(4096));
            // Extended context intentionally skipped for summary provider: summaries are short
            // by design (max_tokens capped at 4096) and the 1M window adds unnecessary cost.
            let provider = ClaudeProvider::new(api_key, model, max_tokens)
                .with_client(llm_client(config.timeouts.llm_request_timeout_secs));
            Ok(AnyProvider::Claude(provider))
        }
        "openai" => {
            let api_key = config
                .secrets
                .openai_api_key
                .as_ref()
                .context("ZEPH_OPENAI_API_KEY required for openai summary provider")?
                .expose()
                .to_owned();
            let openai_cfg = config.llm.openai.as_ref();
            let base_url = openai_cfg.map_or_else(
                || "https://api.openai.com/v1".to_owned(),
                |c| c.base_url.clone(),
            );
            let model = model_override
                .map(str::to_owned)
                .or_else(|| openai_cfg.map(|c| c.model.clone()))
                .unwrap_or_else(|| "gpt-4o-mini".to_owned());
            let max_tokens = openai_cfg.map_or(4096, |c| c.max_tokens);
            Ok(AnyProvider::OpenAi(
                OpenAiProvider::new(api_key, base_url, model, max_tokens, None, None)
                    .with_client(llm_client(config.timeouts.llm_request_timeout_secs)),
            ))
        }
        "gemini" => {
            let api_key = config
                .secrets
                .gemini_api_key
                .as_ref()
                .context("ZEPH_GEMINI_API_KEY required for gemini summary provider")?
                .expose()
                .to_owned();
            let gemini_cfg = config.llm.gemini.as_ref();
            let model = model_override
                .map(str::to_owned)
                .or_else(|| gemini_cfg.map(|c| c.model.clone()))
                .unwrap_or_else(|| "gemini-2.0-flash".to_owned());
            let max_tokens = gemini_cfg.map_or(4096, |c| c.max_tokens.min(4096));
            let base_url = gemini_cfg.map_or_else(
                || "https://generativelanguage.googleapis.com".to_owned(),
                |c| c.base_url.clone(),
            );
            Ok(AnyProvider::Gemini(
                GeminiProvider::new(api_key, model, max_tokens)
                    .with_base_url(base_url)
                    .with_client(llm_client(config.timeouts.llm_request_timeout_secs)),
            ))
        }
        "compatible" => {
            let name = model_override
                .context("compatible summary_model requires format 'compatible/<name>'")?;
            // Delegate to create_named_provider which resolves the entry by name.
            create_named_provider(name, config)
        }
        #[cfg(feature = "candle")]
        "candle" => {
            let candle_cfg = config
                .llm
                .candle
                .as_ref()
                .context("llm.candle config section required for candle summary provider")?;
            let source = match candle_cfg.source.as_str() {
                "local" => zeph_llm::candle_provider::loader::ModelSource::Local {
                    path: std::path::PathBuf::from(&candle_cfg.local_path),
                },
                _ => zeph_llm::candle_provider::loader::ModelSource::HuggingFace {
                    repo_id: config.llm.model.clone(),
                    filename: candle_cfg.filename.clone(),
                },
            };
            let template = zeph_llm::candle_provider::template::ChatTemplate::parse_str(
                &candle_cfg.chat_template,
            );
            let gen_config = zeph_llm::candle_provider::generate::GenerationConfig {
                temperature: candle_cfg.generation.temperature,
                top_p: candle_cfg.generation.top_p,
                top_k: candle_cfg.generation.top_k,
                max_tokens: candle_cfg.generation.capped_max_tokens(),
                seed: candle_cfg.generation.seed,
                repeat_penalty: candle_cfg.generation.repeat_penalty,
                repeat_last_n: candle_cfg.generation.repeat_last_n,
            };
            let device = select_device(&candle_cfg.device)?;
            let provider = zeph_llm::candle_provider::CandleProvider::new(
                &source,
                template,
                gen_config,
                candle_cfg.embedding_repo.as_deref(),
                device,
            )?;
            Ok(AnyProvider::Candle(provider))
        }
        _ => bail!(
            "unsupported summary_model format: '{model_spec}'. \
             Supported: ollama/<model>, claude[/<model>], openai[/<model>], \
             compatible/<name>{candle}",
            candle = if cfg!(feature = "candle") {
                ", candle"
            } else {
                ""
            }
        ),
    }
}

#[cfg(feature = "candle")]
pub fn select_device(preference: &str) -> anyhow::Result<zeph_llm::candle_provider::Device> {
    match preference {
        "metal" => {
            #[cfg(feature = "metal")]
            return Ok(zeph_llm::candle_provider::Device::new_metal(0)?);
            #[cfg(not(feature = "metal"))]
            bail!("candle compiled without metal feature");
        }
        "cuda" => {
            #[cfg(feature = "cuda")]
            return Ok(zeph_llm::candle_provider::Device::new_cuda(0)?);
            #[cfg(not(feature = "cuda"))]
            bail!("candle compiled without cuda feature");
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

/// Create an `AnyProvider` from a structured provider config (`OrchestratorProviderConfig`).
///
/// Mirrors the per-entry creation logic in `build_orchestrator` but returns `AnyProvider`
/// so the result can be used outside the orchestrator context (e.g. as a summary provider).
#[allow(clippy::too_many_lines)]
pub fn create_provider_from_config(
    pcfg: &crate::config::OrchestratorProviderConfig,
    config: &Config,
) -> anyhow::Result<AnyProvider> {
    match pcfg.provider_type.as_str() {
        "ollama" => {
            let base_url = pcfg.base_url.as_deref().unwrap_or(&config.llm.base_url);
            let model = pcfg.model.as_deref().unwrap_or(&config.llm.model);
            let embed = pcfg
                .embedding_model
                .clone()
                .unwrap_or_else(|| config.llm.embedding_model.clone());
            Ok(AnyProvider::Ollama(OllamaProvider::new(
                base_url,
                model.to_owned(),
                embed,
            )))
        }
        "claude" => {
            let api_key = config
                .secrets
                .claude_api_key
                .as_ref()
                .context("ZEPH_CLAUDE_API_KEY required for claude provider")?
                .expose()
                .to_owned();
            let cloud = config.llm.cloud.as_ref();
            let model = pcfg
                .model
                .as_deref()
                .or_else(|| cloud.map(|c| c.model.as_str()))
                .unwrap_or("claude-haiku-4-5-20251001");
            let max_tokens = cloud.map_or(4096, |c| c.max_tokens);
            let enable_extended_context = cloud.is_some_and(|c| c.enable_extended_context);
            let provider = ClaudeProvider::new(api_key, model.to_owned(), max_tokens)
                .with_client(llm_client(config.timeouts.llm_request_timeout_secs))
                .with_extended_context(enable_extended_context);
            Ok(AnyProvider::Claude(provider))
        }
        "openai" => {
            let api_key = config
                .secrets
                .openai_api_key
                .as_ref()
                .context("ZEPH_OPENAI_API_KEY required for openai provider")?
                .expose()
                .to_owned();
            let openai_cfg = config.llm.openai.as_ref();
            let base_url = pcfg
                .base_url
                .clone()
                .or_else(|| openai_cfg.map(|c| c.base_url.clone()))
                .unwrap_or_else(|| "https://api.openai.com/v1".to_owned());
            let model = pcfg
                .model
                .as_deref()
                .or_else(|| openai_cfg.map(|c| c.model.as_str()))
                .unwrap_or("gpt-4o-mini");
            let max_tokens = openai_cfg.map_or(4096, |c| c.max_tokens);
            let embed = pcfg
                .embedding_model
                .clone()
                .or_else(|| openai_cfg.and_then(|c| c.embedding_model.clone()));
            Ok(AnyProvider::OpenAi(
                OpenAiProvider::new(api_key, base_url, model.to_owned(), max_tokens, embed, None)
                    .with_client(llm_client(config.timeouts.llm_request_timeout_secs)),
            ))
        }
        "gemini" => {
            let api_key = config
                .secrets
                .gemini_api_key
                .as_ref()
                .context("ZEPH_GEMINI_API_KEY required for gemini provider")?
                .expose()
                .to_owned();
            let gemini_cfg = config.llm.gemini.as_ref();
            let model = pcfg
                .model
                .as_deref()
                .or_else(|| gemini_cfg.map(|c| c.model.as_str()))
                .unwrap_or("gemini-2.0-flash");
            let max_tokens = gemini_cfg.map_or(4096, |c| c.max_tokens);
            let base_url = gemini_cfg.map_or_else(
                || "https://generativelanguage.googleapis.com".to_owned(),
                |c| c.base_url.clone(),
            );
            let mut provider = GeminiProvider::new(api_key, model.to_owned(), max_tokens)
                .with_base_url(base_url)
                .with_client(llm_client(config.timeouts.llm_request_timeout_secs));
            if let Some(em) = gemini_cfg.and_then(|c| c.embedding_model.as_deref()) {
                provider = provider.with_embedding_model(em);
            }
            Ok(AnyProvider::Gemini(provider))
        }
        "compatible" => {
            let name = pcfg
                .model
                .as_deref()
                .context("compatible provider requires 'model' set to the entry name")?;
            create_named_provider(name, config)
        }
        #[cfg(feature = "candle")]
        "candle" => {
            let candle_cfg = config
                .llm
                .candle
                .as_ref()
                .context("llm.candle config section required for candle provider")?;
            let source = match candle_cfg.source.as_str() {
                "local" => zeph_llm::candle_provider::loader::ModelSource::Local {
                    path: std::path::PathBuf::from(&candle_cfg.local_path),
                },
                _ => zeph_llm::candle_provider::loader::ModelSource::HuggingFace {
                    repo_id: pcfg
                        .model
                        .clone()
                        .unwrap_or_else(|| config.llm.model.clone()),
                    filename: candle_cfg.filename.clone(),
                },
            };
            let template = zeph_llm::candle_provider::template::ChatTemplate::parse_str(
                &candle_cfg.chat_template,
            );
            let device_pref = pcfg.device.as_deref().unwrap_or(&candle_cfg.device);
            let device = select_device(device_pref)?;
            let gen_config = zeph_llm::candle_provider::generate::GenerationConfig {
                temperature: candle_cfg.generation.temperature,
                top_p: candle_cfg.generation.top_p,
                top_k: candle_cfg.generation.top_k,
                max_tokens: candle_cfg.generation.capped_max_tokens(),
                seed: candle_cfg.generation.seed,
                repeat_penalty: candle_cfg.generation.repeat_penalty,
                repeat_last_n: candle_cfg.generation.repeat_last_n,
            };
            let provider = zeph_llm::candle_provider::CandleProvider::new(
                &source,
                template,
                gen_config,
                candle_cfg.embedding_repo.as_deref(),
                device,
            )?;
            Ok(AnyProvider::Candle(provider))
        }
        other => bail!("unknown provider type: '{other}'"),
    }
}

#[allow(clippy::too_many_lines)]
pub fn build_orchestrator(
    config: &Config,
) -> anyhow::Result<zeph_llm::orchestrator::ModelOrchestrator> {
    use std::collections::HashMap;
    use zeph_llm::orchestrator::{ModelOrchestrator, SubProvider, TaskType};

    let orch_cfg = config
        .llm
        .orchestrator
        .as_ref()
        .context("llm.orchestrator config section required for orchestrator provider")?;

    let mut providers = HashMap::new();
    for (name, pcfg) in &orch_cfg.providers {
        let provider = match pcfg.provider_type.as_str() {
            "ollama" => {
                let base_url = pcfg.base_url.as_deref().unwrap_or(&config.llm.base_url);
                let model = pcfg.model.as_deref().unwrap_or(&config.llm.model);
                let embed = pcfg
                    .embedding_model
                    .clone()
                    .unwrap_or_else(|| config.llm.embedding_model.clone());
                SubProvider::Ollama(OllamaProvider::new(base_url, model.to_owned(), embed))
            }
            "claude" => {
                let cloud = config
                    .llm
                    .cloud
                    .as_ref()
                    .context("llm.cloud config required for claude sub-provider")?;
                let api_key = config
                    .secrets
                    .claude_api_key
                    .as_ref()
                    .context("ZEPH_CLAUDE_API_KEY required for claude sub-provider")?
                    .expose()
                    .to_owned();
                let model = pcfg.model.as_deref().unwrap_or(&cloud.model);
                let sub = ClaudeProvider::new(api_key, model.to_owned(), cloud.max_tokens)
                    .with_client(llm_client(config.timeouts.llm_request_timeout_secs))
                    .with_extended_context(cloud.enable_extended_context)
                    .with_thinking_opt(cloud.thinking.clone())
                    .map_err(|e| anyhow::anyhow!("invalid thinking config: {e}"))?
                    .with_server_compaction(cloud.server_compaction);
                SubProvider::Claude(sub)
            }
            "openai" => {
                let openai_cfg = config
                    .llm
                    .openai
                    .as_ref()
                    .context("llm.openai config required for openai sub-provider")?;
                let api_key = config
                    .secrets
                    .openai_api_key
                    .as_ref()
                    .context("ZEPH_OPENAI_API_KEY required for openai sub-provider")?
                    .expose()
                    .to_owned();
                let base_url = pcfg
                    .base_url
                    .clone()
                    .unwrap_or_else(|| openai_cfg.base_url.clone());
                let model = pcfg.model.as_deref().unwrap_or(&openai_cfg.model);
                let embed = pcfg
                    .embedding_model
                    .clone()
                    .or_else(|| openai_cfg.embedding_model.clone());
                SubProvider::OpenAi(
                    OpenAiProvider::new(
                        api_key,
                        base_url,
                        model.to_owned(),
                        openai_cfg.max_tokens,
                        embed,
                        openai_cfg.reasoning_effort.clone(),
                    )
                    .with_client(llm_client(config.timeouts.llm_request_timeout_secs)),
                )
            }
            "gemini" => {
                let api_key = config
                    .secrets
                    .gemini_api_key
                    .as_ref()
                    .context("ZEPH_GEMINI_API_KEY required for gemini sub-provider")?
                    .expose()
                    .to_owned();
                let gemini_cfg = config.llm.gemini.as_ref();
                let model = pcfg
                    .model
                    .as_deref()
                    .or_else(|| gemini_cfg.map(|c| c.model.as_str()))
                    .unwrap_or("gemini-2.0-flash");
                let max_tokens = gemini_cfg.map_or(8192, |c| c.max_tokens);
                let base_url = gemini_cfg.map_or_else(
                    || "https://generativelanguage.googleapis.com".to_owned(),
                    |c| c.base_url.clone(),
                );
                SubProvider::Gemini(
                    GeminiProvider::new(api_key, model.to_owned(), max_tokens)
                        .with_base_url(base_url)
                        .with_client(llm_client(config.timeouts.llm_request_timeout_secs)),
                )
            }
            #[cfg(feature = "candle")]
            "candle" => {
                let candle_cfg = config
                    .llm
                    .candle
                    .as_ref()
                    .context("llm.candle config required for candle sub-provider")?;
                let source = match candle_cfg.source.as_str() {
                    "local" => zeph_llm::candle_provider::loader::ModelSource::Local {
                        path: std::path::PathBuf::from(&candle_cfg.local_path),
                    },
                    _ => zeph_llm::candle_provider::loader::ModelSource::HuggingFace {
                        repo_id: pcfg
                            .model
                            .clone()
                            .unwrap_or_else(|| config.llm.model.clone()),
                        filename: candle_cfg.filename.clone(),
                    },
                };
                let template = zeph_llm::candle_provider::template::ChatTemplate::parse_str(
                    &candle_cfg.chat_template,
                );
                let device_pref = pcfg.device.as_deref().unwrap_or(&candle_cfg.device);
                let device = select_device(device_pref)?;
                let gen_config = zeph_llm::candle_provider::generate::GenerationConfig {
                    temperature: candle_cfg.generation.temperature,
                    top_p: candle_cfg.generation.top_p,
                    top_k: candle_cfg.generation.top_k,
                    max_tokens: candle_cfg.generation.capped_max_tokens(),
                    seed: candle_cfg.generation.seed,
                    repeat_penalty: candle_cfg.generation.repeat_penalty,
                    repeat_last_n: candle_cfg.generation.repeat_last_n,
                };
                let candle_provider = zeph_llm::candle_provider::CandleProvider::new(
                    &source,
                    template,
                    gen_config,
                    candle_cfg.embedding_repo.as_deref(),
                    device,
                )?;
                SubProvider::Candle(candle_provider)
            }
            other => bail!("unknown orchestrator sub-provider type: {other}"),
        };
        providers.insert(name.clone(), provider);
    }

    let mut routes = HashMap::new();
    for (task_str, chain) in &orch_cfg.routes {
        let task = TaskType::parse_str(task_str);
        routes.insert(task, chain.clone());
    }

    Ok(ModelOrchestrator::new(
        routes,
        providers,
        orch_cfg.default.clone(),
        orch_cfg.embed.clone(),
    )?)
}

/// Returns `true` if `base_url` points to a local or private-network endpoint
/// where an API key is typically unnecessary.
fn is_local_endpoint(base_url: &str) -> bool {
    // Strip scheme (http:// or https://) then extract host before port/path.
    let after_scheme = base_url
        .strip_prefix("https://")
        .or_else(|| base_url.strip_prefix("http://"))
        .unwrap_or(base_url);
    let host = after_scheme
        .split('/')
        .next()
        .and_then(|h| h.split(':').next())
        .unwrap_or(after_scheme);

    if host.eq_ignore_ascii_case("localhost")
        || host == "127.0.0.1"
        || host == "::1"
        || host == "[::1]"
    {
        return true;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
            std::net::IpAddr::V6(v6) => v6.is_loopback(),
        };
    }
    // Hostname suffixes, not file extensions — suppress clippy false positive.
    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    {
        host.ends_with(".local") || host.ends_with(".internal")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_endpoints_detected() {
        assert!(is_local_endpoint("http://localhost:11434/v1"));
        assert!(is_local_endpoint("http://127.0.0.1:8080"));
        assert!(is_local_endpoint("https://localhost/api"));
        assert!(is_local_endpoint("http://192.168.1.100:11434/v1"));
        assert!(is_local_endpoint("http://10.0.0.5:8000"));
        assert!(is_local_endpoint("http://172.16.0.1:9090"));
        assert!(is_local_endpoint("http://myhost.local:11434"));
        assert!(is_local_endpoint("http://service.internal:8080"));
    }

    #[test]
    fn remote_endpoints_not_local() {
        assert!(!is_local_endpoint("https://api.openai.com/v1"));
        assert!(!is_local_endpoint("https://api.anthropic.com"));
        assert!(!is_local_endpoint("http://8.8.8.8:11434"));
        assert!(!is_local_endpoint("https://my-server.example.com/v1"));
    }
}
