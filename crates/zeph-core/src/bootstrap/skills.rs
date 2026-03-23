// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

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
    let embed_fn = provider.embed_fn();

    if config.memory.semantic.enabled
        && memory.is_vector_store_connected().await
        && let Some(ops) = qdrant_ops
    {
        let mut qm = QdrantSkillMatcher::with_ops(ops.clone());
        match qm.sync(meta, embedding_model, &embed_fn).await {
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

/// Returns the default managed skills directory: `~/.config/zeph/skills/`.
pub fn managed_skills_dir() -> PathBuf {
    crate::vault::default_vault_dir().join("skills")
}
