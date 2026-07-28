// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Hot-reload of skills and instructions.
//!
//! Extracted from `agent/mod.rs` (#4923). Rebuilds the skill matcher, refreshes
//! per-skill trust scores, and reloads `instructions`/`AGENTS.md` overlays when the
//! filesystem watcher signals a change.

use std::collections::HashSet;

use super::Agent;
use crate::channel::Channel;
use crate::context::build_system_prompt;
use zeph_llm::provider::LlmProvider;
use zeph_skills::loader::Skill;
use zeph_skills::matcher::{SkillMatcher, SkillMatcherBackend};
use zeph_skills::registry::SkillRegistry;
use zeph_tools::registry::ToolDef;

impl<C: Channel> Agent<C> {
    /// Builds the current skill catalog (name + description) for
    /// [`Channel::send_skill_catalog`], excluding blocked skills.
    ///
    /// Shared by the startup emit (`Agent::run`) and the hot-reload emit below so both
    /// apply the same [`zeph_common::SkillTrustLevel::Blocked`] filter — reading
    /// `registry.read().all_meta()` raw at only one of the two sites would let blocked
    /// skills appear in the mention picker until the first hot-reload, then silently
    /// vanish (M2). Also runs [`Self::warn_on_tool_id_collisions`] here for the same reason:
    /// it is the one call site that fires on both startup and every hot-reload, independent
    /// of trust-DB/`memory` availability (#6702 S4).
    pub(super) async fn skill_catalog_items(&mut self) -> Vec<crate::channel::SkillCatalogItem> {
        let all_meta: Vec<zeph_skills::loader::SkillMeta> = self
            .services
            .skill
            .registry
            .read()
            .all_meta()
            .into_iter()
            .cloned()
            .collect();
        self.warn_on_tool_id_collisions(&all_meta);
        let trust_map = match self.build_skill_trust_map().await {
            crate::agent::trust_commands::SkillTrustMapLoad::Fresh(map) => map,
            crate::agent::trust_commands::SkillTrustMapLoad::LoadFailed => {
                self.services.skill.trust_snapshot.read().clone()
            }
        };
        all_meta
            .into_iter()
            .filter(|m| {
                !matches!(
                    trust_map.get(&m.name),
                    Some(snap) if snap.trust_level == zeph_common::SkillTrustLevel::Blocked
                )
            })
            .map(|m| crate::channel::SkillCatalogItem {
                name: m.name,
                description: m.description,
            })
            .collect()
    }

    /// WARN when a skill name collides with a native (non-MCP) tool ID after hyphen/underscore
    /// normalization (#6702). `AutoSkill` names can never contain `_` (rejected by both
    /// `validate_generated_name` and `validate_skill_name`), so only `-` -> `_` normalization
    /// is needed.
    ///
    /// Deliberately independent of `memory`/trust-DB availability, and called from
    /// [`Self::skill_catalog_items`] rather than [`Self::update_trust_for_reloaded_skills`] —
    /// the latter early-returns when `memory` is `None` and is only invoked from the
    /// hot-reload path, so a collision present at startup (before any file changes) never
    /// warned (#6702 S4).
    fn warn_on_tool_id_collisions(&self, all_meta: &[zeph_skills::loader::SkillMeta]) {
        let native_tool_ids: HashSet<String> = self
            .tool_executor
            .tool_definitions_erased()
            .into_iter()
            .filter(|t| !ToolDef::is_mcp_tool(t))
            .map(|t| t.id.to_string())
            .collect();
        for meta in all_meta {
            let normalized = meta.name.replace('-', "_");
            if native_tool_ids.contains(&normalized) {
                tracing::warn!(
                    skill = %meta.name,
                    tool_id = %normalized,
                    "skill name collides with a native tool ID after hyphen/underscore normalization"
                );
            }
        }
    }

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
        // #6031: single DRY choke point for the skill-hot-reload gate — covers every entry
        // point (runner/daemon/acp/serve) at once, instead of patching each `SkillWatcher`
        // call site individually. Without this, a session that correctly started with an
        // empty registry (daemon/acp/serve's `build_shared_core` gate) would still silently
        // re-populate it from disk on the first skill-file change, defeating safe-mode for
        // the rest of the session.
        if self.runtime.config.safe_mode {
            tracing::debug!("safe mode active: skipping skill hot-reload");
            return;
        }
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

