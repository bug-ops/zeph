// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Application bootstrap: config resolution, provider/memory/tool construction.

pub mod config;
pub mod health;
pub mod mcp;
pub mod provider;
pub mod skills;

pub use config::{parse_vault_args, resolve_config_path};
pub use health::{health_check, warmup_provider};
pub use mcp::{create_mcp_manager, create_mcp_registry};
#[cfg(feature = "candle")]
pub use provider::select_device;
pub use provider::{
    BootstrapError, build_orchestrator, create_named_provider, create_provider,
    create_provider_from_config, create_summary_provider,
};
pub use skills::{create_skill_matcher, effective_embedding_model, managed_skills_dir};

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{mpsc, watch};
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider;
use zeph_memory::GraphStore;
use zeph_memory::QdrantOps;
use zeph_memory::semantic::SemanticMemory;
use zeph_skills::loader::SkillMeta;
use zeph_skills::matcher::SkillMatcherBackend;
use zeph_skills::registry::SkillRegistry;
use zeph_skills::watcher::{SkillEvent, SkillWatcher};

use crate::config::Config;
use crate::config_watcher::{ConfigEvent, ConfigWatcher};
use crate::vault::AgeVaultProvider;
use crate::vault::{EnvVaultProvider, VaultProvider};

pub struct AppBuilder {
    config: Config,
    config_path: PathBuf,
    vault: Box<dyn VaultProvider>,
    qdrant_ops: Option<QdrantOps>,
}

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

impl AppBuilder {
    /// Resolve config, load it, create vault, resolve secrets.
    ///
    /// CLI-provided overrides take priority over environment variables and config.
    ///
    /// # Errors
    ///
    /// Returns [`BootstrapError`] if config loading, validation, vault construction,
    /// secret resolution, or Qdrant URL parsing fails.
    pub async fn new(
        config_override: Option<&Path>,
        vault_override: Option<&str>,
        vault_key_override: Option<&Path>,
        vault_path_override: Option<&Path>,
    ) -> Result<Self, BootstrapError> {
        let config_path = resolve_config_path(config_override);
        let mut config = Config::load(&config_path)?;
        config.validate()?;

        let vault_args = parse_vault_args(
            &config,
            vault_override,
            vault_key_override,
            vault_path_override,
        );
        let vault: Box<dyn VaultProvider> = match vault_args.backend.as_str() {
            "env" => Box::new(EnvVaultProvider),
            "age" => {
                let key = vault_args.key_path.ok_or_else(|| {
                    BootstrapError::Provider("--vault-key required for age backend".into())
                })?;
                let path = vault_args.vault_path.ok_or_else(|| {
                    BootstrapError::Provider("--vault-path required for age backend".into())
                })?;
                Box::new(
                    AgeVaultProvider::new(Path::new(&key), Path::new(&path))
                        .map_err(BootstrapError::VaultInit)?,
                )
            }
            other => {
                return Err(BootstrapError::Provider(format!(
                    "unknown vault backend: {other}"
                )));
            }
        };

        config.resolve_secrets(vault.as_ref()).await?;

        let qdrant_ops = match config.memory.vector_backend {
            crate::config::VectorBackend::Qdrant => {
                let ops = QdrantOps::new(&config.memory.qdrant_url).map_err(|e| {
                    BootstrapError::Provider(format!(
                        "invalid qdrant_url '{}': {e}",
                        config.memory.qdrant_url
                    ))
                })?;
                Some(ops)
            }
            crate::config::VectorBackend::Sqlite => None,
        };

        Ok(Self {
            config,
            config_path,
            vault,
            qdrant_ops,
        })
    }

