// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Application bootstrap: config resolution, provider/memory/tool construction.

pub mod config;
pub mod health;
pub mod mcp;
pub mod oauth;
pub mod provider;
pub mod skills;

pub use config::{parse_vault_args, resolve_config_path};
pub use health::{health_check, warmup_provider};
pub use mcp::{create_mcp_manager_with_vault, create_mcp_registry, wire_trust_calibration};
pub use oauth::VaultCredentialStore;
pub use provider::{
    BootstrapError, build_provider_from_entry, create_named_provider, create_provider,
    create_summary_provider,
};
pub use skills::{
    create_embedding_provider, create_skill_matcher, effective_embedding_model, managed_skills_dir,
    plugins_dir, stable_skill_embedding_model,
};

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{RwLock, watch};
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider;
use zeph_memory::GraphStore;
use zeph_memory::QdrantOps;
use zeph_memory::semantic::SemanticMemory;
use zeph_skills::loader::SkillMeta;
use zeph_skills::matcher::SkillMatcherBackend;
use zeph_skills::registry::SkillRegistry;
use zeph_skills::watcher::{SkillEvent, SkillWatcher};

use zeph_core::config::{Config, SecretResolver};
use zeph_core::config_watcher::{ConfigEvent, ConfigWatcher};
use zeph_core::vault::AgeVaultProvider;
use zeph_core::vault::{EnvVaultProvider, VaultProvider};

pub struct AppBuilder {
    config: Config,
    config_path: PathBuf,
    #[allow(dead_code)]
    vault: Box<dyn VaultProvider>,
    /// Present when the vault backend is `age`. Used to pass to `create_mcp_manager_with_vault`
    /// for OAuth credential persistence across sessions.
    age_vault: Option<Arc<RwLock<AgeVaultProvider>>>,
    qdrant_ops: Option<QdrantOps>,
    /// Overlay resolved from installed plugins at startup. Available for TUI and CLI diagnostics.
    #[allow(dead_code)]
    resolved_overlay: zeph_plugins::ResolvedOverlay,
}

pub struct VaultArgs {
    pub backend: String,
    pub key_path: Option<String>,
    pub vault_path: Option<String>,
}

pub struct WatcherBundle {
    pub skill_watcher: Option<SkillWatcher>,
    pub skill_reload_rx: zeph_core::instrumented_channel::InstrumentedReceiver<SkillEvent>,
    pub config_watcher: Option<ConfigWatcher>,
    pub config_reload_rx: zeph_core::instrumented_channel::InstrumentedReceiver<ConfigEvent>,
}

impl WatcherBundle {
    /// Create a bundle with no watchers. Used in `--bare` mode.
    pub fn empty() -> Self {
        use zeph_core::instrumented_channel::instrumented_channel;
        let (_, skill_reload_rx) = instrumented_channel(1, "skill_reload_rx_bare");
        let (_, config_reload_rx) = instrumented_channel(1, "config_reload_rx_bare");
        Self {
            skill_watcher: None,
            skill_reload_rx,
            config_watcher: None,
            config_reload_rx,
        }
    }
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
        config.llm.check_legacy_format()?;

        let vault_args = parse_vault_args(
            &config,
            vault_override,
            vault_key_override,
            vault_path_override,
        );
        let (vault, age_vault): (
            Box<dyn VaultProvider>,
            Option<Arc<RwLock<AgeVaultProvider>>>,
        ) = match vault_args.backend.as_str() {
            "env" => (Box::new(EnvVaultProvider), None),
            "age" => {
                let key = vault_args.key_path.ok_or_else(|| {
                    BootstrapError::Provider("--vault-key required for age backend".into())
                })?;
                let path = vault_args.vault_path.ok_or_else(|| {
                    BootstrapError::Provider("--vault-path required for age backend".into())
                })?;
                let provider = AgeVaultProvider::new(Path::new(&key), Path::new(&path))
                    .map_err(BootstrapError::VaultInit)?;
                let arc = Arc::new(RwLock::new(provider));
                let boxed: Box<dyn VaultProvider> =
                    Box::new(zeph_core::vault::ArcAgeVaultProvider(Arc::clone(&arc)));
                (boxed, Some(arc))
            }
            other => {
                return Err(BootstrapError::Provider(format!(
                    "unknown vault backend: {other}"
                )));
            }
        };

        config.resolve_secrets(vault.as_ref()).await?;

        let resolved_overlay =
            zeph_plugins::apply_plugin_config_overlays(&mut config, &plugins_dir())
                .map_err(|e| BootstrapError::Provider(format!("plugin overlay merge: {e}")))?;

        let qdrant_ops = match config.memory.vector_backend {
            zeph_core::config::VectorBackend::Qdrant => {
                let ops = QdrantOps::new(&config.memory.qdrant_url).map_err(|e| {
                    BootstrapError::Provider(format!(
                        "invalid qdrant_url '{}': {e}",
                        config.memory.qdrant_url
                    ))
                })?;
                Some(ops)
            }
            zeph_core::config::VectorBackend::Sqlite => None,
        };

