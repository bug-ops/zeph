//! Application bootstrap: config resolution, provider/memory/tool construction.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use tokio::sync::{mpsc, watch};
use zeph_llm::any::AnyProvider;
use zeph_llm::claude::ClaudeProvider;
use zeph_llm::ollama::OllamaProvider;
use zeph_llm::provider::LlmProvider;
use zeph_memory::semantic::SemanticMemory;
use zeph_skills::loader::SkillMeta;
use zeph_skills::matcher::{SkillMatcher, SkillMatcherBackend};
use zeph_skills::registry::SkillRegistry;
use zeph_skills::watcher::{SkillEvent, SkillWatcher};
use zeph_tools::{CompositeExecutor, FileExecutor, ShellExecutor, WebScrapeExecutor};

use crate::config::{Config, ProviderKind};
use crate::config_watcher::{ConfigEvent, ConfigWatcher};
#[cfg(feature = "vault-age")]
use crate::vault::AgeVaultProvider;
use crate::vault::{EnvVaultProvider, VaultProvider};

#[cfg(feature = "compatible")]
use zeph_llm::compatible::CompatibleProvider;
#[cfg(feature = "openai")]
use zeph_llm::openai::OpenAiProvider;
#[cfg(feature = "qdrant")]
use zeph_skills::qdrant_matcher::QdrantSkillMatcher;

pub struct AppBuilder {
    config: Config,
    config_path: PathBuf,
    vault: Box<dyn VaultProvider>,
}

#[cfg_attr(not(feature = "vault-age"), allow(dead_code))]
pub struct VaultArgs {
    pub backend: String,
    pub key_path: Option<String>,
    pub vault_path: Option<String>,
}

pub struct WatcherBundle {
    pub skill_watcher: Option<SkillWatcher>,
    pub skill_reload_rx: mpsc::Receiver<SkillEvent>,
    pub config_watcher: Option<ConfigWatcher>,
    pub config_reload_rx: mpsc::Receiver<ConfigEvent>,
}

#[cfg(feature = "mcp")]
pub struct ToolExecutorBundle {
    pub executor: CompositeExecutor<
        CompositeExecutor<FileExecutor, CompositeExecutor<ShellExecutor, WebScrapeExecutor>>,
        zeph_mcp::McpToolExecutor,
    >,
    pub mcp_tools: Vec<zeph_mcp::McpTool>,
    pub mcp_manager: std::sync::Arc<zeph_mcp::McpManager>,
}

#[cfg(not(feature = "mcp"))]
pub struct ToolExecutorBundleNoMcp {
    pub executor:
        CompositeExecutor<FileExecutor, CompositeExecutor<ShellExecutor, WebScrapeExecutor>>,
}

