// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

pub use crate::provider_factory::effective_embedding_model;

use std::path::PathBuf;
use zeph_llm::any::AnyProvider;
use zeph_memory::QdrantOps;
use zeph_memory::semantic::SemanticMemory;
use zeph_skills::loader::SkillMeta;
use zeph_skills::matcher::{SkillMatcher, SkillMatcherBackend};
use zeph_skills::qdrant_matcher::QdrantSkillMatcher;

use crate::config::Config;

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
    crate::vault::default_vault_dir().join("skills")
}