        Ok(Self {
            config,
            config_path,
            vault,
            age_vault,
            qdrant_ops,
            resolved_overlay,
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

    /// Returns the plugin config overlay resolved at startup.
    #[allow(dead_code)]
    pub fn resolved_overlay(&self) -> &zeph_plugins::ResolvedOverlay {
        &self.resolved_overlay
    }

    /// Returns the vault provider used for secret resolution.
    ///
    /// Retained as part of the public `Bootstrap` API for external callers
    /// that may inspect or override vault behavior at runtime.
    #[allow(dead_code)]
    pub fn vault(&self) -> &dyn VaultProvider {
        self.vault.as_ref()
    }

    /// Returns the shared age vault, if the backend is `age`.
    ///
    /// Pass this to `create_mcp_manager_with_vault` so OAuth tokens are persisted
    /// across sessions.
    pub fn age_vault_arc(&self) -> Option<&Arc<RwLock<AgeVaultProvider>>> {
        self.age_vault.as_ref()
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

        if let Some(ctx) = provider.context_window()
            && !matches!(provider, AnyProvider::Ollama(_))
        {
            tracing::info!(context_window = ctx, "detected provider context window");
        }

        Ok((provider, status_tx_clone, status_rx))
    }

    pub fn auto_budget_tokens(&self, provider: &AnyProvider) -> usize {
        let tokens =
            if self.config.memory.auto_budget && self.config.memory.context_budget_tokens == 0 {
                if let Some(ctx_size) = provider.context_window() {
                    tracing::info!(model_context = ctx_size, "auto-configured context budget");
                    ctx_size
                } else {
                    0
                }
            } else {
                self.config.memory.context_budget_tokens
            };
        if tokens == 0 {
            tracing::warn!(
                "context_budget_tokens resolved to 0 — using fallback of 128000 tokens to ensure compaction runs"
            );
            128_000
        } else {
            tokens
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
        // Resolve the database path: prefer database_url (PostgreSQL) over sqlite_path.
        let db_path: &str = self
            .config
            .memory
            .database_url
            .as_deref()
            .unwrap_or(&self.config.memory.sqlite_path);

        if zeph_db::is_postgres_url(db_path) {
            return Err(BootstrapError::Memory(
                "database_url points to PostgreSQL but binary was compiled with the \
                 sqlite feature. Recompile with --features postgres."
                    .to_string(),
            ));
        }

        let mut memory = match self.config.memory.vector_backend {
            zeph_core::config::VectorBackend::Sqlite => {
                SemanticMemory::with_sqlite_backend_and_pool_size(
                    db_path,
                    provider.clone(),
                    &embed_model,
                    self.config.memory.semantic.vector_weight,
                    self.config.memory.semantic.keyword_weight,
                    self.config.memory.sqlite_pool_size,
                )
                .await
                .map_err(|e| BootstrapError::Memory(e.to_string()))?
            }
            zeph_core::config::VectorBackend::Qdrant => {
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
                    db_path,
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

        memory = memory.with_importance_options(
            self.config.memory.semantic.importance_enabled,
            self.config.memory.semantic.importance_weight,
        );

        memory = memory.with_retrieval_options(
            self.config.memory.retrieval.depth,
            &self.config.memory.retrieval.search_prompt_template,
        );

        memory = memory.with_query_bias(
            self.config.memory.retrieval.query_bias_correction,
            self.config.memory.retrieval.query_bias_profile_weight,
            self.config.memory.retrieval.query_bias_centroid_ttl_secs,
        );

        memory = memory.with_hebbian(
            self.config.memory.hebbian.enabled,
            self.config.memory.hebbian.hebbian_lr,
        );

        if self.config.memory.semantic.enabled && memory.is_vector_store_connected().await {
            tracing::info!("semantic memory enabled, vector store connected");
        }

        if self.config.memory.graph.enabled {
            memory = self.attach_graph_stores(memory, db_path).await?;
        }

        // Build the dedicated embed provider before attach_reasoning_memory so the
        // Qdrant collection probe uses it. The primary router excludes embed-only entries
        // (build_all_pool_providers skips `embed = true`), so passing only `provider`
        // here would fail the probe with "no providers available" (#3375).
        let embed_provider = self.build_memory_embed_provider();
        memory = self
            .attach_reasoning_memory(memory, provider, embed_provider.as_ref())
            .await;

        if self.config.memory.admission.enabled {
            memory = memory.with_admission_control(self.build_admission_control());
        }

        if let Some(ep) = embed_provider {
            memory = memory.with_embed_provider(ep);
        }

        memory =
            memory.with_key_facts_dedup_threshold(self.config.memory.key_facts_dedup_threshold);

        Ok(memory)
    }

    /// Attach `GraphStore` and optionally `ExperienceStore` to the in-progress `SemanticMemory`.
    ///
    /// Opens a dedicated pool for graph operations to prevent pool starvation from community
    /// detection and spreading activation. When `experience.enabled`, the pool is shared with
    /// `ExperienceStore` (both are DB-maintenance workloads on graph tables).
    ///
    /// # Errors
    ///
    /// Returns [`BootstrapError::Memory`] if the pool cannot be opened.
    async fn attach_graph_stores(
        &self,
        memory: SemanticMemory,
        db_path: &str,
    ) -> Result<SemanticMemory, BootstrapError> {
        // Open a dedicated pool — community detection can saturate the shared message pool
        // (pool_size=5), causing pool.acquire() cancellation and semaphore drift in sqlx 0.8.
        let graph_pool = zeph_db::DbConfig {
            url: db_path.to_string(),
            max_connections: self.config.memory.graph.pool_size,
            pool_size: self.config.memory.graph.pool_size,
        }
        .connect()
        .await
        .map_err(|e| BootstrapError::Memory(e.to_string()))?;

        // Clone the pool so ExperienceStore can share it when enabled.
        // sqlx Pool is cheaply cloneable (internally Arc-backed).
        let store = Arc::new(GraphStore::new(graph_pool.clone()));
        let mut memory = memory.with_graph_store(store);
        tracing::info!(
            pool_size = self.config.memory.graph.pool_size,
            "graph memory enabled, GraphStore attached with dedicated pool"
        );

        if self.config.memory.graph.experience.enabled {
            let exp_store = Arc::new(zeph_memory::ExperienceStore::new(graph_pool));
            memory = memory.with_experience_store(exp_store);
            let exp_cfg = &self.config.memory.graph.experience;
            tracing::info!(
                evolution_sweep_enabled = exp_cfg.evolution_sweep_enabled,
                evolution_sweep_interval = exp_cfg.evolution_sweep_interval,
                confidence_prune_threshold = exp_cfg.confidence_prune_threshold,
                "experience memory enabled",
            );
        }

        Ok(memory)
    }

    /// Attach [`zeph_memory::ReasoningMemory`] to the `SemanticMemory` when `memory.reasoning.enabled`.
    ///
    /// Uses the main `SQLite` pool from `SemanticMemory` so no additional pool is needed.
    /// When Qdrant is configured, the `QdrantOps` is cloned and passed as the vector store
    /// for embedding-similarity retrieval. The `reasoning_strategies` collection is created
    /// at startup with the correct vector dimension probed from `embed_provider` when present,
    /// falling back to `provider`. The dedicated embed provider must be passed here because the
    /// primary router excludes `embed = true` pool entries and cannot produce embeddings (#3375).
    pub async fn attach_reasoning_memory(
        &self,
        memory: SemanticMemory,
        provider: &AnyProvider,
        embed_provider: Option<&AnyProvider>,
    ) -> SemanticMemory {
        if !self.config.memory.reasoning.enabled {
            return memory;
        }

        let pool = memory.sqlite().pool().clone();

        // Wire Qdrant vector store when available so retrieval is live (C1 fix).
        let vector_store: Option<std::sync::Arc<dyn zeph_memory::VectorStore>> = if let Some(ops) =
            &self.qdrant_ops
        {
            let ops_arc: std::sync::Arc<dyn zeph_memory::VectorStore> = Arc::new(ops.clone());

            // Use the dedicated embed provider for the dimension probe when available.
            // The primary router skips embed-only pool entries, so it cannot produce
            // embeddings when the config uses a separate embedder (#3375).
            let probe_provider = embed_provider.unwrap_or(provider);

            // Ensure the reasoning_strategies collection exists with the correct vector size.
            // Best-effort: a failure here is logged and Qdrant falls back to SQLite-only mode.
            if probe_provider.supports_embeddings() {
                match probe_provider.embed("dimension probe").await {
                    Ok(probe) => {
                        let vector_size = u64::try_from(probe.len()).unwrap_or(1536);
                        if let Err(e) = ops
                            .ensure_collection(
                                zeph_memory::reasoning::REASONING_COLLECTION,
                                vector_size,
                            )
                            .await
                        {
                            tracing::warn!(
                                error = %e,
                                collection = zeph_memory::reasoning::REASONING_COLLECTION,
                                "reasoning: ensure_collection failed — Qdrant retrieval disabled"
                            );
                        } else {
                            tracing::info!(
                                collection = zeph_memory::reasoning::REASONING_COLLECTION,
                                vector_size,
                                "reasoning: Qdrant collection ready"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "reasoning: embed probe failed — cannot ensure Qdrant collection"
                        );
                    }
                }
            }

            Some(ops_arc)
        } else {
            None
        };

        let reasoning = Arc::new(zeph_memory::ReasoningMemory::new(pool, vector_store));
        let mem = memory.with_reasoning(reasoning);
        tracing::info!(
            qdrant_wired = self.qdrant_ops.is_some(),
            "reasoning bank enabled, ReasoningMemory attached"
        );
        mem
    }

    /// Build a minimal ephemeral memory for bare mode.
    ///
    /// Uses an in-process `SQLite` `:memory:` database with no Qdrant, no graph store,
    /// and no admission control. Avoids all file-system and network I/O at startup.
    pub async fn build_bare_memory(
        &self,
        provider: &AnyProvider,
    ) -> Result<SemanticMemory, BootstrapError> {
        let embed_model = self.embedding_model();
        SemanticMemory::with_sqlite_backend_and_pool_size(
            ":memory:",
            provider.clone(),
            &embed_model,
            self.config.memory.semantic.vector_weight,
            self.config.memory.semantic.keyword_weight,
            1,
        )
        .await
        .map_err(|e| BootstrapError::Memory(e.to_string()))
    }

    fn build_memory_embed_provider(&self) -> Option<AnyProvider> {
        let name = self
            .config
            .memory
            .semantic
            .embed_provider
            .as_deref()
            .filter(|s| !s.is_empty())?;

        match create_named_provider(name, &self.config) {
            Ok(ep) => {
                tracing::info!(provider = %name, "Using dedicated embed provider for memory backfill");
                Some(ep)
            }
            Err(e) => {
                tracing::warn!(
                    provider = %name,
                    error = %e,
                    "Memory embed_provider resolution failed — main provider will be used"
                );
                None
            }
        }
    }
}

/// Spawn a background task that backfills missing embeddings.
///
/// Fire-and-forget: the caller does not need to await the returned handle.
/// The task runs for at most `timeout_secs` seconds.
///
/// # Errors
///
/// The returned `JoinHandle` resolves to `()` — errors are logged internally.
pub fn spawn_embed_backfill(
    memory: Arc<SemanticMemory>,
    timeout_secs: u64,
    progress_tx: Option<
        tokio::sync::watch::Sender<Option<zeph_memory::semantic::BackfillProgress>>,
    >,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            memory.embed_missing(progress_tx.clone()),
        )
        .await;
        match result {
            Ok(Ok(n)) if n > 0 => tracing::info!("backfilled {n} missing embedding(s)"),
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::warn!("embed_missing failed: {e:#}"),
            Err(_) => tracing::warn!("embed_missing timed out after {timeout_secs}s"),
        }
        // Ensure progress signals done on timeout/error.
        if let Some(tx) = progress_tx {
            let _ = tx.send(None);
        }
    })
}

impl AppBuilder {
    fn build_admission_control(&self) -> zeph_memory::AdmissionControl {
        let w = &self.config.memory.admission.weights;
        let weights = zeph_memory::AdmissionWeights {
            future_utility: w.future_utility,
            factual_confidence: w.factual_confidence,
            semantic_novelty: w.semantic_novelty,
            temporal_recency: w.temporal_recency,
            content_type_prior: w.content_type_prior,
            goal_utility: w.goal_utility,
        };
        let mut control = zeph_memory::AdmissionControl::new(
            self.config.memory.admission.threshold,
            self.config.memory.admission.fast_path_margin,
            weights,
        );
        if !self.config.memory.admission.admission_provider.is_empty() {
            match create_named_provider(
                &self.config.memory.admission.admission_provider,
                &self.config,
            ) {
                Ok(p) => {
                    tracing::info!(
                        provider = %p.name(),
                        "A-MAC admission provider configured"
                    );
                    control = control.with_provider(p);
                }
                Err(e) => {
                    tracing::warn!(
                        provider = %self.config.memory.admission.admission_provider,
                        error = %e,
                        "A-MAC admission provider resolution failed — embed provider will be used as fallback"
                    );
                    // intentionally no .with_provider() — evaluate() will use fallback embed provider
                }
            }
        }

        if self.config.memory.admission.goal_conditioned_write {
            let goal_provider = if self
                .config
                .memory
                .admission
                .goal_utility_provider
                .is_empty()
            {
                None
            } else {
                match create_named_provider(
                    &self.config.memory.admission.goal_utility_provider,
                    &self.config,
                ) {
                    Ok(p) => Some(p),
                    Err(e) => {
                        tracing::warn!(
                            provider = %self.config.memory.admission.goal_utility_provider,
                            error = %e,
                            "goal_utility_provider not found, LLM refinement disabled"
                        );
                        None
                    }
                }
            };
            control = control.with_goal_gate(zeph_memory::GoalGateConfig {
                threshold: self.config.memory.admission.goal_utility_threshold,
                provider: goal_provider,
                weight: self.config.memory.admission.goal_utility_weight,
            });
            tracing::info!(
                threshold = self.config.memory.admission.goal_utility_threshold,
                weight = self.config.memory.admission.goal_utility_weight,
                "A-MAC: goal-conditioned write gate enabled"
            );
        }

        if self.config.memory.admission.admission_strategy == zeph_config::AdmissionStrategy::Rl {
            tracing::warn!(
                "admission_strategy = \"rl\" is configured but the RL model is not yet wired \
                 into the admission path — falling back to heuristic. See #2416."
            );
        }

        tracing::info!(
            threshold = self.config.memory.admission.threshold,
            "A-MAC admission control enabled"
        );
        control
    }

    pub async fn build_skill_matcher(
        &self,
        provider: &AnyProvider,
        meta: &[&SkillMeta],
        memory: &SemanticMemory,
    ) -> Option<SkillMatcherBackend> {
        // Use the stable model name derived from the resolved embed provider entry so that
        // `model_has_changed` in `EmbeddingRegistry` does not trigger a collection rebuild on
        // every restart when `effective_embedding_model` returns an unstable or empty string.
        let embed_model = stable_skill_embedding_model(&self.config);
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
        let managed = managed_skills_dir();
        {
            match zeph_skills::bundled::provision_bundled_skills(&managed) {
                Ok(report) => {
                    if !report.installed.is_empty() {
                        tracing::info!(
                            skills = ?report.installed,
                            "provisioned new bundled skills"
                        );
                    }
                    if !report.updated.is_empty() {
                        tracing::info!(
                            skills = ?report.updated,
                            "updated bundled skills"
                        );
                    }
                    for (name, err) in &report.failed {
                        tracing::warn!(skill = %name, error = %err, "failed to provision bundled skill");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "bundled skill provisioning failed");
                }
            }
        }

        let skill_paths = self.skill_paths_for_registry();
        let registry = SkillRegistry::load(&skill_paths).with_hub_dirs(std::iter::once(managed));

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

        if self.config.skills.trust.scanner.capability_escalation_check {
            // Build a trust-level mapping from all loaded skill metas.
            // Skills without a trust record default to the configured default_level.
            let default_level = self.config.skills.trust.default_level;
            let trust_levels: Vec<(String, zeph_tools::SkillTrustLevel)> = registry
                .all_meta()
                .iter()
                .map(|meta| (meta.name.clone(), default_level))
                .collect();

            let violations = registry.check_escalations(&trust_levels);
            for v in &violations {
                tracing::warn!(
                    skill = %v.skill_name,
                    denied_tools = ?v.denied_tools,
                    "capability escalation: skill declares tools exceeding its trust level"
                );
            }
            if violations.is_empty() {
                tracing::debug!("capability escalation check: no violations found");
            }
        }

        registry
    }

    /// Returns per-plugin skill directories expanded via `PluginManager::collect_skill_dirs`.
    ///
    /// Used by [`Self::build_registry`] and by runner/daemon/acp when constructing the agent
    /// via `with_skill_reload`. Every entry points directly at a directory containing `SKILL.md`.
    pub fn skill_paths_for_registry(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self.config.skills.paths.iter().map(PathBuf::from).collect();
        let managed_dir = managed_skills_dir();
        if !paths.contains(&managed_dir) {
            paths.push(managed_dir.clone());
        }
        let plugins_dir = plugins_dir();
        let mgr = zeph_plugins::PluginManager::new(
            plugins_dir,
            managed_dir,
            self.config.mcp.allowed_commands.clone(),
            self.config.tools.shell.allowed_commands.clone(),
        );
        if let Ok(plugin_skill_dirs) = mgr.collect_skill_dirs() {
            for dir in plugin_skill_dirs {
                if !paths.contains(&dir) {
                    paths.push(dir);
                }
            }
        }
        paths
    }

    /// Returns paths for the filesystem watcher: config paths + managed dir + plugins root.
    ///
    /// Passes the plugins root (not per-plugin subdirs) so that skills added by `/plugins add`
    /// after startup are covered by the recursive watcher without re-registration.
    /// Eagerly creates the plugins root directory so [`SkillWatcher::start`] does not fail on a
    /// clean install where no plugins have been installed yet.
    pub fn skill_paths_for_watcher(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self.config.skills.paths.iter().map(PathBuf::from).collect();
        let managed_dir = managed_skills_dir();
        if !paths.contains(&managed_dir) {
            paths.push(managed_dir);
        }
        let plugins_root = plugins_dir();
        let _ = std::fs::create_dir_all(&plugins_root).map_err(|e| {
            tracing::warn!(
                path = %plugins_root.display(),
                error = %e,
                "failed to create plugins directory for watcher; skipping"
            );
        });
        if plugins_root.exists() && !paths.contains(&plugins_root) {
            paths.push(plugins_root);
        }
        paths
    }

    /// Returns a closure that resolves current per-plugin skill directories.
    ///
    /// Pass the returned closure to `AgentBuilder::with_plugin_dirs_supplier` so that
    /// `reload_skills()` picks up plugins installed after agent startup.
    ///
    /// Both `mcp.allowed_commands` and `tools.shell.allowed_commands` are
    /// captured by value at construction time. If the operator reloads
    /// config at runtime, this supplier must be rebuilt.
    pub fn plugin_dirs_supplier(
        &self,
    ) -> impl Fn() -> Vec<std::path::PathBuf> + Send + Sync + 'static {
        let plugins_dir = plugins_dir();
        let managed_dir = managed_skills_dir();
        let mcp_allowed = self.config.mcp.allowed_commands.clone();
        let base_shell_allowed = self.config.tools.shell.allowed_commands.clone();
        move || {
            let mgr = zeph_plugins::PluginManager::new(
                plugins_dir.clone(),
                managed_dir.clone(),
                mcp_allowed.clone(),
                base_shell_allowed.clone(),
            );
            mgr.collect_skill_dirs().unwrap_or_default()
        }
    }

    #[allow(dead_code)]
    pub fn managed_skills_dir() -> PathBuf {
        managed_skills_dir()
    }

    pub fn build_watchers(&self) -> WatcherBundle {
        use zeph_core::instrumented_channel::instrumented_channel;

        let skill_paths = self.skill_paths_for_watcher();
        let (skill_tx, skill_reload_rx) = instrumented_channel(4, "skill_reload_rx");
        let skill_watcher = match SkillWatcher::start(&skill_paths, skill_tx.into_inner()) {
            Ok(w) => {
                tracing::info!("skill watcher started");
                Some(w)
            }
            Err(e) => {
                tracing::warn!("skill watcher unavailable: {e:#}");
                None
            }
        };

        let (config_tx, config_reload_rx) = instrumented_channel(4, "config_reload_rx");
        let config_watcher = match ConfigWatcher::start(&self.config_path, config_tx.into_inner()) {
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
        if let Some(ref entry) = self.config.llm.summary_provider {
            return match build_provider_from_entry(entry, &self.config) {
                Ok(sp) => {
                    tracing::info!(
                        provider_type = ?entry.provider_type,
                        model = ?entry.model,
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
    ) -> Option<(AnyProvider, zeph_sanitizer::QuarantineConfig)> {
        let ci = &self.config.security.content_isolation;
        let qc = &ci.quarantine;
        if !qc.enabled {
            if ci.mcp_to_acp_boundary {
                tracing::warn!(
                    "mcp_to_acp_boundary is enabled but quarantine is disabled — \
                     cross-boundary MCP tool results in ACP sessions will be \
                     spotlighted but NOT quarantine-summarized; enable \
                     [security.content_isolation.quarantine] for full protection"
                );
            }
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
    #[allow(dead_code)]
    pub fn build_guardrail_filter(&self) -> Option<zeph_sanitizer::guardrail::GuardrailFilter> {
        let (provider, config) = self.build_guardrail_provider()?;
        match zeph_sanitizer::guardrail::GuardrailFilter::new(provider, &config) {
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
    pub fn build_guardrail_provider(
        &self,
    ) -> Option<(AnyProvider, zeph_sanitizer::guardrail::GuardrailConfig)> {
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
        use zeph_core::config::DetectorMode;
        let learning = &self.config.skills.learning;
        if learning.detector_mode != DetectorMode::Judge {
            return None;
        }
        if learning.judge_model.is_empty() {
            tracing::warn!(
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

    /// Build an `LlmClassifier` for `detector_mode = "model"` feedback detection.
    ///
    /// Resolves `feedback_provider` from `[[llm.providers]]` registry.
    /// Pass the session's primary provider as `primary` for fallback when `feedback_provider`
    /// is empty. Returns `None` with a warning on resolution failure — never fails startup.
    pub fn build_feedback_classifier(
        &self,
        primary: &AnyProvider,
    ) -> Option<zeph_llm::classifier::llm::LlmClassifier> {
        use zeph_core::config::DetectorMode;
        let learning = &self.config.skills.learning;
        if learning.detector_mode != DetectorMode::Model {
            return None;
        }
        let provider = if learning.feedback_provider.is_empty() {
            tracing::debug!("feedback_provider empty — using primary provider for LlmClassifier");
            Some(primary.clone())
        } else {
            match crate::bootstrap::provider::create_named_provider(
                &learning.feedback_provider,
                &self.config,
            ) {
                Ok(p) => {
                    tracing::info!(
                        provider = %learning.feedback_provider,
                        "LlmClassifier feedback provider configured"
                    );
                    Some(p)
                }
                Err(e) => {
                    tracing::warn!(
                        provider = %learning.feedback_provider,
                        error = %e,
                        "feedback_provider not found in registry, degrading to regex-only"
                    );
                    None
                }
            }
        };
        if let Some(p) = provider {
            Some(zeph_llm::classifier::llm::LlmClassifier::new(
                std::sync::Arc::new(p),
            ))
        } else {
            tracing::warn!(
                "detector_mode=model but no provider available, degrading to regex-only"
            );
            None
        }
    }

    /// Build a dedicated provider for compaction probe LLM calls.
    ///
    /// Returns `None` when `probe_provider` is empty (falls back to summary provider at call site).
    /// Emits a `tracing::warn` on resolution failure (summary/primary provider used as fallback).
    pub fn build_probe_provider(&self) -> Option<AnyProvider> {
        let name = &self.config.memory.compression.probe.probe_provider;
        if name.is_empty() {
            return None;
        }
        match create_named_provider(name, &self.config) {
            Ok(p) => {
                tracing::info!(provider = %name, "compaction probe provider configured");
                Some(p)
            }
            Err(e) => {
                tracing::warn!(
                    provider = %name,
                    error = %e,
                    "probe provider resolution failed — summary/primary provider will be used"
                );
                None
            }
        }
    }

    /// Build a dedicated provider for `compress_context` LLM calls (#2356).
    ///
    /// Returns `None` when `compress_provider` is empty (falls back to primary provider at call site).
    /// Emits a `tracing::warn` on resolution failure (primary provider used as fallback).
    pub fn build_compress_provider(&self) -> Option<AnyProvider> {
        let name = &self.config.memory.compression.compress_provider;
        if name.is_empty() {
            return None;
        }
        match create_named_provider(name, &self.config) {
            Ok(p) => {
                tracing::info!(provider = %name, "compress_context provider configured");
                Some(p)
            }
            Err(e) => {
                tracing::warn!(
                    provider = %name,
                    error = %e,
                    "compress_context provider resolution failed — primary provider will be used"
                );
                None
            }
        }
    }

    /// Build a dedicated provider for ACON compression guidelines LLM calls.
    ///
    /// Returns `None` when `guidelines_provider` is empty (falls back to primary provider at call site).
    ///
    /// # Errors (logged, not propagated)
    ///
    /// Emits a `tracing::warn` on resolution failure; primary provider is used as fallback.
    pub fn build_guidelines_provider(&self) -> Option<AnyProvider> {
        let name = &self
            .config
            .memory
            .compression_guidelines
            .guidelines_provider;
        if name.is_empty() {
            return None;
        }
        match create_named_provider(name, &self.config) {
            Ok(p) => {
                tracing::info!(provider = %name, "compression guidelines provider configured");
                Some(p)
            }
            Err(e) => {
                tracing::warn!(
                    provider = %name,
                    error = %e,
                    "guidelines provider resolution failed — primary provider will be used"
                );
                None
            }
        }
    }

    /// Build a dedicated provider for All-Mem consolidation LLM calls.
    ///
    /// Returns `None` when `consolidation_provider` is empty (falls back to primary provider at
    /// call site) or when provider resolution fails (logs a warning, fails open).
    pub fn build_consolidation_provider(&self) -> Option<AnyProvider> {
        let name = &self.config.memory.consolidation.consolidation_provider;
        if name.is_empty() {
            return None;
        }
        match create_named_provider(name, &self.config) {
            Ok(p) => {
                tracing::info!(provider = %name, "consolidation provider configured");
                Some(p)
            }
            Err(e) => {
                tracing::warn!(
                    provider = %name,
                    error = %e,
                    "consolidation provider resolution failed — primary provider will be used"
                );
                None
            }
        }
    }

    /// Build a dedicated provider for Hebbian cluster distillation LLM calls (HL-F4, #3345).
    ///
    /// Returns `None` when `consolidate_provider` is empty or resolution fails; the
    /// caller falls back to the primary provider.
    pub fn build_hebbian_consolidation_provider(&self) -> Option<AnyProvider> {
        let name = &self.config.memory.hebbian.consolidate_provider;
        if name.is_empty() {
            return None;
        }
        match create_named_provider(name, &self.config) {
            Ok(p) => {
                tracing::info!(provider = %name, "Hebbian consolidation provider configured");
                Some(p)
            }
            Err(e) => {
                tracing::warn!(
                    provider = %name,
                    error = %e,
                    "Hebbian consolidation provider resolution failed — primary provider will be used"
                );
                None
            }
        }
    }

    /// Build a dedicated provider for `TiMem` tree consolidation LLM calls (#2262).
    ///
    /// Returns `None` when `consolidation_provider` is empty or resolution fails.
    pub fn build_tree_consolidation_provider(&self) -> Option<AnyProvider> {
        let name = &self.config.memory.tree.consolidation_provider;
        if name.is_empty() {
            return None;
        }
        match create_named_provider(name, &self.config) {
            Ok(p) => {
                tracing::info!(provider = %name, "tree consolidation provider configured");
                Some(p)
            }
            Err(e) => {
                tracing::warn!(
                    provider = %name,
                    error = %e,
                    "tree consolidation provider resolution failed — primary provider will be used"
                );
                None
            }
        }
    }

    /// Build a dedicated provider for orchestration planner LLM calls.
    ///
    /// Returns `None` when `planner_provider` is empty (falls back to primary provider at call site).
    ///
    /// # Errors (logged, not propagated)
    ///
    /// Emits a `tracing::warn` on resolution failure; primary provider is used as fallback.
    pub fn build_planner_provider(&self) -> Option<AnyProvider> {
        let name = &self.config.orchestration.planner_provider;
        if name.is_empty() {
            return None;
        }
        match create_named_provider(name, &self.config) {
            Ok(p) => {
                tracing::info!(provider = %name, "planner provider configured");
                Some(p)
            }
            Err(e) => {
                tracing::warn!(
                    provider = %name,
                    error = %e,
                    "planner provider resolution failed — primary provider will be used"
                );
                None
            }
        }
    }

    /// Build a `TopologyAdvisor` when `[orchestration.adaptorch]` is enabled.
    ///
    /// Returns `None` when disabled or when the classify provider cannot be resolved.
    pub fn build_topology_advisor(
        &self,
    ) -> Option<std::sync::Arc<zeph_orchestration::TopologyAdvisor>> {
        let cfg = &self.config.orchestration.adaptorch;
        if !cfg.enabled {
            return None;
        }
        let classify_provider = if cfg.topology_provider.is_empty() {
            match create_named_provider(
                &self.config.llm.providers.first()?.effective_name(),
                &self.config,
            ) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "adaptorch: cannot resolve classify provider");
                    return None;
                }
            }
        } else {
            match create_named_provider(cfg.topology_provider.as_str(), &self.config) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        provider = %cfg.topology_provider.as_str(),
                        error = %e,
                        "adaptorch: classify provider resolution failed"
                    );
                    return None;
                }
            }
        };
        let state_path = if cfg.state_path.is_empty() {
            std::path::PathBuf::new()
        } else {
            std::path::PathBuf::from(&cfg.state_path)
        };
        let timeout = std::time::Duration::from_secs(cfg.classify_timeout_secs);
        tracing::info!(
            provider = %cfg.topology_provider.as_str(),
            timeout_secs = cfg.classify_timeout_secs,
            "adaptorch: topology advisor initialized"
        );
        Some(std::sync::Arc::new(
            zeph_orchestration::TopologyAdvisor::new(
                std::sync::Arc::new(classify_provider),
                state_path,
                timeout,
            ),
        ))
    }

    /// Build the `PlanVerifier` provider from `[orchestration] verify_provider`.
    ///
    /// Returns `None` when `verify_provider` is empty (falls back to the primary provider at
    /// runtime) or when provider resolution fails (logs a warning, fails open).
    pub fn build_verify_provider(&self) -> Option<AnyProvider> {
        let name = &self.config.orchestration.verify_provider;
        if name.is_empty() {
            return None;
        }
        match create_named_provider(name, &self.config) {
            Ok(p) => {
                tracing::info!(provider = %name, "verify provider configured");
                Some(p)
            }
            Err(e) => {
                tracing::warn!(
                    provider = %name,
                    error = %e,
                    "verify provider resolution failed — primary provider will be used"
                );
                None
            }
        }
    }
    pub fn build_eval_provider(&self) -> Option<AnyProvider> {
        let model_spec = self.config.experiments.eval_model.as_deref()?;
        match create_summary_provider(model_spec, &self.config) {
            Ok(p) => {
                tracing::info!(eval_model = %model_spec, "experiment eval provider configured");
                Some(p)
            }
            Err(e) => {
                tracing::warn!(
                    eval_model = %model_spec,
                    error = %e,
                    "failed to create eval provider — primary provider will be used as judge"
                );
                None
            }
        }
    }

    /// Build a dedicated provider for `MemScene` label/profile LLM generation.
    ///
    /// Returns `None` when `tiers.scene_provider` is empty (caller falls back to primary provider).
    /// Emits a `tracing::warn` on resolution failure; primary provider is used as fallback.
    pub fn build_scene_provider(&self) -> Option<AnyProvider> {
        let name = &self.config.memory.tiers.scene_provider;
        if name.is_empty() {
            return None;
        }
        match create_named_provider(name, &self.config) {
            Ok(p) => {
                tracing::info!(provider = %name, "scene consolidation provider configured");
                Some(p)
            }
            Err(e) => {
                tracing::warn!(
                    provider = %name,
                    error = %e,
                    "scene provider resolution failed — primary provider will be used"
                );
                None
            }
        }
    }

    #[cfg(test)]
    pub fn for_test(config: zeph_core::config::Config) -> Self {
        Self {
            config,
            config_path: std::path::PathBuf::new(),
            vault: Box::new(zeph_core::vault::EnvVaultProvider),
            age_vault: None,
            qdrant_ops: None,
            resolved_overlay: zeph_plugins::ResolvedOverlay::default(),
        }
    }
}

#[cfg(test)]
mod tests;