impl AppBuilder {
    /// Resolve config, load it, create vault, resolve secrets.
    pub async fn from_env() -> anyhow::Result<Self> {
        let config_path = resolve_config_path();
        let mut config = Config::load(&config_path)?;
        config.validate()?;

        let vault_args = parse_vault_args(&config);
        let vault: Box<dyn VaultProvider> = match vault_args.backend.as_str() {
            "env" => Box::new(EnvVaultProvider),
            #[cfg(feature = "vault-age")]
            "age" => {
                let key = vault_args
                    .key_path
                    .context("--vault-key required for age backend")?;
                let path = vault_args
                    .vault_path
                    .context("--vault-path required for age backend")?;
                Box::new(AgeVaultProvider::new(Path::new(&key), Path::new(&path))?)
            }
            other => bail!("unknown vault backend: {other}"),
        };

        config.resolve_secrets(vault.as_ref()).await?;

        Ok(Self {
            config,
            config_path,
            vault,
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn config_mut(&mut self) -> &mut Config {
        &mut self.config
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    #[allow(dead_code)]
    pub fn vault(&self) -> &dyn VaultProvider {
        self.vault.as_ref()
    }

    pub async fn build_provider(
        &self,
    ) -> anyhow::Result<(AnyProvider, tokio::sync::mpsc::UnboundedReceiver<String>)> {
        let mut provider = create_provider(&self.config)?;

        let (status_tx, status_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        provider.set_status_tx(status_tx);

        health_check(&provider).await;

        if let AnyProvider::Ollama(ref mut ollama) = provider
            && let Ok(info) = ollama.fetch_model_info().await
            && let Some(ctx) = info.context_length
        {
            ollama.set_context_window(ctx);
            tracing::info!(context_window = ctx, "detected Ollama model context window");
        }

        Ok((provider, status_rx))
    }

    pub fn auto_budget_tokens(&self, provider: &AnyProvider) -> usize {
        if self.config.memory.auto_budget && self.config.memory.context_budget_tokens == 0 {
            if let Some(ctx_size) = provider.context_window() {
                tracing::info!(model_context = ctx_size, "auto-configured context budget");
                ctx_size
            } else {
                0
            }
        } else {
            self.config.memory.context_budget_tokens
        }
    }

    pub async fn build_memory(&self, provider: &AnyProvider) -> anyhow::Result<SemanticMemory> {
        let embed_model = self.embedding_model();
        let memory = SemanticMemory::with_weights(
            &self.config.memory.sqlite_path,
            &self.config.memory.qdrant_url,
            provider.clone(),
            &embed_model,
            self.config.memory.semantic.vector_weight,
            self.config.memory.semantic.keyword_weight,
        )
        .await?;

        if self.config.memory.semantic.enabled && memory.has_qdrant() {
            tracing::info!("semantic memory enabled, Qdrant connected");
            match memory.embed_missing().await {
                Ok(n) if n > 0 => tracing::info!("backfilled {n} missing embedding(s)"),
                Ok(_) => {}
                Err(e) => tracing::warn!("embed_missing failed: {e:#}"),
            }
        }

        Ok(memory)
    }

    pub async fn build_skill_matcher(
        &self,
        provider: &AnyProvider,
        meta: &[&SkillMeta],
        memory: &SemanticMemory,
    ) -> Option<SkillMatcherBackend> {
        let embed_model = self.embedding_model();
        create_skill_matcher(&self.config, provider, meta, memory, &embed_model).await
    }

    pub fn build_registry(&self) -> SkillRegistry {
        let skill_paths: Vec<PathBuf> =
            self.config.skills.paths.iter().map(PathBuf::from).collect();
        SkillRegistry::load(&skill_paths)
    }

    pub fn skill_paths(&self) -> Vec<PathBuf> {
        self.config.skills.paths.iter().map(PathBuf::from).collect()
    }

    #[cfg(feature = "mcp")]
    pub async fn build_tool_executor(&self) -> anyhow::Result<ToolExecutorBundle> {
        let permission_policy = self
            .config
            .tools
            .permission_policy(self.config.security.autonomy_level);
        let mut shell_executor =
            ShellExecutor::new(&self.config.tools.shell).with_permissions(permission_policy);
        if self.config.tools.audit.enabled
            && let Ok(logger) = zeph_tools::AuditLogger::from_config(&self.config.tools.audit).await
        {
            shell_executor = shell_executor.with_audit(logger);
        }

        let scrape_executor = WebScrapeExecutor::new(&self.config.tools.scrape);
        let file_executor = FileExecutor::new(
            self.config
                .tools
                .shell
                .allowed_paths
                .iter()
                .map(PathBuf::from)
                .collect(),
        );

        let mcp_manager = std::sync::Arc::new(create_mcp_manager(&self.config));
        let mcp_tools = mcp_manager.connect_all().await;
        tracing::info!("discovered {} MCP tool(s)", mcp_tools.len());

        let mcp_executor = zeph_mcp::McpToolExecutor::new(mcp_manager.clone());
        let base_executor = CompositeExecutor::new(
            file_executor,
            CompositeExecutor::new(shell_executor, scrape_executor),
        );
        let executor = CompositeExecutor::new(base_executor, mcp_executor);

        Ok(ToolExecutorBundle {
            executor,
            mcp_tools,
            mcp_manager,
        })
    }

    #[cfg(not(feature = "mcp"))]
    pub async fn build_tool_executor(&self) -> anyhow::Result<ToolExecutorBundleNoMcp> {
        let permission_policy = self
            .config
            .tools
            .permission_policy(self.config.security.autonomy_level);
        let mut shell_executor =
            ShellExecutor::new(&self.config.tools.shell).with_permissions(permission_policy);
        if self.config.tools.audit.enabled
            && let Ok(logger) = zeph_tools::AuditLogger::from_config(&self.config.tools.audit).await
        {
            shell_executor = shell_executor.with_audit(logger);
        }

        let scrape_executor = WebScrapeExecutor::new(&self.config.tools.scrape);
        let file_executor = FileExecutor::new(
            self.config
                .tools
                .shell
                .allowed_paths
                .iter()
                .map(PathBuf::from)
                .collect(),
        );

        let executor = CompositeExecutor::new(
            file_executor,
            CompositeExecutor::new(shell_executor, scrape_executor),
        );

        Ok(ToolExecutorBundleNoMcp { executor })
    }

    pub fn build_watchers(&self) -> WatcherBundle {
        let skill_paths = self.skill_paths();
        let (reload_tx, skill_reload_rx) = mpsc::channel(4);
        let skill_watcher = match SkillWatcher::start(&skill_paths, reload_tx) {
            Ok(w) => {
                tracing::info!("skill watcher started");
                Some(w)
            }
            Err(e) => {
                tracing::warn!("skill watcher unavailable: {e:#}");
                None
            }
        };

        let (config_reload_tx, config_reload_rx) = mpsc::channel(4);
        let config_watcher = match ConfigWatcher::start(&self.config_path, config_reload_tx) {
            Ok(w) => {
                tracing::info!("config watcher started");
                Some(w)
            }
            Err(e) => {
                tracing::warn!("config watcher unavailable: {e:#}");
                None
            }
        };

        WatcherBundle {
            skill_watcher,
            skill_reload_rx,
            config_watcher,
            config_reload_rx,
        }
    }

    pub fn build_shutdown() -> (watch::Sender<bool>, watch::Receiver<bool>) {
        watch::channel(false)
    }

    pub fn embedding_model(&self) -> String {
        effective_embedding_model(&self.config)
    }

    pub fn build_summary_provider(&self) -> Option<AnyProvider> {
        self.config.agent.summary_model.as_ref().and_then(
            |model_spec| match create_summary_provider(model_spec, &self.config) {
                Ok(sp) => {
                    tracing::info!(model = %model_spec, "summary provider configured");
                    Some(sp)
                }
                Err(e) => {
                    tracing::warn!("failed to create summary provider: {e:#}, using primary");
                    None
                }
            },
        )
    }
}

// --- Free functions moved from main.rs ---

pub fn resolve_config_path() -> PathBuf {
    let args: Vec<String> = std::env::args().collect();
    if let Some(path) = args.windows(2).find(|w| w[0] == "--config").map(|w| &w[1]) {
        return PathBuf::from(path);
    }
    if let Ok(path) = std::env::var("ZEPH_CONFIG") {
        return PathBuf::from(path);
    }
    PathBuf::from("config/default.toml")
}

/// Priority: CLI --vault > `ZEPH_VAULT_BACKEND` env > config.vault.backend > "env"
pub fn parse_vault_args(config: &Config) -> VaultArgs {
    let args: Vec<String> = std::env::args().collect();
    let cli_backend = args
        .windows(2)
        .find(|w| w[0] == "--vault")
        .map(|w| w[1].clone());
    let env_backend = std::env::var("ZEPH_VAULT_BACKEND").ok();
    let backend = cli_backend
        .or(env_backend)
        .unwrap_or_else(|| config.vault.backend.clone());
    let key_path = args
        .windows(2)
        .find(|w| w[0] == "--vault-key")
        .map(|w| w[1].clone());
    let vault_path = args
        .windows(2)
        .find(|w| w[0] == "--vault-path")
        .map(|w| w[1].clone());
    VaultArgs {
        backend,
        key_path,
        vault_path,
    }
}

pub async fn health_check(provider: &AnyProvider) {
    match provider {
        AnyProvider::Ollama(ollama) => match ollama.health_check().await {
            Ok(()) => tracing::info!("ollama health check passed"),
            Err(e) => tracing::warn!("ollama health check failed: {e:#}"),
        },
        #[cfg(feature = "candle")]
        AnyProvider::Candle(candle) => {
            tracing::info!("candle provider loaded, device: {}", candle.device_name());
        }
        #[cfg(feature = "orchestrator")]
        AnyProvider::Orchestrator(orch) => {
            for (name, p) in orch.providers() {
                tracing::info!(
                    "orchestrator sub-provider '{name}': {}",
                    zeph_llm::provider::LlmProvider::name(p)
                );
            }
        }
        _ => {}
    }
}

pub async fn warmup_provider(provider: &AnyProvider) {
    match provider {
        AnyProvider::Ollama(ollama) => {
            let start = std::time::Instant::now();
            match ollama.warmup().await {
                Ok(()) => {
                    tracing::info!("ollama model ready ({:.1}s)", start.elapsed().as_secs_f64());
                }
                Err(e) => tracing::warn!("ollama warmup failed: {e:#}"),
            }
        }
        #[cfg(feature = "orchestrator")]
        AnyProvider::Orchestrator(orch) => {
            for (name, p) in orch.providers() {
                if let zeph_llm::orchestrator::SubProvider::Ollama(ollama) = p {
                    let start = std::time::Instant::now();
                    match ollama.warmup().await {
                        Ok(()) => tracing::info!(
                            "ollama '{name}' ready ({:.1}s)",
                            start.elapsed().as_secs_f64()
                        ),
                        Err(e) => tracing::warn!("ollama '{name}' warmup failed: {e:#}"),
                    }
                }
            }
        }
        _ => {}
    }
}

#[allow(unused_variables)]
pub async fn create_skill_matcher(
    config: &Config,
    provider: &AnyProvider,
    meta: &[&SkillMeta],
    memory: &SemanticMemory,
    embedding_model: &str,
) -> Option<SkillMatcherBackend> {
    let embed_fn = provider.embed_fn();

    #[cfg(feature = "qdrant")]
    if config.memory.semantic.enabled && memory.has_qdrant() {
        match QdrantSkillMatcher::new(&config.memory.qdrant_url) {
            Ok(mut qm) => match qm.sync(meta, embedding_model, &embed_fn).await {
                Ok(_) => return Some(SkillMatcherBackend::Qdrant(qm)),
                Err(e) => {
                    tracing::warn!("Qdrant skill sync failed, falling back to in-memory: {e:#}");
                }
            },
            Err(e) => {
                tracing::warn!("Qdrant client creation failed, falling back to in-memory: {e:#}");
            }
        }
    }

    SkillMatcher::new(meta, &embed_fn)
        .await
        .map(SkillMatcherBackend::InMemory)
}

pub fn effective_embedding_model(config: &Config) -> String {
    match config.llm.provider {
        #[cfg(feature = "openai")]
        ProviderKind::OpenAi => {
            if let Some(m) = config
                .llm
                .openai
                .as_ref()
                .and_then(|o| o.embedding_model.clone())
            {
                return m;
            }
        }
        #[cfg(feature = "orchestrator")]
        ProviderKind::Orchestrator => {
            if let Some(orch) = &config.llm.orchestrator
                && let Some(pcfg) = orch.providers.get(&orch.embed)
            {
                #[cfg(feature = "openai")]
                if pcfg.provider_type == "openai"
                    && let Some(m) = config
                        .llm
                        .openai
                        .as_ref()
                        .and_then(|o| o.embedding_model.clone())
                {
                    return m;
                }
            }
        }
        ProviderKind::Compatible => {
            if let Some(entries) = &config.llm.compatible
                && let Some(entry) = entries.first()
                && let Some(ref m) = entry.embedding_model
            {
                return m.clone();
            }
        }
        _ => {}
    }
    config.llm.embedding_model.clone()
}

#[allow(clippy::too_many_lines)]
pub fn create_provider(config: &Config) -> anyhow::Result<AnyProvider> {
    match config.llm.provider {
        ProviderKind::Ollama | ProviderKind::Claude => {
            create_named_provider(config.llm.provider.as_str(), config)
        }
        #[cfg(feature = "openai")]
        ProviderKind::OpenAi => create_named_provider("openai", config),
        #[cfg(feature = "compatible")]
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
        #[cfg(feature = "orchestrator")]
        ProviderKind::Orchestrator => {
            let orch = build_orchestrator(config)?;
            Ok(AnyProvider::Orchestrator(Box::new(orch)))
        }
        #[cfg(feature = "router")]
        ProviderKind::Router => {
            use zeph_llm::router::RouterProvider;

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
            Ok(AnyProvider::Router(Box::new(RouterProvider::new(
                providers,
            ))))
        }
        #[allow(unreachable_patterns)]
        other => bail!("LLM provider {other} not available (feature not enabled)"),
    }
}

pub fn create_named_provider(name: &str, config: &Config) -> anyhow::Result<AnyProvider> {
    match name {
        "ollama" => {
            let provider = OllamaProvider::new(
                &config.llm.base_url,
                config.llm.model.clone(),
                config.llm.embedding_model.clone(),
            );
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
            Ok(AnyProvider::Claude(ClaudeProvider::new(
                api_key,
                cloud.model.clone(),
                cloud.max_tokens,
            )))
        }
        #[cfg(feature = "openai")]
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
            Ok(AnyProvider::OpenAi(OpenAiProvider::new(
                api_key,
                openai_cfg.base_url.clone(),
                openai_cfg.model.clone(),
                openai_cfg.max_tokens,
                openai_cfg.embedding_model.clone(),
                openai_cfg.reasoning_effort.clone(),
            )))
        }
        other => {
            #[cfg(feature = "compatible")]
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

#[cfg(feature = "orchestrator")]
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
                let model = pcfg.model.as_deref().unwrap_or(&config.llm.model);
                SubProvider::Ollama(OllamaProvider::new(
                    &config.llm.base_url,
                    model.to_owned(),
                    config.llm.embedding_model.clone(),
                ))
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
                SubProvider::Claude(ClaudeProvider::new(
                    api_key,
                    model.to_owned(),
                    cloud.max_tokens,
                ))
            }
            #[cfg(feature = "openai")]
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
                let model = pcfg.model.as_deref().unwrap_or(&openai_cfg.model);
                SubProvider::OpenAi(OpenAiProvider::new(
                    api_key,
                    openai_cfg.base_url.clone(),
                    model.to_owned(),
                    openai_cfg.max_tokens,
                    openai_cfg.embedding_model.clone(),
                    openai_cfg.reasoning_effort.clone(),
                ))
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

#[cfg(feature = "mcp")]
pub fn create_mcp_manager(config: &Config) -> zeph_mcp::McpManager {
    let entries: Vec<zeph_mcp::ServerEntry> = config
        .mcp
        .servers
        .iter()
        .map(|s| {
            let transport = if let Some(ref url) = s.url {
                zeph_mcp::McpTransport::Http { url: url.clone() }
            } else {
                zeph_mcp::McpTransport::Stdio {
                    command: s.command.clone().unwrap_or_default(),
                    args: s.args.clone(),
                    env: s.env.clone(),
                }
            };
            zeph_mcp::ServerEntry {
                id: s.id.clone(),
                transport,
                timeout: std::time::Duration::from_secs(s.timeout),
            }
        })
        .collect();
    zeph_mcp::McpManager::new(entries)
}

#[cfg(feature = "mcp")]
pub async fn create_mcp_registry(
    config: &Config,
    provider: &AnyProvider,
    mcp_tools: &[zeph_mcp::McpTool],
    embedding_model: &str,
) -> Option<zeph_mcp::McpToolRegistry> {
    if !config.memory.semantic.enabled {
        return None;
    }
    match zeph_mcp::McpToolRegistry::new(&config.memory.qdrant_url) {
        Ok(mut reg) => {
            let embed_fn = provider.embed_fn();
            if let Err(e) = reg.sync(mcp_tools, embedding_model, &embed_fn).await {
                tracing::warn!("MCP tool embedding sync failed: {e:#}");
            }
            Some(reg)
        }
        Err(e) => {
            tracing::warn!("MCP tool registry unavailable: {e:#}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vault_args_defaults_in_test_context() {
        let config = Config::load(Path::new("/nonexistent")).unwrap();
        let args = parse_vault_args(&config);
        assert_eq!(args.backend, "env");
        assert!(args.key_path.is_none());
        assert!(args.vault_path.is_none());
    }

    #[test]
    fn vault_args_uses_config_backend_as_fallback() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.vault.backend = "age".into();
        let args = parse_vault_args(&config);
        assert_eq!(args.backend, "age");
    }

    #[test]
    fn vault_args_env_overrides_config() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.vault.backend = "age".into();
        unsafe { std::env::set_var("ZEPH_VAULT_BACKEND", "env") };
        let args = parse_vault_args(&config);
        unsafe { std::env::remove_var("ZEPH_VAULT_BACKEND") };
        assert_eq!(args.backend, "env");
    }

    #[test]
    fn vault_args_struct_construction() {
        let args = VaultArgs {
            backend: "age".into(),
            key_path: Some("/tmp/key".into()),
            vault_path: Some("/tmp/vault".into()),
        };
        assert_eq!(args.backend, "age");
        assert_eq!(args.key_path.as_deref(), Some("/tmp/key"));
        assert_eq!(args.vault_path.as_deref(), Some("/tmp/vault"));
    }

    #[test]
    fn vault_args_struct_env_backend() {
        let args = VaultArgs {
            backend: "env".into(),
            key_path: None,
            vault_path: None,
        };
        assert_eq!(args.backend, "env");
        assert!(args.key_path.is_none());
        assert!(args.vault_path.is_none());
    }

    #[test]
    fn create_provider_ollama() {
        let config = Config::load(Path::new("/nonexistent")).unwrap();
        let provider = create_provider(&config).unwrap();
        assert!(matches!(provider, AnyProvider::Ollama(_)));
        assert_eq!(provider.name(), "ollama");
    }

    #[test]
    fn create_provider_claude_without_cloud_config_errors() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::Claude;
        config.llm.cloud = None;
        let result = create_provider(&config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("llm.cloud config section required")
        );
    }

    #[test]
    fn create_provider_claude_without_api_key_errors() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::Claude;
        config.llm.cloud = Some(crate::config::CloudLlmConfig {
            model: "claude-3-opus".into(),
            max_tokens: 4096,
        });
        config.secrets.claude_api_key = None;

        let result = create_provider(&config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("ZEPH_CLAUDE_API_KEY not found")
        );
    }

    #[tokio::test]
    async fn health_check_ollama_unreachable() {
        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://127.0.0.1:1",
            "test".into(),
            "embed".into(),
        ));
        health_check(&provider).await;
    }

    #[tokio::test]
    async fn health_check_claude_noop() {
        let provider = AnyProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024));
        health_check(&provider).await;
    }

