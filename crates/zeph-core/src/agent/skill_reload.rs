// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Hot-reload of skills and instructions.
//!
//! Extracted from `agent/mod.rs` (#4923). Rebuilds the skill matcher, refreshes
//! per-skill trust scores, and reloads `instructions`/`AGENTS.md` overlays when the
//! filesystem watcher signals a change.

use std::collections::HashMap;
use std::sync::Arc;

use super::{Agent, state};
use crate::channel::Channel;
use crate::context::build_system_prompt;
use zeph_llm::provider::LlmProvider;
use zeph_skills::loader::Skill;
use zeph_skills::matcher::{SkillMatcher, SkillMatcherBackend};
use zeph_skills::registry::SkillRegistry;

impl<C: Channel> Agent<C> {
    /// Update trust DB records for all reloaded skills.
    async fn update_trust_for_reloaded_skills(
        &mut self,
        all_meta: &[zeph_skills::loader::SkillMeta],
    ) {
        // Clone Arc before any .await so no &self fields are held across suspension points.
        let memory = self.services.memory.persistence.memory.clone();
        let Some(memory) = memory else {
            return;
        };
        let trust_cfg = self.services.skill.trust_config.clone();
        let managed_dir = self.services.skill.managed_dir.clone();
        let bundled_names: std::collections::HashSet<String> =
            zeph_skills::bundled_skill_names().into_iter().collect();
        for meta in all_meta {
            // Compute hash and classify source_kind in spawn_blocking — both are blocking FS calls
            // (.bundled marker .exists() and compute_skill_hash both do std::fs I/O).
            let skill_dir = meta.skill_dir.clone();
            let managed_dir_ref = managed_dir.clone();
            let bundled_names_ref = bundled_names.clone();
            let fs_result: Option<(String, zeph_memory::store::SourceKind)> =
                tokio::task::spawn_blocking(move || {
                    let hash = zeph_skills::compute_skill_hash(&skill_dir).ok()?;
                    let source_kind = Self::classify_source_kind(
                        &skill_dir,
                        managed_dir_ref.as_ref(),
                        &bundled_names_ref,
                    );
                    Some((hash, source_kind))
                })
                .await
                .unwrap_or(None);

            let Some((current_hash, source_kind)) = fs_result else {
                tracing::warn!("failed to compute hash for '{}'", meta.name);
                continue;
            };
            let initial_level = match source_kind {
                zeph_memory::store::SourceKind::Bundled => &trust_cfg.bundled_level,
                zeph_memory::store::SourceKind::Local | zeph_memory::store::SourceKind::File => {
                    &trust_cfg.local_level
                }
                _ => &trust_cfg.default_level,
            };
            let existing = memory
                .sqlite()
                .load_skill_trust(&meta.name)
                .await
                .ok()
                .flatten();
            let trust_level = if let Some(ref row) = existing {
                if row.blake3_hash != current_hash {
                    trust_cfg.hash_mismatch_level
                } else if row.source_kind != source_kind {
                    // source_kind changed (e.g., hub → bundled on upgrade).
                    // Never override an explicit operator block. For active trust levels,
                    // adopt the source-kind initial level when it grants more trust.
                    let stored = row.trust_level;
                    if !stored.is_active() || stored.severity() <= initial_level.severity() {
                        stored
                    } else {
                        *initial_level
                    }
                } else {
                    row.trust_level
                }
            } else {
                *initial_level
            };
            let source_path = meta.skill_dir.to_str();
            if let Err(e) = memory
                .sqlite()
                .upsert_skill_trust(
                    &meta.name,
                    trust_level,
                    source_kind,
                    None,
                    source_path,
                    &current_hash,
                )
                .await
            {
                tracing::warn!("failed to record trust for '{}': {e:#}", meta.name);
            }
        }
    }
    /// Rebuild or sync the in-memory skill matcher and BM25 index after a registry update.
    async fn rebuild_skill_matcher(&mut self, all_meta: &[&zeph_skills::loader::SkillMeta]) {
        let provider = self.embedding_provider.clone();
        let embed_timeout =
            std::time::Duration::from_secs(self.runtime.config.timeouts.embedding_seconds);
        let embed_fn = move |text: &str| -> zeph_skills::matcher::EmbedFuture {
            let owned = text.to_owned();
            let p = provider.clone();
            Box::pin(async move {
                if let Ok(result) = tokio::time::timeout(embed_timeout, p.embed(&owned)).await {
                    result
                } else {
                    tracing::warn!(
                        timeout_secs = embed_timeout.as_secs(),
                        "skill matcher: embedding timed out"
                    );
                    Err(zeph_llm::LlmError::Timeout)
                }
            })
        };

        let needs_inmemory_rebuild = !self
            .services
            .skill
            .matcher
            .as_ref()
            .is_some_and(SkillMatcherBackend::is_qdrant);

        if needs_inmemory_rebuild {
            self.services.skill.matcher = SkillMatcher::new(all_meta, embed_fn)
                .await
                .map(SkillMatcherBackend::InMemory);
        } else if let Some(ref mut backend) = self.services.skill.matcher {
            self.channel
                .send_status_best_effort("syncing skill index...")
                .await;
            let on_progress: Option<Box<dyn Fn(usize, usize) + Send>> =
                self.services.session.status_tx.clone().map(
                    |tx| -> Box<dyn Fn(usize, usize) + Send> {
                        Box::new(move |completed, total| {
                            let msg = format!("Syncing skills: {completed}/{total}");
                            let _ = tx.send(msg);
                        })
                    },
                );
            if let Err(e) = backend
                .sync(
                    all_meta,
                    &self.services.skill.embedding_model,
                    embed_fn,
                    on_progress,
                )
                .await
            {
                tracing::warn!("failed to sync skill embeddings: {e:#}");
            }
        }

        if self.services.skill.hybrid_search {
            let descs: Vec<&str> = all_meta.iter().map(|m| m.description.as_str()).collect();
            self.channel
                .send_status_best_effort("rebuilding search index...")
                .await;
            self.services.skill.rebuild_bm25(&descs);
        }
    }
    #[tracing::instrument(name = "core.agent.reload_skills", skip_all, level = "debug")]
    pub(super) async fn reload_skills(&mut self) {
        let old_fp = self.services.skill.fingerprint();
        let reload_paths = if let Some(ref supplier) = self.services.skill.plugin_dirs_supplier {
            let plugin_dirs = supplier();
            let mut paths = self.services.skill.skill_paths.clone();
            for dir in plugin_dirs {
                if !paths.contains(&dir) {
                    paths.push(dir);
                }
            }
            paths
        } else {
            self.services.skill.skill_paths.clone()
        };
        // Build the reloaded registry off the shared lock entirely (WalkDir + SKILL.md
        // parsing for every skill is blocking fs I/O), then swap it in with only a brief
        // write-lock hold. Wrapping the existing `.write().reload(...)` call in
        // spawn_blocking as-is would move the I/O to a worker thread but still hold the
        // shared write lock for the full reload duration, stalling any concurrent
        // `.read()` (e.g. a concurrent `rebuild_system_prompt`) for the same span.
        let hub_dirs: Vec<std::path::PathBuf> =
            self.services.skill.registry.read().hub_dirs().to_vec();
        let span = tracing::info_span!("skills.registry.reload_blocking");
        match tokio::task::spawn_blocking(move || {
            let _enter = span.enter();
            SkillRegistry::load(&reload_paths).with_hub_dirs(hub_dirs)
        })
        .await
        {
            Ok(new_registry) => {
                *self.services.skill.registry.write() = new_registry;
            }
            Err(e) => {
                tracing::error!(
                    "reload_skills: spawn_blocking panicked, skill registry left unchanged: {e}"
                );
                return;
            }
        }
        if self.services.skill.fingerprint() == old_fp {
            return;
        }
        self.channel
            .send_status_best_effort("reloading skills...")
            .await;

        let all_meta = self
            .services
            .skill
            .registry
            .read()
            .all_meta()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();

        self.update_trust_for_reloaded_skills(&all_meta).await;

        let all_meta_refs = all_meta.iter().collect::<Vec<_>>();
        self.rebuild_skill_matcher(&all_meta_refs).await;

        // `reg.skill()` loads each skill's body (and resources) from disk on first access —
        // synchronous fs I/O that must not run inline on the agent's async task.
        let registry_arc = Arc::clone(&self.services.skill.registry);
        let span = tracing::info_span!("skills.registry.load_bodies_blocking");
        let all_skills: Vec<Skill> = tokio::task::spawn_blocking(move || {
            let _enter = span.enter();
            let reg = registry_arc.read();
            reg.all_meta()
                .iter()
                .filter_map(|m| reg.skill(&m.name).ok())
                .collect()
        })
        .await
        .unwrap_or_else(|e| {
            tracing::error!("reload_skills: spawn_blocking for skill bodies panicked: {e}");
            Vec::new()
        });
        let trust_map = self.build_skill_trust_map().await;
        let empty_health: HashMap<String, (f64, u32)> = HashMap::new();
        let skills_prompt =
            state::SkillState::rebuild_prompt(&all_skills, &trust_map, &empty_health);
        self.services
            .skill
            .last_skills_prompt
            .clone_from(&skills_prompt);
        let system_prompt = build_system_prompt(&skills_prompt, None);
        if let Some(msg) = self.msg.messages.first_mut() {
            msg.content = system_prompt;
        }

        self.channel.send_status_best_effort("").await;
        tracing::info!(
            "reloaded {} skill(s)",
            self.services.skill.registry.read().all_meta().len()
        );
    }
    pub(super) async fn reload_instructions(&mut self) {
        // Drain any additional queued events before reloading to avoid redundant reloads.
        if let Some(ref mut rx) = self.runtime.instructions.reload_rx {
            while rx.try_recv().is_ok() {}
        }
        let Some(ref state) = self.runtime.instructions.reload_state else {
            return;
        };
        let base_dir = state.base_dir.clone();
        let provider_kinds = state.provider_kinds.clone();
        let explicit_files = state.explicit_files.clone();
        let auto_detect = state.auto_detect;
        let new_blocks = crate::instructions::load_instructions_async(
            base_dir,
            provider_kinds,
            explicit_files,
            auto_detect,
        )
        .await;
        let old_sources: std::collections::HashSet<_> = self
            .runtime
            .instructions
            .blocks
            .iter()
            .map(|b| &b.source)
            .collect();
        let new_sources: std::collections::HashSet<_> =
            new_blocks.iter().map(|b| &b.source).collect();
        for added in new_sources.difference(&old_sources) {
            tracing::info!(path = %added.display(), "instruction file added");
        }
        for removed in old_sources.difference(&new_sources) {
            tracing::info!(path = %removed.display(), "instruction file removed");
        }
        tracing::info!(
            old_count = self.runtime.instructions.blocks.len(),
            new_count = new_blocks.len(),
            "reloaded instruction files"
        );
        self.runtime.instructions.blocks = new_blocks;
    }
}
