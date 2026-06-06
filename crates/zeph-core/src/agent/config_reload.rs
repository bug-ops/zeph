// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Hot-reload of the runtime configuration overlay.
//!
//! Extracted from `agent/mod.rs` (#4923). Re-reads the config file, applies the
//! shell overlay, recomputes the context budget, and warns when the reloaded shell
//! configuration diverges from the active one.

use super::{Agent, resolve_context_budget};
use crate::channel::Channel;
use crate::config::Config;
use crate::context::ContextBudget;

impl<C: Channel> Agent<C> {
    #[allow(clippy::too_many_lines)]
    pub(super) fn reload_config(&mut self) {
        let Some(path) = self.runtime.lifecycle.config_path.clone() else {
            return;
        };
        let Some(config) = self.load_config_with_overlay(&path) else {
            return;
        };
        let budget_tokens = resolve_context_budget(&config, &self.provider);
        self.runtime.config.security = config.security;
        self.runtime.config.timeouts = config.timeouts;
        self.runtime.config.redact_credentials = config.memory.redact_credentials;
        self.services.memory.persistence.history_limit = config.memory.history_limit;
        self.services.memory.persistence.recall_limit = config.memory.semantic.recall_limit;
        self.services.memory.compaction.summarization_threshold =
            config.memory.summarization_threshold;
        self.services.skill.max_active_skills = config.skills.max_active_skills.get();
        self.services.skill.disambiguation_threshold = config.skills.disambiguation_threshold;
        self.services.skill.min_injection_score = config.skills.min_injection_score;
        self.services.skill.cosine_weight = config.skills.cosine_weight.clamp(0.0, 1.0);
        self.services.skill.hybrid_search = config.skills.hybrid_search;
        {
            let alpha = config.skills.bm25_alpha;
            if !(0.0..=1.0).contains(&alpha) {
                tracing::warn!(
                    bm25_alpha = alpha,
                    "bm25_alpha is outside [0.0, 1.0]; clamping to valid range"
                );
            }
            self.services.skill.bm25_alpha = alpha.clamp(0.0, 1.0);
        }
        self.services.skill.two_stage_matching = config.skills.two_stage_matching;
        self.services.skill.confusability_threshold =
            config.skills.confusability_threshold.clamp(0.0, 1.0);
        self.services.skill.group_structured = config.skills.group_structured;
        self.services.skill.support_similarity_threshold =
            config.skills.support_similarity_threshold;
        config
            .skills
            .query_rewrite_provider
            .as_str()
            .clone_into(&mut self.services.skill.query_rewrite_provider_name);
        config
            .skills
            .generation_provider
            .as_str()
            .clone_into(&mut self.services.skill.generation_provider_name);
        config
            .skills
            .disambiguate_provider
            .as_str()
            .clone_into(&mut self.services.skill.disambiguate_provider_name);
        self.services.skill.generation_timeout_ms = config.skills.generation_timeout_ms;
        self.services.skill.semantic_scan = config.skills.semantic_scan;
        config
            .skills
            .semantic_scan_provider
            .as_str()
            .clone_into(&mut self.services.skill.semantic_scan_provider);
        self.services.skill.generation_output_dir =
            config.skills.generation_output_dir.as_deref().map(|p| {
                if let Some(stripped) = p.strip_prefix("~/") {
                    dirs::home_dir()
                        .map_or_else(|| std::path::PathBuf::from(p), |h| h.join(stripped))
                } else {
                    std::path::PathBuf::from(p)
                }
            });

        self.context_manager.budget = Some(
            ContextBudget::new(budget_tokens, 0.20).with_graph_enabled(config.memory.graph.enabled),
        );

        {
            let graph_cfg = &config.memory.graph;
            if graph_cfg.rpe.enabled {
                // Re-create router only if it doesn't exist yet; preserve state on hot-reload.
                if self.services.memory.extraction.rpe_router.is_none() {
                    self.services.memory.extraction.rpe_router =
                        Some(std::sync::Mutex::new(zeph_memory::RpeRouter::new(
                            graph_cfg.rpe.threshold,
                            graph_cfg.rpe.max_skip_turns,
                        )));
                }
            } else {
                self.services.memory.extraction.rpe_router = None;
            }
            self.services.memory.extraction.graph_config = graph_cfg.clone();
        }
        self.context_manager.soft_compaction_threshold = config.memory.soft_compaction_threshold;
        self.context_manager.hard_compaction_threshold = config.memory.hard_compaction_threshold;
        self.context_manager.compaction_preserve_tail = config.memory.compaction_preserve_tail;
        self.context_manager
            .set_compaction_cooldown_turns(config.memory.compaction_cooldown_turns);
        self.context_manager.prune_protect_tokens = config.memory.prune_protect_tokens;
        self.context_manager.compression = config.memory.compression.clone();
        self.context_manager.routing = config.memory.store_routing.clone();
        // Resolve routing_classifier_provider from the provider pool (#2484).
        self.context_manager.store_routing_provider = if config
            .memory
            .store_routing
            .routing_classifier_provider
            .is_empty()
        {
            None
        } else {
            let resolved = self.resolve_background_provider(
                config
                    .memory
                    .store_routing
                    .routing_classifier_provider
                    .as_str(),
            );
            Some(std::sync::Arc::new(resolved))
        };
        self.services
            .memory
            .persistence
            .cross_session_score_threshold = config.memory.cross_session_score_threshold;

        self.services.index.repo_map_tokens = config.index.repo_map_tokens;
        self.services.index.repo_map_ttl =
            std::time::Duration::from_secs(config.index.repo_map_ttl_secs);

        self.services
            .session
            .hooks_config
            .cwd_changed
            .clone_from(&config.hooks.cwd_changed);
        self.services
            .session
            .hooks_config
            .permission_denied
            .clone_from(&config.hooks.permission_denied);
        self.services
            .session
            .hooks_config
            .turn_complete
            .clone_from(&config.hooks.turn_complete);
        // file_changed_hooks require watcher restart to take effect — skipped here.

        tracing::info!("config reloaded");
    }
    /// Load config from disk, apply plugin overlays, and warn on shell divergence.
    ///
    /// Returns `None` when loading or overlay merge fails (caller keeps prior runtime state).
    fn load_config_with_overlay(&mut self, path: &std::path::Path) -> Option<Config> {
        let mut config = match Config::load(path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("config reload failed: {e:#}");
                return None;
            }
        };