    #[test]
    fn effective_embedding_model_defaults_to_llm() {
        let config = Config::load(Path::new("/nonexistent")).unwrap();
        assert_eq!(effective_embedding_model(&config), "qwen3-embedding");
    }

    #[cfg(feature = "openai")]
    #[test]
    fn effective_embedding_model_uses_openai_when_set() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::OpenAi;
        config.llm.openai = Some(crate::config::OpenAiConfig {
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-5.2".into(),
            max_tokens: 4096,
            embedding_model: Some("text-embedding-3-small".into()),
            reasoning_effort: None,
        });
        assert_eq!(effective_embedding_model(&config), "text-embedding-3-small");
    }

    #[cfg(feature = "openai")]
    #[test]
    fn effective_embedding_model_falls_back_when_openai_embed_missing() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::OpenAi;
        config.llm.openai = Some(crate::config::OpenAiConfig {
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-5.2".into(),
            max_tokens: 4096,
            embedding_model: None,
            reasoning_effort: None,
        });
        assert_eq!(effective_embedding_model(&config), "qwen3-embedding");
    }

    #[cfg(feature = "openai")]
    #[test]
    fn create_provider_openai_missing_config_errors() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::OpenAi;
        config.llm.openai = None;
        let result = create_provider(&config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("llm.openai config section required")
        );
    }

    #[cfg(feature = "openai")]
    #[test]
    fn create_provider_openai_missing_api_key_errors() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::OpenAi;
        config.llm.openai = Some(crate::config::OpenAiConfig {
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o".into(),
            max_tokens: 4096,
            embedding_model: None,
            reasoning_effort: None,
        });
        config.secrets.openai_api_key = None;
        let result = create_provider(&config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("ZEPH_OPENAI_API_KEY not found")
        );
    }

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

    #[cfg(feature = "candle")]
    #[test]
    fn create_provider_candle_without_config_errors() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::Candle;
        config.llm.candle = None;
        let result = create_provider(&config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("llm.candle config section required")
        );
    }

    #[cfg(feature = "orchestrator")]
    #[test]
    fn create_provider_orchestrator_without_config_errors() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::Orchestrator;
        config.llm.orchestrator = None;
        let result = create_provider(&config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("llm.orchestrator config section required")
        );
    }

    #[cfg(feature = "orchestrator")]
    #[test]
    fn build_orchestrator_with_unknown_provider_errors() {
        use crate::config::OrchestratorProviderConfig;
        use std::collections::HashMap;

        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::Orchestrator;

        let mut providers = HashMap::new();
        providers.insert(
            "test".to_string(),
            OrchestratorProviderConfig {
                provider_type: "unknown_type".to_string(),
                model: None,
                filename: None,
                device: None,
            },
        );

        config.llm.orchestrator = Some(crate::config::OrchestratorConfig {
            providers,
            routes: HashMap::new(),
            default: "test".to_string(),
            embed: "test".to_string(),
        });

        let result = build_orchestrator(&config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unknown orchestrator sub-provider type")
        );
    }

    #[cfg(feature = "orchestrator")]
    #[test]
    fn build_orchestrator_claude_without_cloud_config_errors() {
        use crate::config::OrchestratorProviderConfig;
        use std::collections::HashMap;

        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::Orchestrator;
        config.llm.cloud = None;

        let mut providers = HashMap::new();
        providers.insert(
            "claude_sub".to_string(),
            OrchestratorProviderConfig {
                provider_type: "claude".to_string(),
                model: None,
                filename: None,
                device: None,
            },
        );

        config.llm.orchestrator = Some(crate::config::OrchestratorConfig {
            providers,
            routes: HashMap::new(),
            default: "claude_sub".to_string(),
            embed: "claude_sub".to_string(),
        });

        let result = build_orchestrator(&config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("llm.cloud config required")
        );
    }

    #[cfg(feature = "orchestrator")]
    #[test]
    fn build_orchestrator_claude_sub_without_api_key_errors() {
        use crate::config::OrchestratorProviderConfig;
        use std::collections::HashMap;

        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::Orchestrator;
        config.llm.cloud = Some(crate::config::CloudLlmConfig {
            model: "claude-3".into(),
            max_tokens: 4096,
        });
        config.secrets.claude_api_key = None;

        let mut providers = HashMap::new();
        providers.insert(
            "claude_sub".to_string(),
            OrchestratorProviderConfig {
                provider_type: "claude".to_string(),
                model: None,
                filename: None,
                device: None,
            },
        );

        config.llm.orchestrator = Some(crate::config::OrchestratorConfig {
            providers,
            routes: HashMap::new(),
            default: "claude_sub".to_string(),
            embed: "claude_sub".to_string(),
        });

        let result = build_orchestrator(&config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("ZEPH_CLAUDE_API_KEY required")
        );
    }

    #[cfg(all(feature = "orchestrator", feature = "candle"))]
    #[test]
    fn build_orchestrator_candle_without_config_errors() {
        use crate::config::OrchestratorProviderConfig;
        use std::collections::HashMap;

        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::Orchestrator;
        config.llm.candle = None;

        let mut providers = HashMap::new();
        providers.insert(
            "candle_sub".to_string(),
            OrchestratorProviderConfig {
                provider_type: "candle".to_string(),
                model: None,
                filename: None,
                device: None,
            },
        );

        config.llm.orchestrator = Some(crate::config::OrchestratorConfig {
            providers,
            routes: HashMap::new(),
            default: "candle_sub".to_string(),
            embed: "candle_sub".to_string(),
        });

        let result = build_orchestrator(&config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("llm.candle config required")
        );
    }

    #[cfg(feature = "orchestrator")]
    #[test]
    fn build_orchestrator_with_ollama_sub_provider() {
        use crate::config::OrchestratorProviderConfig;
        use std::collections::HashMap;

        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::Orchestrator;

        let mut providers = HashMap::new();
        providers.insert(
            "ollama_sub".to_string(),
            OrchestratorProviderConfig {
                provider_type: "ollama".to_string(),
                model: Some("llama2".to_string()),
                filename: None,
                device: None,
            },
        );

        config.llm.orchestrator = Some(crate::config::OrchestratorConfig {
            providers,
            routes: HashMap::new(),
            default: "ollama_sub".to_string(),
            embed: "ollama_sub".to_string(),
        });

        let result = build_orchestrator(&config);
        assert!(result.is_ok());
    }

    #[cfg(feature = "orchestrator")]
    #[test]
    fn build_orchestrator_routes_parsing() {
        use crate::config::OrchestratorProviderConfig;
        use std::collections::HashMap;

        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::Orchestrator;

        let mut providers = HashMap::new();
        providers.insert(
            "ollama_sub".to_string(),
            OrchestratorProviderConfig {
                provider_type: "ollama".to_string(),
                model: None,
                filename: None,
                device: None,
            },
        );

        let mut routes = HashMap::new();
        routes.insert("chat".to_string(), vec!["ollama_sub".to_string()]);
        routes.insert("embed".to_string(), vec!["ollama_sub".to_string()]);

        config.llm.orchestrator = Some(crate::config::OrchestratorConfig {
            providers,
            routes,
            default: "ollama_sub".to_string(),
            embed: "ollama_sub".to_string(),
        });

        let result = build_orchestrator(&config);
        assert!(result.is_ok());
    }

    #[cfg(all(feature = "orchestrator", feature = "candle"))]
    #[test]
    fn build_orchestrator_with_candle_local_source() {
        use crate::config::OrchestratorProviderConfig;
        use std::collections::HashMap;

        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::Orchestrator;
        config.llm.candle = Some(crate::config::CandleConfig {
            source: "local".into(),
            local_path: "/tmp/model.gguf".into(),
            filename: Some("model.gguf".to_string()),
            chat_template: "{{ messages[0].content }}".into(),
            device: "cpu".into(),
            embedding_repo: Some("embed/model".into()),
            generation: crate::config::GenerationParams {
                temperature: 0.7,
                top_p: Some(0.9),
                top_k: Some(50),
                max_tokens: 512,
                seed: 42,
                repeat_penalty: 1.1,
                repeat_last_n: 64,
            },
        });

        let mut providers = HashMap::new();
        providers.insert(
            "candle_local".to_string(),
            OrchestratorProviderConfig {
                provider_type: "candle".to_string(),
                model: Some("local-model".to_string()),
                filename: None,
                device: Some("cpu".to_string()),
            },
        );

        config.llm.orchestrator = Some(crate::config::OrchestratorConfig {
            providers,
            routes: HashMap::new(),
            default: "candle_local".to_string(),
            embed: "candle_local".to_string(),
        });

        let result = build_orchestrator(&config);
        assert!(result.is_err(), "expected error loading nonexistent model");
    }

    #[cfg(feature = "candle")]
    #[tokio::test]
    async fn health_check_candle_logs_device() {
        use zeph_llm::candle_provider::CandleProvider;

        let source = zeph_llm::candle_provider::loader::ModelSource::HuggingFace {
            repo_id: "test/model".to_string(),
            filename: Some("model.gguf".to_string()),
        };
        let template = zeph_llm::candle_provider::template::ChatTemplate::parse_str(
            "{{ bos_token }}{{ messages[0].content }}",
        );
        let gen_config = zeph_llm::candle_provider::generate::GenerationConfig {
            temperature: 0.7,
            top_p: Some(0.9),
            top_k: Some(50),
            max_tokens: 512,
            seed: 42,
            repeat_penalty: 1.1,
            repeat_last_n: 64,
        };
        let device = zeph_llm::candle_provider::Device::Cpu;

        let candle_result =
            CandleProvider::new(&source, template, gen_config, Some("embed/model"), device);

        if let Ok(candle) = candle_result {
            let provider = AnyProvider::Candle(candle);
            health_check(&provider).await;
        }
    }

    #[cfg(feature = "orchestrator")]
    #[tokio::test]
    async fn health_check_orchestrator_logs_providers() {
        use std::collections::HashMap;
        use zeph_llm::orchestrator::{ModelOrchestrator, SubProvider};

        let mut providers = HashMap::new();
        providers.insert(
            "ollama_local".to_string(),
            SubProvider::Ollama(OllamaProvider::new(
                "http://localhost:11434",
                "test".into(),
                "embed".into(),
            )),
        );

        let routes = HashMap::new();
        let orch = ModelOrchestrator::new(
            routes,
            providers,
            "ollama_local".to_string(),
            "ollama_local".to_string(),
        )
        .unwrap();

        let provider = AnyProvider::Orchestrator(Box::new(orch));
        health_check(&provider).await;
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn create_mcp_manager_with_http_transport() {
        use std::collections::HashMap;

        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.mcp.servers = vec![crate::config::McpServerConfig {
            id: "test".into(),
            url: Some("http://localhost:3000".into()),
            command: None,
            args: vec![],
            env: HashMap::new(),
            timeout: 30,
        }];

        let manager = create_mcp_manager(&config);
        let debug = format!("{manager:?}");
        assert!(debug.contains("server_count: 1"));
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn create_mcp_manager_with_stdio_transport() {
        use std::collections::HashMap;

        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.mcp.servers = vec![crate::config::McpServerConfig {
            id: "test".into(),
            url: None,
            command: Some("node".into()),
            args: vec!["server.js".into()],
            env: HashMap::new(),
            timeout: 30,
        }];

        let manager = create_mcp_manager(&config);
        let debug = format!("{manager:?}");
        assert!(debug.contains("server_count: 1"));
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn create_mcp_manager_empty_servers() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.mcp.servers = vec![];

        let manager = create_mcp_manager(&config);
        let debug = format!("{manager:?}");
        assert!(debug.contains("server_count: 0"));
    }

    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn create_mcp_registry_when_semantic_disabled() {
        let config_path = Path::new("/nonexistent");
        let mut config = Config::load(config_path).unwrap();
        config.memory.semantic.enabled = false;

        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        ));

        let mcp_tools = vec![];
        let registry = create_mcp_registry(&config, &provider, &mcp_tools, "test-model").await;
        assert!(registry.is_none());
    }

    #[tokio::test]
    async fn create_skill_matcher_when_semantic_disabled() {
        let tmp = std::env::temp_dir().join("zeph_test_skill_matcher_bootstrap.db");
        let tmp_path = tmp.to_string_lossy().to_string();

        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.memory.semantic.enabled = false;
        config.memory.sqlite_path = tmp_path.clone();

        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        ));

        let memory = SemanticMemory::new(
            &tmp_path,
            &config.memory.qdrant_url,
            provider.clone(),
            &config.llm.embedding_model,
        )
        .await
        .unwrap();

        let meta: Vec<&SkillMeta> = vec![];
        let result = create_skill_matcher(&config, &provider, &meta, &memory, "test-model").await;
        assert!(result.is_none());

        let _ = std::fs::remove_file(&tmp);
    }
}