        // Catalog-only listing (name + description) — full skill bodies are injected
        // exclusively by the per-turn `rebuild_system_prompt(query)` matcher, so a reload
        // must not force-load every skill's body from disk (#6413). Building `Skill` stubs
        // straight from metadata (already loaded in `all_meta` above) needs no further I/O.
        // Blocked skills are excluded, mirroring `apply_skill_trust_and_gating`'s per-turn
        // catalog filter.
        let trust_map = match self.build_skill_trust_map().await {
            crate::agent::trust_commands::SkillTrustMapLoad::Fresh(map) => map,
            crate::agent::trust_commands::SkillTrustMapLoad::LoadFailed => {
                // Same fail-closed policy as `apply_skill_trust_and_gating`: a transient
                // read failure must not be treated as "no trust data" (which would drop
                // the Blocked-skill catalog filter below) — reuse the last-known snapshot.
                tracing::warn!(
                    "reload_skills: trust snapshot load failed, reusing previous snapshot \
                     for catalog filtering"
                );
                self.services.skill.trust_snapshot.read().clone()
            }
        };
        let catalog_skills: Vec<Skill> = all_meta
            .iter()
            .filter(|m| {
                !matches!(
                    trust_map.get(&m.name),
                    Some(snap) if snap.trust_level == zeph_common::SkillTrustLevel::Blocked
                )
            })
            .map(|m| Skill {
                meta: m.clone(),
                body: String::new(),
                resources: zeph_skills::resource::SkillResources::default(),
            })
            .collect();
        let skills_prompt = zeph_skills::prompt::format_skills_catalog(&catalog_skills);
        self.services
            .skill
            .last_skills_prompt
            .clone_from(&skills_prompt);
        let system_prompt = build_system_prompt(&skills_prompt, None);
        if let Some(msg) = self.msg.messages.first_mut() {
            msg.content = system_prompt;
        }
        // The mutation above bypasses `push_message`'s incremental token accounting, so the
        // cached prompt-token count must be recomputed explicitly or it goes stale until the
        // next turn's `rebuild_system_prompt` overwrites it (#6413).
        self.recompute_prompt_tokens();

        // Re-emit the catalog so an open TUI mention picker's Skills tab (spec 084 §6)
        // picks up additions/removals/blocked-status changes on hot-reload, not just at
        // startup.
        let catalog_items = self.skill_catalog_items().await;
        if let Err(e) = self.channel.send_skill_catalog(&catalog_items).await {
            tracing::warn!("failed to re-emit skill catalog after reload: {e}");
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::layer::SubscriberExt as _;
    use zeph_common::SkillTrustLevel;
    use zeph_memory::semantic::SemanticMemory;
    use zeph_memory::store::SourceKind;

    use super::super::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use super::*;

    async fn test_memory() -> Arc<SemanticMemory> {
        let provider = zeph_llm::any::AnyProvider::Mock(zeph_llm::mock::MockProvider::default());
        Arc::new(
            SemanticMemory::new(
                ":memory:",
                "http://127.0.0.1:1",
                None,
                provider,
                "test-model",
            )
            .await
            .unwrap(),
        )
    }

    fn agent_with_memory_and_executor(
        memory: Arc<SemanticMemory>,
        executor: MockToolExecutor,
    ) -> Agent<MockChannel> {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            memory,
            zeph_memory::ConversationId(1),
            50,
            5,
            50,
        )
    }

    /// Regression test for #6702: once a `skill_trust` row has been written at
    /// `Quarantined` (simulating the eager write `log_and_persist` now performs right
    /// after an `AutoSkill` draft is generated), a hot-reload pass with
    /// `trust_cfg.default_level = Trusted` must NOT silently promote it. Only the
    /// `_quarantine/` directory naming used to be relied on for this — which the loader
    /// never actually enforced.
    #[tokio::test]
    async fn update_trust_preserves_quarantine_despite_trusted_default() {
        let managed = tempfile::tempdir().unwrap();
        let skill_dir = managed.path().join("_quarantine").join("my-skill");
        tokio::fs::create_dir_all(&skill_dir).await.unwrap();
        tokio::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: A test skill.\nversion: 0\nsource: trace_extraction\n---\n\n## How to use\nDo the thing.\n",
        )
        .await
        .unwrap();
        let hash = zeph_skills::compute_skill_hash(&skill_dir).unwrap();

        let memory = test_memory().await;
        memory
            .sqlite()
            .upsert_skill_trust(
                "my-skill",
                SkillTrustLevel::Quarantined,
                SourceKind::Hub,
                None,
                skill_dir.to_str(),
                &hash,
            )
            .await
            .unwrap();

        let mut agent =
            agent_with_memory_and_executor(memory.clone(), MockToolExecutor::no_tools());
        agent.services.skill.trust_config.default_level = SkillTrustLevel::Trusted;
        agent.services.skill.managed_dir = Some(managed.path().to_path_buf());

