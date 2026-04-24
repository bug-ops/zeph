// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

pub use zeph_core::provider_factory::effective_embedding_model;

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use zeph_llm::any::AnyProvider;
use zeph_memory::QdrantOps;
use zeph_memory::semantic::SemanticMemory;
use zeph_skills::loader::SkillMeta;
use zeph_skills::matcher::{SkillMatcher, SkillMatcherBackend};
use zeph_skills::qdrant_matcher::QdrantSkillMatcher;

use zeph_core::config::Config;

#[allow(unused_variables)]
pub async fn create_skill_matcher(
    config: &Config,
    provider: &AnyProvider,
    meta: &[&SkillMeta],
    memory: &SemanticMemory,
    embedding_model: &str,
    qdrant_ops: Option<&QdrantOps>,
) -> Option<SkillMatcherBackend> {
    let inner_embed = provider.embed_fn();
    let embed_timeout = std::time::Duration::from_secs(config.timeouts.embedding_seconds);
    let embed_fn = move |text: &str| -> zeph_llm::provider::EmbedFuture {
        let fut = inner_embed(text);
        Box::pin(async move {
            if let Ok(result) = tokio::time::timeout(embed_timeout, fut).await {
                result
            } else {
                tracing::warn!(
                    timeout_secs = embed_timeout.as_secs(),
                    "skill matcher: embedding probe timed out"
                );
                Err(zeph_llm::LlmError::Timeout)
            }
        })
    };

    if config.memory.semantic.enabled
        && memory.is_vector_store_connected().await
        && let Some(ops) = qdrant_ops
    {
        let mut qm = QdrantSkillMatcher::with_ops(ops.clone());
        match qm.sync(meta, embedding_model, &embed_fn, None).await {
            Ok(_) => return Some(SkillMatcherBackend::Qdrant(qm)),
            Err(e) => {
                tracing::warn!("Qdrant skill sync failed, falling back to in-memory: {e:#}");
            }
        }
    }

    SkillMatcher::new(meta, &embed_fn)
        .await
        .map(SkillMatcherBackend::InMemory)
}

/// Resolve the dedicated embedding provider from `[[llm.providers]]`.
///
/// Prefers the entry with `embed = true`; falls back to the first entry that has
/// `embedding_model` set; finally falls back to `primary`. This provider is stored
/// separately from the chat provider and is **never replaced** by `/provider switch`.
pub fn create_embedding_provider(config: &Config, primary: &AnyProvider) -> AnyProvider {
    // Find a dedicated embed entry.
    let embed_entry = config.llm.providers.iter().find(|e| e.embed).or_else(|| {
        config
            .llm
            .providers
            .iter()
            .find(|e| e.embedding_model.is_some())
    });

    let Some(entry) = embed_entry else {
        return primary.clone();
    };

    match crate::bootstrap::build_provider_from_entry(entry, config) {
        Ok(p) => {
            tracing::debug!(
                provider = entry.effective_name(),
                "embedding provider resolved from [[llm.providers]]"
            );
            p
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to build embedding provider, falling back to primary"
            );
            primary.clone()
        }
    }
}

/// Returns the default managed skills directory: `~/.config/zeph/skills/`.
pub fn managed_skills_dir() -> PathBuf {
    zeph_core::vault::default_vault_dir().join("skills")
}

/// Returns the default plugins directory: `~/.local/share/zeph/plugins/`.
///
/// Delegates to [`zeph_plugins::PluginManager::default_plugins_dir`] so there is a single
/// canonical source of truth used by both the CLI and the TUI path in `zeph-core`.
pub fn plugins_dir() -> PathBuf {
    zeph_plugins::PluginManager::default_plugins_dir()
}