    pub fn qdrant_ops(&self) -> Option<&QdrantOps> {
        self.qdrant_ops.as_ref()
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

    /// Returns the vault provider used for secret resolution.
    ///
    /// Retained as part of the public `Bootstrap` API for external callers
    /// that may inspect or override vault behavior at runtime.
    pub fn vault(&self) -> &dyn VaultProvider {
        self.vault.as_ref()
    }

    /// # Errors
    ///
    /// Returns [`BootstrapError`] if provider creation or health check fails.
    pub async fn build_provider(
        &self,
    ) -> Result<
        (
            AnyProvider,
            tokio::sync::mpsc::UnboundedSender<String>,
            tokio::sync::mpsc::UnboundedReceiver<String>,
        ),
        BootstrapError,
    > {
        let mut provider = create_provider(&self.config)?;

        let (status_tx, status_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let status_tx_clone = status_tx.clone();
        provider.set_status_tx(status_tx);

        health_check(&provider).await;

        if let AnyProvider::Ollama(ref mut ollama) = provider
            && let Ok(info) = ollama.fetch_model_info().await
            && let Some(ctx) = info.context_length
        {
            ollama.set_context_window(ctx);
            tracing::info!(context_window = ctx, "detected Ollama model context window");
        }

        if let AnyProvider::Orchestrator(ref mut orch) = provider {
            orch.auto_detect_context_window().await;
        }
        if let Some(ctx) = provider.context_window()
            && !matches!(provider, AnyProvider::Ollama(_))
        {
            tracing::info!(context_window = ctx, "detected orchestrator context window");
        }

        Ok((provider, status_tx_clone, status_rx))
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

    /// # Errors
    ///
    /// Returns [`BootstrapError`] if `SQLite` cannot be initialized or if `vector_backend = "Qdrant"`
    /// but `qdrant_ops` is `None` (invariant violation — should not happen if `AppBuilder::new`
    /// succeeded).
    pub async fn build_memory(
        &self,
        provider: &AnyProvider,
    ) -> Result<SemanticMemory, BootstrapError> {
        let embed_model = self.embedding_model();
        let mut memory = match self.config.memory.vector_backend {
            crate::config::VectorBackend::Sqlite => {
                SemanticMemory::with_sqlite_backend_and_pool_size(
                    &self.config.memory.sqlite_path,
                    provider.clone(),
                    &embed_model,
                    self.config.memory.semantic.vector_weight,
                    self.config.memory.semantic.keyword_weight,
                    self.config.memory.sqlite_pool_size,
                )
                .await
                .map_err(|e| BootstrapError::Memory(e.to_string()))?
            }
            crate::config::VectorBackend::Qdrant => {
                let ops = self
                    .qdrant_ops
                    .as_ref()
                    .ok_or_else(|| {
                        BootstrapError::Memory(
                            "qdrant_ops must be Some when vector_backend = Qdrant".into(),
                        )
                    })?
                    .clone();
                SemanticMemory::with_qdrant_ops(
                    &self.config.memory.sqlite_path,
                    ops,
                    provider.clone(),
                    &embed_model,
                    self.config.memory.semantic.vector_weight,
                    self.config.memory.semantic.keyword_weight,
                    self.config.memory.sqlite_pool_size,
                )
                .await
                .map_err(|e| BootstrapError::Memory(e.to_string()))?
            }
        };

        memory = memory.with_ranking_options(
            self.config.memory.semantic.temporal_decay_enabled,
            self.config.memory.semantic.temporal_decay_half_life_days,
            self.config.memory.semantic.mmr_enabled,
            self.config.memory.semantic.mmr_lambda,
        );

        if self.config.memory.semantic.enabled && memory.is_vector_store_connected().await {
            tracing::info!("semantic memory enabled, vector store connected");
            match memory.embed_missing().await {
                Ok(n) if n > 0 => tracing::info!("backfilled {n} missing embedding(s)"),
                Ok(_) => {}
                Err(e) => tracing::warn!("embed_missing failed: {e:#}"),
            }
        }

        if self.config.memory.graph.enabled {
            let pool = memory.sqlite().pool().clone();
            let store = Arc::new(GraphStore::new(pool));
            memory = memory.with_graph_store(store);
            tracing::info!("graph memory enabled, GraphStore attached");
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
        create_skill_matcher(
            &self.config,
            provider,
            meta,
            memory,
            &embed_model,
            self.qdrant_ops.as_ref(),
        )
        .await
    }

    pub fn build_registry(&self) -> SkillRegistry {
        let skill_paths: Vec<PathBuf> =
            self.config.skills.paths.iter().map(PathBuf::from).collect();
        let registry = SkillRegistry::load(&skill_paths);

        if self.config.skills.trust.scan_on_load {
            let findings = registry.scan_loaded();
            if findings.is_empty() {
                tracing::debug!("skill content scan: no injection patterns found");
            } else {
                tracing::warn!(
                    count = findings.len(),
                    "skill content scan complete: {} skill(s) with potential injection patterns",
                    findings.len()
                );
            }
        }

        registry
    }

    pub fn skill_paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self.config.skills.paths.iter().map(PathBuf::from).collect();
        let managed_dir = managed_skills_dir();
        if !paths.contains(&managed_dir) {
            paths.push(managed_dir);
        }
        paths
    }

    pub fn managed_skills_dir() -> PathBuf {
        managed_skills_dir()
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
        // Structured config takes precedence over the string-based summary_model.
        if let Some(ref pcfg) = self.config.llm.summary_provider {
            return match create_provider_from_config(pcfg, &self.config) {
                Ok(sp) => {
                    tracing::info!(
                        provider_type = %pcfg.provider_type,
                        model = ?pcfg.model,
                        "summary provider configured via [llm.summary_provider]"
                    );
                    Some(sp)
                }
                Err(e) => {
                    tracing::warn!("failed to create summary provider: {e:#}, using primary");
                    None
                }
            };
        }
        self.config.llm.summary_model.as_ref().and_then(
            |model_spec| match create_summary_provider(model_spec, &self.config) {
                Ok(sp) => {
                    tracing::info!(model = %model_spec, "summary provider configured via llm.summary_model");
                    Some(sp)
                }
                Err(e) => {
                    tracing::warn!("failed to create summary provider: {e:#}, using primary");
                    None
                }
            },
        )
    }

    /// Build the quarantine summarizer provider when `security.content_isolation.quarantine.enabled = true`.
    ///
    /// Returns `None` when quarantine is disabled or provider resolution fails.
    /// Emits a `tracing::warn` on resolution failure (quarantine silently disabled).
    pub fn build_quarantine_provider(
        &self,
    ) -> Option<(AnyProvider, crate::sanitizer::QuarantineConfig)> {
        let qc = &self.config.security.content_isolation.quarantine;
        if !qc.enabled {
            return None;
        }
        match create_named_provider(&qc.model, &self.config) {
            Ok(p) => {
                tracing::info!(model = %qc.model, "quarantine provider configured");
                Some((p, qc.clone()))
            }
            Err(e) => {
                tracing::warn!(
                    model = %qc.model,
                    error = %e,
                    "quarantine provider resolution failed, quarantine disabled"
                );
                None
            }
        }
    }

    /// Build the guardrail filter when `security.guardrail.enabled = true`.
    ///
    /// Returns `None` when guardrail is disabled or provider resolution fails.
    /// Emits a `tracing::warn` on resolution failure (guardrail silently disabled).
    #[cfg(feature = "guardrail")]
    pub fn build_guardrail_filter(&self) -> Option<crate::sanitizer::guardrail::GuardrailFilter> {
        let (provider, config) = self.build_guardrail_provider()?;
        match crate::sanitizer::guardrail::GuardrailFilter::new(provider, &config) {
            Ok(filter) => Some(filter),
            Err(e) => {
                tracing::warn!(error = %e, "guardrail filter construction failed, guardrail disabled");
                None
            }
        }
    }

    /// Build the guardrail provider and config pair for use in multi-session contexts.
    ///
    /// Returns `None` when guardrail is disabled or provider resolution fails.
    #[cfg(feature = "guardrail")]
    pub fn build_guardrail_provider(
        &self,
    ) -> Option<(AnyProvider, crate::sanitizer::guardrail::GuardrailConfig)> {
        let gc = &self.config.security.guardrail;
        if !gc.enabled {
            return None;
        }
        let provider_name = gc.provider.as_deref().unwrap_or("ollama");
        match create_named_provider(provider_name, &self.config) {
            Ok(p) => {
                tracing::info!(
                    provider = %provider_name,
                    model = ?gc.model,
                    "guardrail provider configured"
                );
                Some((p, gc.clone()))
            }
            Err(e) => {
                tracing::warn!(
                    provider = %provider_name,
                    error = %e,
                    "guardrail provider resolution failed, guardrail disabled"
                );
                None
            }
        }
    }

    /// Build a dedicated provider for the judge detector when `detector_mode = judge`.
    ///
    /// Returns `None` when mode is `Regex` or `judge_model` is empty (primary provider used).
    /// Emits a `tracing::warn` when mode is `Judge` but no model is specified.
    pub fn build_judge_provider(&self) -> Option<AnyProvider> {
        use crate::config::DetectorMode;
        let learning = &self.config.skills.learning;
        if learning.detector_mode != DetectorMode::Judge {
            return None;
        }
        if learning.judge_model.is_empty() {
            tracing::warn!(
                provider = ?self.config.llm.provider,
                "detector_mode=judge but judge_model is empty — primary provider will be used for judging"
            );
            return None;
        }
        match create_named_provider(&learning.judge_model, &self.config) {
            Ok(jp) => {
                tracing::info!(model = %learning.judge_model, "judge provider configured");
                Some(jp)
            }
            Err(e) => {
                tracing::warn!("failed to create judge provider: {e:#}, using primary");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests;