        // Re-apply plugin overlays. On error, keep previous runtime state intact.
        let new_overlay = if self.runtime.lifecycle.plugins_dir.as_os_str().is_empty() {
            None
        } else {
            match zeph_plugins::apply_plugin_config_overlays(
                &mut config,
                &self.runtime.lifecycle.plugins_dir,
            ) {
                Ok(o) => Some(o),
                Err(e) => {
                    tracing::warn!(
                        "plugin overlay merge failed during reload: {e:#}; \
                         keeping previous runtime state"
                    );
                    return None;
                }
            }
        };

        // M4: detect shell-level divergence from the baked-in executor and warn loudly.
        // ShellExecutor is not rebuilt on hot-reload; only skill threshold is live.
        // A follow-up P2 issue tracks live-rebuild of ShellExecutor.
        if let Some(ref overlay) = new_overlay {
            self.warn_on_shell_overlay_divergence(overlay, &config);
        }
        Some(config)
    }
    /// React to shell policy divergence detected on hot-reload.
    ///
    /// `blocked_commands` is rebuilt live via `ShellPolicyHandle::rebuild` — no restart needed.
    /// `allowed_commands` cannot be rebuilt (feeds sandbox path intersection at construction time)
    /// — emit a warn + status banner when it changes.
    pub(super) fn warn_on_shell_overlay_divergence(
        &self,
        new_overlay: &zeph_plugins::ResolvedOverlay,
        config: &Config,
    ) {
        let new_blocked: Vec<String> = {
            let mut v = config.tools.shell.blocked_commands.clone();
            v.sort();
            v
        };
        let new_allowed: Vec<String> = {
            let mut v = config.tools.shell.allowed_commands.clone();
            v.sort();
            v
        };

        let startup = &self.runtime.lifecycle.startup_shell_overlay;
        let blocked_changed = new_blocked != startup.blocked;
        let allowed_changed = new_allowed != startup.allowed;

        // blocked_commands IS rebuilt live — emit info-level confirmation only.
        if blocked_changed && let Some(ref h) = self.runtime.lifecycle.shell_policy_handle {
            h.rebuild(&config.tools.shell);
            tracing::info!(
                blocked_count = h.snapshot_blocked().len(),
                "shell blocked_commands rebuilt from hot-reload"
            );
        }

        // allowed_commands cannot be rebuilt — sandbox path intersection is computed at
        // executor construction time. Warn loudly so the user restarts.
        //
        // Note: when base `allowed_commands` is empty (the default), the overlay's
        // intersection semantics keep it empty, so this branch is silently unreachable
        // for users who do not set a non-empty base list.
        if allowed_changed {
            let msg = "plugin config overlay changed shell allowed_commands; RESTART REQUIRED \
                 for sandbox path recomputation (blocked_commands was rebuilt live)";
            tracing::warn!("{msg}");
            if let Some(ref tx) = self.services.session.status_tx {
                let _ = tx.send(msg.to_owned());
            }
        }

        let _ = new_overlay;
    }
}