/// Build a [`zeph_skills::evaluator::SkillEvaluator`] from `[skills.evaluation]` config.
///
/// Returns `None` when `config.skills.evaluation.enabled = false`.
/// On provider resolution failure falls back to `primary` and logs a warning.
///
/// # Examples
///
/// ```rust,no_run
/// # use zeph_llm::any::AnyProvider;
/// # use zeph_core::config::Config;
/// # use std::path::Path;
/// # let config = Config::load(Path::new("/nonexistent")).unwrap();
/// # let provider = AnyProvider::Mock(zeph_llm::mock::MockProvider::default());
/// let evaluator = crate::bootstrap::skills::build_skill_evaluator(&config, &provider);
/// ```
pub fn build_skill_evaluator(
    config: &Config,
    primary: &AnyProvider,
) -> Option<Arc<zeph_skills::evaluator::SkillEvaluator>> {
    let eval_cfg = &config.skills.evaluation;
    if !eval_cfg.enabled {
        return None;
    }

    let critic = if eval_cfg.provider.is_empty() {
        primary.clone()
    } else {
        match crate::bootstrap::create_named_provider(&eval_cfg.provider, config) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    provider = %eval_cfg.provider,
                    error = %e,
                    "skill evaluator provider resolution failed, falling back to primary"
                );
                primary.clone()
            }
        }
    };

    let weights = zeph_skills::evaluator::EvaluationWeights {
        correctness: eval_cfg.weight_correctness,
        reusability: eval_cfg.weight_reusability,
        specificity: eval_cfg.weight_specificity,
    };

    Some(Arc::new(zeph_skills::evaluator::SkillEvaluator::new(
        critic,
        weights,
        eval_cfg.quality_threshold,
        eval_cfg.fail_open_on_error,
        eval_cfg.timeout_ms,
    )))
}

/// `SkillWriter` implementation that delegates to a `SkillGenerator`.
///
/// Bridges `zeph-memory`'s `SkillWriter` trait (which cannot depend on `zeph-skills`)
/// to the concrete `SkillGenerator` in `zeph-skills`. Defined in the binary crate to
/// avoid the circular dependency `zeph-memory` ↔ `zeph-skills`.
struct GeneratorSkillWriter {
    /// Provider used to build a fresh `SkillGenerator` per call.
    provider: AnyProvider,
    /// Output directory for generated SKILL.md files.
    output_dir: PathBuf,
    /// Optional quality gate — forwarded to the generator via `with_evaluator`.
    evaluator: Option<Arc<zeph_skills::evaluator::SkillEvaluator>>,
    /// Evaluation weights forwarded to `with_evaluator`.
    eval_weights: zeph_skills::evaluator::EvaluationWeights,
    /// Evaluation threshold forwarded to `with_evaluator`.
    eval_threshold: f32,
}

impl zeph_memory::compression::promotion::SkillWriter for GeneratorSkillWriter {
    fn write_skill(
        &self,
        description: String,
        signature: String,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            let generator =
                zeph_skills::SkillGenerator::new(self.provider.clone(), self.output_dir.clone());
            let generator = if let Some(ref eval) = self.evaluator {
                generator.with_evaluator(Arc::clone(eval), self.eval_weights, self.eval_threshold)
            } else {
                generator
            };

            let req = zeph_skills::SkillGenerationRequest {
                description: description.clone(),
                category: None,
                allowed_tools: vec![],
            };
            let generated = generator.generate(req).await.map_err(|e| e.to_string())?;

            // Use the signature as idempotency key: skip write if skill dir already exists.
            let skill_dir = self.output_dir.join(format!(
                "promoted-pattern-{}",
                &signature[..12.min(signature.len())]
            ));
            if skill_dir.exists() {
                return Ok(());
            }

            generator
                .approve_and_save(&generated)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    }
}

/// Build an `Arc<dyn SkillWriter>` backed by a `SkillGenerator`.
///
/// Returns `None` when the promotion engine is disabled or the output directory cannot be
/// determined. On provider resolution failure falls back to `primary`.
pub fn build_skill_writer(
    config: &Config,
    primary: &AnyProvider,
    evaluator: Option<Arc<zeph_skills::evaluator::SkillEvaluator>>,
    eval_weights: zeph_skills::evaluator::EvaluationWeights,
    eval_threshold: f32,
    skills_paths: &[PathBuf],
) -> Option<Arc<dyn zeph_memory::compression::promotion::SkillWriter>> {
    let spectrum_cfg = &config.memory.compression_spectrum;
    if !spectrum_cfg.enabled {
        return None;
    }

    let output_dir = if let Some(ref dir) = spectrum_cfg.promotion_output_dir {
        PathBuf::from(dir)
    } else if let Some(first) = skills_paths.first() {
        first.join("promoted")
    } else {
        managed_skills_dir().join("promoted")
    };

    let provider = if spectrum_cfg.promotion_provider.is_empty() {
        primary.clone()
    } else {
        match crate::bootstrap::create_named_provider(&spectrum_cfg.promotion_provider, config) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    provider = %spectrum_cfg.promotion_provider,
                    error = %e,
                    "promotion provider resolution failed, falling back to primary"
                );
                primary.clone()
            }
        }
    };

    Some(Arc::new(GeneratorSkillWriter {
        provider,
        output_dir,
        evaluator,
        eval_weights,
        eval_threshold,
    }))
}
