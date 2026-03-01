// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use anyhow::{Context, bail};
use zeph_llm::any::AnyProvider;
use zeph_llm::claude::ClaudeProvider;
use zeph_llm::compatible::CompatibleProvider;
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
                let p = create_named_provider(name, config)?;
                providers.push(p);
            }
            if providers.is_empty() {
                bail!("router chain is empty");
            }
            let router = if config.llm.router_ema_enabled {
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
                .with_thinking_opt(cloud.thinking.clone())
                .map_err(|e| anyhow::anyhow!("invalid thinking config: {e}"))?;
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
        other => {
            if let Some(entries) = &config.llm.compatible {
                let entry = if other == "compatible" {
                    entries.first()
                } else {
                    entries.iter().find(|e| e.name == other)
                };
                if let Some(entry) = entry {
                    let api_key = config
                        .secrets
                        .compatible_api_keys
                        .get(&entry.name)
                        .with_context(|| {
                            format!(
                                "ZEPH_COMPATIBLE_{}_API_KEY required for {}",
                                entry.name.to_uppercase(),
                                entry.name
                            )
                        })?
                        .expose()
                        .to_owned();
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

pub fn create_summary_provider(model_spec: &str, config: &Config) -> anyhow::Result<AnyProvider> {
    if let Some(model) = model_spec.strip_prefix("ollama/") {
        let base_url = &config.llm.base_url;
        let provider = OllamaProvider::new(base_url, model.to_owned(), String::new());
        Ok(AnyProvider::Ollama(provider))
    } else {
        bail!("unsupported summary_model format: {model_spec} (expected 'ollama/<model>')")
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
                    .with_thinking_opt(cloud.thinking.clone())
                    .map_err(|e| anyhow::anyhow!("invalid thinking config: {e}"))?;
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