        let meta = zeph_skills::loader::SkillMeta {
            name: "my-skill".into(),
            description: "A test skill.".into(),
            version: 0,
            source: "trace_extraction".into(),
            skill_dir: skill_dir.clone(),
            ..Default::default()
        };
        agent.update_trust_for_reloaded_skills(&[meta]).await;

        let row = memory
            .sqlite()
            .load_skill_trust("my-skill")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.trust_level,
            SkillTrustLevel::Quarantined,
            "an existing Quarantined row must survive a reload even when default_level=Trusted"
        );
    }

    struct MessageCaptureLayer {
        messages: Arc<Mutex<Vec<String>>>,
    }
    struct MessageVisitor(String);
    impl tracing::field::Visit for MessageVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0 = format!("{value:?}");
            }
        }
    }
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for MessageCaptureLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut visitor = MessageVisitor(String::new());
            event.record(&mut visitor);
            self.messages.lock().unwrap().push(visitor.0);
        }
    }

    fn colliding_tool_executor() -> MockToolExecutor {
        let tool_def = ToolDef {
            id: "list_directory".into(),
            description: "list directory tool".into(),
            schema: schemars::Schema::default(),
            invocation: zeph_tools::registry::InvocationHint::ToolCall,
            output_schema: None,
            server_id: None,
        };
        MockToolExecutor::no_tools().with_definitions(vec![tool_def])
    }

    fn colliding_skill_meta() -> zeph_skills::loader::SkillMeta {
        zeph_skills::loader::SkillMeta {
            name: "list-directory".into(),
            description: "A colliding skill.".into(),
            version: 0,
            source: "trace_extraction".into(),
            skill_dir: std::path::PathBuf::from("/nonexistent/list-directory"),
            ..Default::default()
        }
    }

    fn capture_warn_messages() -> (Arc<Mutex<Vec<String>>>, tracing::subscriber::DefaultGuard) {
        let messages: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let layer = MessageCaptureLayer {
            messages: messages.clone(),
        };
        let subscriber = tracing_subscriber::registry().with(layer);
        let guard = tracing::subscriber::set_default(subscriber);
        (messages, guard)
    }

    fn assert_collision_warn_fired(captured: &[String]) {
        assert!(
            captured.iter().any(|m| m.contains(
                "skill name collides with a native tool ID after hyphen/underscore normalization"
            )),
            "expected a collision WARN, got: {captured:?}"
        );
    }

    /// Regression test for #6702 direction 1: a skill whose name collides with a native
    /// tool ID after hyphen/underscore normalization must WARN, with `memory` present.
    #[tokio::test]
    async fn update_trust_warns_on_native_tool_id_collision() {
        let memory = test_memory().await;
        let agent = agent_with_memory_and_executor(memory, colliding_tool_executor());

        let (messages, _guard) = capture_warn_messages();
        agent.warn_on_tool_id_collisions(&[colliding_skill_meta()]);
        assert_collision_warn_fired(&messages.lock().unwrap());
    }

    /// Regression test for #6702 S4(a): the collision WARN must fire even when `memory` is
    /// `None` (no `SemanticMemory` configured) — the check must not be gated behind trust-DB
    /// availability, since `update_trust_for_reloaded_skills` early-returns without memory but
    /// the collision itself has nothing to do with the trust DB.
    #[tokio::test]
    async fn warn_on_tool_id_collisions_fires_without_memory() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let agent = Agent::new(
            provider,
            channel,
            registry,
            None,
            5,
            colliding_tool_executor(),
        );

        let (messages, _guard) = capture_warn_messages();
        agent.warn_on_tool_id_collisions(&[colliding_skill_meta()]);
        assert_collision_warn_fired(&messages.lock().unwrap());
    }

    /// Regression test for #6702 S4(b): the collision check must run from
    /// `skill_catalog_items()` — the call site shared by the startup emit (`Agent::run`) and
    /// every hot-reload — so a colliding skill present at startup (steady state, before any
    /// file changes) is caught too, not only after the first hot-reload's fingerprint change.
    #[tokio::test]
    async fn skill_catalog_items_warns_on_native_tool_id_collision() {
        let temp_dir = tempfile::tempdir().unwrap();
        let skill_dir = temp_dir.path().join("list-directory");
        std::fs::create_dir(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: list-directory\ndescription: A colliding skill.\n---\nBody",
        )
        .unwrap();
        let registry = SkillRegistry::load(&[temp_dir.path().to_path_buf()]);

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let mut agent = Agent::new(
            provider,
            channel,
            registry,
            None,
            5,
            colliding_tool_executor(),
        );

        let (messages, _guard) = capture_warn_messages();
        let _ = agent.skill_catalog_items().await;
        assert_collision_warn_fired(&messages.lock().unwrap());
    }
}
