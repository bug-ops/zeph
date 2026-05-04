// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;
use std::sync::Arc;

use zeph_agent_context::helpers::BudgetHint;
use zeph_llm::provider::LlmProvider;
use zeph_skills::ScoredMatch;
use zeph_skills::loader::SkillMeta;
use zeph_skills::prompt::{format_skills_catalog, format_skills_prompt_compact};

use super::super::{Agent, Skill, format_skills_prompt};
use crate::channel::Channel;
use crate::context::build_system_prompt_with_instructions;

// ── Security event sink adapter ───────────────────────────────────────────────
//
// Wraps the metrics watch-channel sender so `ContextService::prepare_context`
// can publish security events without depending on `zeph-core`-internal types.
// Defined at module scope to satisfy clippy::items_after_statements.
struct SecuritySink<'a>(
    &'a mut Option<tokio::sync::watch::Sender<crate::metrics::MetricsSnapshot>>,
);

impl zeph_agent_context::state::SecurityEventSink for SecuritySink<'_> {
    fn push(
        &mut self,
        category: zeph_common::SecurityEventCategory,
        source: &'static str,
        detail: String,
    ) {
        if let Some(tx) = &self.0 {
            let event = crate::metrics::SecurityEvent::new(category, source, detail);
            tx.send_modify(|m| {
                if m.security_events.len() >= crate::metrics::SECURITY_EVENT_CAP {
                    m.security_events.pop_front();
                }
                m.security_events.push_back(event);
            });
        }
    }
}

impl<C: Channel> Agent<C> {
    /// Construct a `ProviderHandles` bundle from the agent's primary and embedding providers.
    pub(in crate::agent) fn providers(&self) -> zeph_agent_context::state::ProviderHandles {
        zeph_agent_context::state::ProviderHandles {
            primary: self.provider.clone(),
            embedding: self.embedding_provider.clone(),
        }
    }

    /// Construct a `MessageWindowView` from disjoint `Agent<C>` sub-fields.
    ///
    /// All `&mut` borrows resolve to distinct top-level fields (`msg`, `runtime.providers`,
    /// `runtime.metrics`, `services.tool_state`), so the borrow checker accepts the literal.
    fn message_window_view(&mut self) -> zeph_agent_context::state::MessageWindowView<'_> {
        zeph_agent_context::state::MessageWindowView {
            messages: &mut self.msg.messages,
            last_persisted_message_id: &mut self.msg.last_persisted_message_id,
            deferred_db_hide_ids: &mut self.msg.deferred_db_hide_ids,
            deferred_db_summaries: &mut self.msg.deferred_db_summaries,
            cached_prompt_tokens: &mut self.runtime.providers.cached_prompt_tokens,
            token_counter: Arc::clone(&self.runtime.metrics.token_counter),
            completed_tool_ids: &mut self.services.tool_state.completed_tool_ids,
        }
    }

    /// Construct a [`ContextSummarizationView`] borrow-lens from `Agent<C>` fields.
    ///
    /// All `&mut` borrows resolve to distinct top-level sub-fields, so the borrow checker
    /// accepts the literal. The view is used by [`ContextService`] summarization methods
    /// (deferred summaries, compaction, goal/subgoal scheduling) so they can access only
    /// the state they need without taking `&mut self` on `Agent<C>`.
    ///
    /// Call sites are added as each summarization method is migrated in subsequent PRs
    /// (PR4 deferred summaries, PR7 proactive compression, PR8 compaction).
    ///
    /// [`ContextSummarizationView`]: zeph_agent_context::state::ContextSummarizationView
    /// [`ContextService`]: zeph_agent_context::ContextService
    pub(in crate::agent) fn summarization_view(
        &mut self,
    ) -> zeph_agent_context::state::ContextSummarizationView<'_> {
        let summarization_deps = self.build_summarization_deps();
        let redact = self.runtime.config.redact_credentials;

        zeph_agent_context::state::ContextSummarizationView {
            messages: &mut self.msg.messages,
            deferred_db_hide_ids: &mut self.msg.deferred_db_hide_ids,
            deferred_db_summaries: &mut self.msg.deferred_db_summaries,
            cached_prompt_tokens: &mut self.runtime.providers.cached_prompt_tokens,
            context_manager: &mut self.context_manager,
            server_compaction_active: self.runtime.providers.server_compaction_active,
            token_counter: Arc::clone(&self.runtime.metrics.token_counter),
            summarization_deps,
            task_supervisor: Arc::clone(&self.runtime.lifecycle.task_supervisor),
            memory: self.services.memory.persistence.memory.clone(),
            conversation_id: self.services.memory.persistence.conversation_id,
            tool_call_cutoff: self.services.memory.persistence.tool_call_cutoff,
            subgoal_registry: &mut self.services.compression.subgoal_registry,
            pending_task_goal: &mut self.services.compression.pending_task_goal,
            pending_subgoal: &mut self.services.compression.pending_subgoal,
            current_task_goal: &mut self.services.compression.current_task_goal,
            task_goal_user_msg_hash: &mut self.services.compression.task_goal_user_msg_hash,
            subgoal_user_msg_hash: &mut self.services.compression.subgoal_user_msg_hash,
            status_tx: self.services.session.status_tx.clone(),
            scrub: if redact {
                crate::redact::scrub_content
            } else {
                |s| std::borrow::Cow::Borrowed(s)
            },
            // Compaction callbacks — populated by the shim before calling compact_context.
            compression_guidelines: None,
            probe: None,
            archive: None,
            persistence: None,
            metrics: None,
        }
    }

    pub(in crate::agent) fn clear_history(&mut self) {
        let svc = zeph_agent_context::ContextService::new();
        svc.clear_history(&mut self.message_window_view());
    }

    /// Remove previously injected LSP context notes from the message history.
    ///
    /// Called before injecting fresh notes each turn so stale diagnostics/hover
    /// data from the previous tool call do not accumulate across iterations.
    pub(in crate::agent) fn remove_lsp_messages(&mut self) {
        let svc = zeph_agent_context::ContextService::new();
        svc.remove_lsp_messages(&mut self.message_window_view());
    }

    pub(in crate::agent) fn remove_code_context_messages(&mut self) {
        let svc = zeph_agent_context::ContextService::new();
        svc.remove_code_context_messages(&mut self.message_window_view());
    }

    /// Spawn a fire-and-forget background task to generate and persist a session digest for
    /// `conversation_id`. No-op when digest is disabled or the conversation has no messages.
    fn spawn_outgoing_digest(&self, conversation_id: Option<zeph_memory::ConversationId>) {
        if !self.services.memory.compaction.digest_config.enabled {
            return;
        }
        let non_system: Vec<_> = self
            .msg
            .messages
            .iter()
            .skip(1)
            .filter(|m| m.role != zeph_llm::provider::Role::System)
            .cloned()
            .collect();
        if non_system.is_empty() {
            return;
        }
        let digest_config = self.services.memory.compaction.digest_config.clone();
        let memory = self.services.memory.persistence.memory.clone();
        let provider = self.provider.clone();
        let tc = self.runtime.metrics.token_counter.clone();
        let sanitizer = self.services.security.sanitizer.clone();
        let status_tx = self.services.session.status_tx.clone();
        let task_supervisor = Arc::clone(&self.runtime.lifecycle.task_supervisor);
        if let Some(tx) = &self.services.session.status_tx {
            let _ = tx.send("Generating session digest...".to_string());
        }
        let digest_future = async move {
            if let (Some(mem), Some(cid)) = (memory, conversation_id) {
                super::super::session_digest::generate_and_store_digest(
                    &provider,
                    &mem,
                    cid,
                    &non_system,
                    &digest_config,
                    &tc,
                    &sanitizer,
                )
                .await;
            }
            if let Some(tx) = status_tx {
                let _ = tx.send(String::new());
            }
        };
        let cell = std::sync::Arc::new(std::sync::Mutex::new(Some(digest_future)));
        task_supervisor.spawn(zeph_common::task_supervisor::TaskDescriptor {
            name: "agent.session.digest",
            restart: zeph_common::task_supervisor::RestartPolicy::RunOnce,
            factory: move || {
                let f = cell.lock().ok().and_then(|mut g| g.take());
                async move {
                    if let Some(f) = f {
                        f.await;
                    }
                }
            },
        });
    }

    /// Reset the conversation window for `/new`.
    ///
    /// Creates a new `ConversationId` in `SQLite` first (fail-fast: no state is mutated
    /// if the `DB` call fails). Then resets all session-scoped state while preserving
    /// cross-session state (memory, MCP, providers, skills).
    ///
    /// `keep_plan` — when `true`, `orchestration.pending_graph` is preserved.
    /// `no_digest` — when `true`, skip generating a session digest for the outgoing
    ///               conversation. Default behaviour: generate digest fire-and-forget.
    ///
    /// Returns the old and new `ConversationId` for the confirmation message.
    ///
    /// # Errors
    ///
    /// Returns an error if [`create_conversation`](zeph_memory::store::SqliteStore::create_conversation)
    /// fails. In that case no agent state is modified.
    pub(in crate::agent) async fn reset_conversation(
        &mut self,
        keep_plan: bool,
        no_digest: bool,
    ) -> Result<
        (
            Option<zeph_memory::ConversationId>,
            Option<zeph_memory::ConversationId>,
        ),
        super::super::error::AgentError,
    > {
        // --- Step 1: create new ConversationId FIRST (fail-fast) ---
        // Clone the Arc before .await so &mut self is not held across the await boundary.
        let memory_arc = self.services.memory.persistence.memory.clone();
        let new_conversation_id = if let Some(memory) = memory_arc {
            match memory.sqlite().create_conversation().await {
                Ok(id) => Some(id),
                Err(e) => return Err(super::super::error::AgentError::Memory(e)),
            }
        } else {
            None
        };

        let old_conversation_id = self.services.memory.persistence.conversation_id;

        // --- Step 2: fire-and-forget digest for outgoing conversation ---
        if !no_digest {
            self.spawn_outgoing_digest(old_conversation_id);
        }

        // --- Step 3: TUI status ---
        if let Some(ref tx) = self.services.session.status_tx {
            let _ = tx.send("Resetting conversation...".to_string());
        }

        // --- Step 4: abort background compression tasks (context-compression) ---
        {
            if let Some(h) = self.services.compression.pending_task_goal.take() {
                h.abort();
            }
            if let Some(h) = self.services.compression.pending_sidequest_result.take() {
                h.abort();
            }
            if let Some(h) = self.services.compression.pending_subgoal.take() {
                h.abort();
            }
            self.services.compression.current_task_goal = None;
            self.services.compression.task_goal_user_msg_hash = None;
            self.services.compression.subgoal_registry =
                zeph_agent_context::SubgoalRegistry::default();
            self.services.compression.subgoal_user_msg_hash = None;
        }

        // --- Step 5: cancel running plan and clear orchestration ---
        if !keep_plan {
            if let Some(token) = self.services.orchestration.plan_cancel_token.take() {
                token.cancel();
            }
            self.services.orchestration.pending_graph = None;
            self.services.orchestration.pending_goal_embedding = None;
        }
        // Cancel running sub-agents regardless of keep_plan.
        if let Some(ref mut mgr) = self.services.orchestration.subagent_manager {
            mgr.shutdown_all();
        }

        // --- Step 6: reset message history and caches ---
        self.clear_history();
        self.tool_orchestrator.clear_cache();

        // Drain message queue, logging discarded entries.
        let discarded = self.clear_queue();
        if discarded > 0 {
            tracing::debug!(
                discarded,
                "/new: discarded queued messages that arrived during reset"
            );
        }
        self.msg.pending_image_parts.clear();

        // --- Step 7: reset security URL sets ---
        self.services.security.user_provided_urls.write().clear();
        self.services.security.flagged_urls.clear();

        // --- Step 8: reset compaction and compression states ---
        self.context_manager.reset_compaction();
        self.services.focus.reset();
        self.services.sidequest.reset();

        // --- Step 9: reset misc session-scoped fields ---
        self.runtime.debug.iteration_counter = 0;
        self.msg.last_persisted_message_id = None;
        self.msg.deferred_db_hide_ids.clear();
        self.msg.deferred_db_summaries.clear();
        self.services.tool_state.cached_filtered_tool_ids = None;
        self.runtime.providers.cached_prompt_tokens = 0;

        // --- Step 10: update conversation ID and memory state ---
        self.services.memory.persistence.conversation_id = new_conversation_id;
        self.services.memory.persistence.unsummarized_count = 0;
        // Clear cached digest — the new conversation has no prior digest yet.
        self.services.memory.compaction.cached_session_digest = None;
        // Reset MemCoT per-session distillation counters so the new conversation starts fresh.
        if let Some(ref acc) = self.services.memory.extraction.memcot_accumulator {
            acc.reset_session_counters().await;
        }

        // --- Step 11: clear TUI status ---
        if let Some(ref tx) = self.services.session.status_tx {
            let _ = tx.send(String::new());
        }

        Ok((old_conversation_id, new_conversation_id))
    }

    /// Gather context from all memory sources and inject into the message window.
    ///
    /// Delegates to [`zeph_agent_context::ContextService::prepare_context`] and then
    /// applies the returned [`ContextDelta`] (injects code context via
    /// [`Self::inject_code_context`] which stays on `Agent<C>` per scope decision).
    #[allow(clippy::too_many_lines)] // view construction: all fields are required by ContextAssemblyView; splitting would reduce readability
    pub(in crate::agent) async fn prepare_context(
        &mut self,
        query: &str,
    ) -> Result<(), super::super::error::AgentError> {
        use zeph_agent_context::state::ContextAssemblyView;

        let svc = zeph_agent_context::ContextService::new();

        // Capture values that are needed in the view but cannot be borrowed mutably alongside
        // the mutable borrows in window/view — snapshot before establishing the long-lived
        // mutable borrows so the borrow checker accepts disjoint field access.
        let cached_prompt_tokens_snapshot = self.runtime.providers.cached_prompt_tokens;
        let providers = self.providers();

        let correction_config = self.services.learning_engine.config.as_ref().map(|c| {
            zeph_context::input::CorrectionConfig {
                correction_detection: c.correction_detection,
                correction_recall_limit: c.correction_recall_limit,
                correction_min_similarity: c.correction_min_similarity,
            }
        });

        let mut security_sink = SecuritySink(&mut self.runtime.metrics.metrics_tx);

        // Snapshot MemCoT semantic state before constructing the view (requires async read).
        let memcot_state = if let Some(ref acc) = self.services.memory.extraction.memcot_accumulator
        {
            acc.current_state().await
        } else {
            None
        };

        // Construct the view using disjoint field projections.
        // Each `&mut` resolves to a unique top-level path under `Agent<C>`.
        let mut window = zeph_agent_context::state::MessageWindowView {
            messages: &mut self.msg.messages,
            last_persisted_message_id: &mut self.msg.last_persisted_message_id,
            deferred_db_hide_ids: &mut self.msg.deferred_db_hide_ids,
            deferred_db_summaries: &mut self.msg.deferred_db_summaries,
            cached_prompt_tokens: &mut self.runtime.providers.cached_prompt_tokens,
            token_counter: Arc::clone(&self.runtime.metrics.token_counter),
            completed_tool_ids: &mut self.services.tool_state.completed_tool_ids,
        };

        let mut view = ContextAssemblyView {
            memory: self.services.memory.persistence.memory.clone(),
            conversation_id: self.services.memory.persistence.conversation_id,
            recall_limit: self.services.memory.persistence.recall_limit,
            cross_session_score_threshold: self
                .services
                .memory
                .persistence
                .cross_session_score_threshold,
            context_format: self.services.memory.persistence.context_format,
            last_recall_confidence: &mut self.services.memory.persistence.last_recall_confidence,
            context_strategy: self.services.memory.compaction.context_strategy,
            crossover_turn_threshold: self.services.memory.compaction.crossover_turn_threshold,
            cached_session_digest: self
                .services
                .memory
                .compaction
                .cached_session_digest
                .clone(),
            digest_enabled: self.services.memory.compaction.digest_config.enabled,
            graph_config: self.services.memory.extraction.graph_config.clone(),
            document_config: self.services.memory.extraction.document_config.clone(),
            persona_config: self.services.memory.extraction.persona_config.clone(),
            trajectory_config: self.services.memory.extraction.trajectory_config.clone(),
            reasoning_config: self.services.memory.extraction.reasoning_config.clone(),
            memcot_config: self.services.memory.extraction.memcot_config.clone(),
            memcot_state,
            tree_config: self.services.memory.subsystems.tree_config.clone(),
            last_skills_prompt: &mut self.services.skill.last_skills_prompt,
            active_skill_names: &mut self.services.skill.active_skill_names,
            skill_registry: Arc::clone(&self.services.skill.registry),
            skill_paths: &self.services.skill.skill_paths,
            correction_config,
            sidequest_turn_counter: self.services.sidequest.turn_counter,
            proactive_explorer: self.services.proactive_explorer.clone(),
            sanitizer: &self.services.security.sanitizer,
            quarantine_summarizer: self.services.security.quarantine_summarizer.as_ref(),
            context_manager: &mut self.context_manager,
            token_counter: Arc::clone(&self.runtime.metrics.token_counter),
            metrics: zeph_agent_context::MetricsCounters::default(),
            security_events: &mut security_sink,
            cached_prompt_tokens: cached_prompt_tokens_snapshot,
            redact_credentials: self.runtime.config.redact_credentials,
            channel_skills: &self.runtime.config.channel_skills.allowed,
            scrub: crate::redact::scrub_content,
            #[cfg(feature = "index")]
            index: Some(&self.services.index as &dyn zeph_context::input::IndexAccess),
        };
        let _ = self.channel.send_status("recalling context...").await;
        let result = svc
            .prepare_context(query, &mut window, &mut view, &providers)
            .await;
        let _ = self.channel.send_status("").await;

        let delta =
            result.map_err(|e| super::super::error::AgentError::ContextError(format!("{e:#}")))?;

        // Apply accumulated metric deltas to the metrics snapshot.
        let m = view.metrics;
        self.update_metrics(|ms| {
            ms.sanitizer_runs += m.sanitizer_runs;
            ms.sanitizer_injection_flags += m.sanitizer_injection_flags;
            ms.sanitizer_truncations += m.sanitizer_truncations;
            ms.quarantine_invocations += m.quarantine_invocations;
            ms.quarantine_failures += m.quarantine_failures;
        });

        if let Some(body) = delta.code_context {
            self.inject_code_context(&body);
        }
        Ok(())
    }

    /// Delegate skill disambiguation to [`ContextService::disambiguate_skills`].
    pub(super) async fn disambiguate_skills(
        &self,
        query: &str,
        all_meta: &[&SkillMeta],
        scored: &[ScoredMatch],
    ) -> Option<Vec<usize>> {
        let svc = zeph_agent_context::ContextService::new();
        let providers = self.providers();
        svc.disambiguate_skills(query, all_meta, scored, &providers)
            .await
    }

    #[allow(clippy::too_many_lines)] // system prompt assembly: skills + tools + knowledge sections, tightly coupled formatting
    pub(in crate::agent) async fn rebuild_system_prompt(&mut self, query: &str) {
        let all_meta: Vec<zeph_skills::loader::SkillMeta> = self
            .services
            .skill
            .registry
            .read()
            .all_meta()
            .into_iter()
            .cloned()
            .collect();
        let all_meta_refs: Vec<&zeph_skills::loader::SkillMeta> = all_meta.iter().collect();
        let all_meta = all_meta_refs;
        // Tracks only skills that were genuinely scored by the embedding matcher.
        // Stays empty when falling back to the full skill set (no matcher, embed failure).
        let mut skills_to_record: Vec<String> = Vec::new();

        let matched_indices: Vec<usize> = if let Some(matcher) = &self.services.skill.matcher {
            let provider = self.embedding_provider.clone();
            let _ = self.channel.send_status("matching skills...").await;
            let match_result = matcher
                .match_skills(
                    &all_meta,
                    query,
                    self.services.skill.max_active_skills,
                    self.services.skill.two_stage_matching,
                    |text| {
                        let owned = text.to_owned();
                        let p = provider.clone();
                        Box::pin(async move { p.embed(&owned).await })
                    },
                )
                .await;
            let (mut scored, infra_error) = match match_result {
                zeph_skills::MatchResult::InfraError => (Vec::new(), true),
                zeph_skills::MatchResult::Scored(v) => (v, false),
            };

            if !scored.is_empty() {
                if self.services.skill.hybrid_search
                    && let Some(ref bm25) = self.services.skill.bm25_index
                {
                    let bm25_results = bm25.search(query, self.services.skill.max_active_skills);
                    scored = zeph_skills::bm25::rrf_fuse(
                        &scored,
                        &bm25_results,
                        self.services.skill.max_active_skills,
                    );
                }

                let metrics_map: std::collections::HashMap<String, (u32, u32)> =
                    if let Some(memory) = &self.services.memory.persistence.memory {
                        memory
                            .sqlite()
                            .load_skill_outcome_stats()
                            .await
                            .unwrap_or_default()
                            .into_iter()
                            .map(|m| {
                                let pair = (
                                    u32::try_from(m.successes).unwrap_or(0),
                                    u32::try_from(m.failures).unwrap_or(0),
                                );
                                (m.skill_name, pair)
                            })
                            .collect()
                    } else {
                        std::collections::HashMap::new()
                    };
                zeph_skills::trust_score::rerank(
                    &mut scored,
                    self.services.skill.cosine_weight,
                    |idx| {
                        all_meta
                            .get(idx)
                            .and_then(|m| metrics_map.get(&m.name))
                            .copied()
                            .unwrap_or((0, 0))
                    },
                );

                // SkillOrchestra: RL routing head re-rank (past warmup only).
                if let Some(rl_head) = &self.services.skill.rl_head
                    && let Ok(query_embed) = self.embedding_provider.embed(query).await
                    && {
                        let ok = query_embed.len() == rl_head.embed_dim();
                        if !ok {
                            tracing::warn!(
                                query_dim = query_embed.len(),
                                head_dim = rl_head.embed_dim(),
                                "rl_head: embed dim mismatch, skipping RL re-rank this turn"
                            );
                        }
                        ok
                    }
                {
                    let rl_weight = self.services.skill.rl_weight;
                    let warmup = self.services.skill.rl_warmup_updates;
                    // Build candidates: (skill_index, skill_embed, cosine_score).
                    // Skills without a stored embedding are skipped (Qdrant backend).
                    let candidates: Vec<(usize, &[f32], f32)> = scored
                        .iter()
                        .filter_map(|s| {
                            matcher
                                .skill_embedding(s.index)
                                .map(|emb| (s.index, emb, s.score))
                        })
                        .collect();
                    if candidates.len() == scored.len() {
                        let stats: Vec<(f32, u32)> = candidates
                            .iter()
                            .map(|&(idx, _, _)| {
                                let (succ, fail) = all_meta
                                    .get(idx)
                                    .and_then(|m| metrics_map.get(&m.name))
                                    .copied()
                                    .unwrap_or((0, 0));
                                let total = succ + fail;
                                let rate = if total == 0 {
                                    0.5
                                } else {
                                    #[allow(clippy::cast_precision_loss)]
                                    {
                                        succ as f32 / total as f32
                                    }
                                };
                                (rate, total)
                            })
                            .collect();
                        let reranked =
                            rl_head.rerank(&query_embed, &candidates, &stats, rl_weight, warmup);
                        // Apply new order to scored.
                        scored.sort_by(|a, b| {
                            let pos_a = reranked.iter().position(|(i, _)| *i == a.index);
                            let pos_b = reranked.iter().position(|(i, _)| *i == b.index);
                            pos_a.cmp(&pos_b)
                        });
                    } else {
                        tracing::debug!(
                            total = scored.len(),
                            with_embeddings = candidates.len(),
                            "RL re-rank skipped: skill embeddings unavailable (Qdrant backend does not expose in-process vectors)"
                        );
                    }
                }
            }

            let indices: Vec<usize> = if infra_error {
                // Embed or Qdrant infrastructure failure: fall back to all skills so the agent
                // remains functional rather than running with an empty skill set.
                tracing::warn!("skill matcher infrastructure error, falling back to all skills");
                (0..all_meta.len()).collect()
            } else {
                // Drop skills whose score falls below the minimum injection floor.
                let min_score = self.services.skill.min_injection_score;
                let pre_retain_count = scored.len();
                let max_score_before_retain = scored
                    .iter()
                    .map(|s| s.score)
                    .fold(f32::NEG_INFINITY, f32::max);
                scored.retain(|s| s.score >= min_score);
                if scored.is_empty() {
                    tracing::warn!(
                        candidate_count = pre_retain_count,
                        threshold = min_score,
                        max_score = max_score_before_retain,
                        "all skill candidates dropped below min_injection_score threshold; running without skills this turn"
                    );
                }

                // Capture the names of skills that had real embedding scores for
                // usage stats — before disambiguation may reorder indices.
                skills_to_record = scored
                    .iter()
                    .filter_map(|s| all_meta.get(s.index).map(|m| m.name.clone()))
                    .collect();

                if scored.len() >= 2
                    && (scored[0].score - scored[1].score)
                        < self.services.skill.disambiguation_threshold
                {
                    match self.disambiguate_skills(query, &all_meta, &scored).await {
                        Some(reordered) => reordered,
                        None => scored.iter().map(|s| s.index).collect(),
                    }
                } else {
                    scored.iter().map(|s| s.index).collect()
                }
            };
            let _ = self.channel.send_status("").await;
            indices
        } else {
            (0..all_meta.len()).collect()
        };

        let matched_indices: Vec<usize> = matched_indices
            .into_iter()
            .filter(|&i| {
                let Some(meta) = all_meta.get(i) else {
                    return false;
                };
                let missing: Vec<&str> = meta
                    .requires_secrets
                    .iter()
                    .filter(|s| {
                        !self
                            .services
                            .skill
                            .available_custom_secrets
                            .contains_key(s.as_str())
                    })
                    .map(String::as_str)
                    .collect();
                if !missing.is_empty() {
                    tracing::info!(
                        skill = %meta.name,
                        missing = ?missing,
                        "skill deactivated: missing required secrets"
                    );
                    return false;
                }
                true
            })
            .collect();

        self.services.skill.active_skill_names = matched_indices
            .iter()
            .filter_map(|&i| all_meta.get(i).map(|m| m.name.clone()))
            .collect();

        let skill_names = self.services.skill.active_skill_names.clone();
        let total = all_meta.len();
        self.update_metrics(|m| {
            m.active_skills = skill_names;
            m.total_skills = total;
        });

        if !skills_to_record.is_empty()
            && let Some(memory) = &self.services.memory.persistence.memory
        {
            let names: Vec<&str> = skills_to_record.iter().map(String::as_str).collect();
            if let Err(e) = memory.sqlite().record_skill_usage(&names).await {
                tracing::warn!("failed to record skill usage: {e:#}");
            }
        }
        self.update_skill_confidence_metrics().await;

        let (all_skills, active_skills): (Vec<Skill>, Vec<Skill>) = {
            let reg = self.services.skill.registry.read();
            let all: Vec<Skill> = reg
                .all_meta()
                .iter()
                .filter_map(|m| reg.skill(&m.name).ok())
                .filter(|s| {
                    let allowed = zeph_config::is_skill_allowed(
                        s.name(),
                        &self.runtime.config.channel_skills,
                    );
                    if !allowed {
                        tracing::debug!(skill = s.name(), "skill excluded by channel allowlist");
                    }
                    allowed
                })
                .collect();
            let active: Vec<Skill> = self
                .services
                .skill
                .active_skill_names
                .iter()
                .filter_map(|name| reg.skill(name).ok())
                .filter(|s| {
                    let allowed = zeph_config::is_skill_allowed(
                        s.name(),
                        &self.runtime.config.channel_skills,
                    );
                    if !allowed {
                        tracing::debug!(
                            skill = s.name(),
                            "active skill excluded by channel allowlist"
                        );
                    }
                    allowed
                })
                .collect();
            (all, active)
        };
        let trust_map = self.build_skill_trust_map().await;

        // Write the per-turn trust snapshot so SkillInvokeExecutor can resolve trust
        // without re-querying the store on every tool call.
        self.services
            .skill
            .trust_snapshot
            .write()
            .clone_from(&trust_map);

        let remaining_skills: Vec<Skill> = all_skills
            .iter()
            .filter(|s| {
                !self
                    .services
                    .skill
                    .active_skill_names
                    .contains(&s.name().to_string())
            })
            .filter(|s| match trust_map.get(s.name()) {
                Some(zeph_common::SkillTrustLevel::Blocked) => {
                    tracing::debug!(skill = s.name(), "excluded from catalog: trust=blocked");
                    false
                }
                _ => true,
            })
            .cloned()
            .collect();

        // Apply the most restrictive trust level among active skills to the executor gate.
        let effective_trust = if self.services.skill.active_skill_names.is_empty() {
            zeph_common::SkillTrustLevel::Trusted
        } else {
            self.services
                .skill
                .active_skill_names
                .iter()
                .filter_map(|name| trust_map.get(name).copied())
                .fold(zeph_common::SkillTrustLevel::Trusted, |acc, lvl| {
                    acc.min_trust(lvl)
                })
        };
        self.tool_executor.set_effective_trust(effective_trust);

        // Build health_map: skill_name -> (posterior_mean, total_uses) for XML attributes.
        let health_map: std::collections::HashMap<String, (f64, u32)> = if let Some(memory) =
            &self.services.memory.persistence.memory
        {
            memory
                .sqlite()
                .load_skill_outcome_stats()
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|m| {
                    let successes = u32::try_from(m.successes).unwrap_or(0);
                    let failures = u32::try_from(m.failures).unwrap_or(0);
                    let total = successes + failures;
                    let posterior = zeph_skills::trust_score::posterior_mean(successes, failures);
                    (m.skill_name, (posterior, total))
                })
                .collect()
        } else {
            std::collections::HashMap::new()
        };

        let effective_mode = match self.services.skill.prompt_mode {
            crate::config::SkillPromptMode::Auto => {
                if let Some(ref budget) = self.context_manager.budget
                    && budget.max_tokens() < 8192
                {
                    crate::config::SkillPromptMode::Compact
                } else {
                    crate::config::SkillPromptMode::Full
                }
            }
            other => other,
        };

        let mut skills_prompt = if effective_mode == crate::config::SkillPromptMode::Compact {
            format_skills_prompt_compact(&active_skills)
        } else {
            format_skills_prompt(&active_skills, &trust_map, &health_map)
        };
        // ERL: append learned heuristics for active skills (no-op when erl_enabled = false).
        let erl_suffix = self.build_erl_heuristics_prompt().await;
        if !erl_suffix.is_empty() {
            skills_prompt.push_str(&erl_suffix);
        }
        let catalog_prompt = format_skills_catalog(&remaining_skills);
        self.services
            .skill
            .last_skills_prompt
            .clone_from(&skills_prompt);
        self.services.session.env_context.refresh_git_branch();
        self.services
            .session
            .env_context
            .model_name
            .clone_from(&self.runtime.config.model_name);

        // MCP tool discovery (#2321 / #2298): select tools relevant to this turn's query.
        // Strategy dispatch: Embedding (new), Llm (existing prune_tools_cached), None (all).
        // Runs before the schema filter so the selected subset feeds into the combined
        // (native + MCP) tool set that the schema filter operates on.
        if !self.services.mcp.tools.is_empty() {
            match self.services.mcp.discovery_strategy {
                zeph_mcp::ToolDiscoveryStrategy::Embedding => {
                    let params = self.services.mcp.discovery_params.clone();
                    if self.services.mcp.tools.len() < params.min_tools_to_filter {
                        // Below threshold — skip filtering.
                        self.services.mcp.sync_executor_tools();
                    } else if let Some(ref index) = self.services.mcp.semantic_index {
                        // Resolve embedding provider for query.
                        let embed_provider = self
                            .services
                            .mcp
                            .discovery_provider
                            .clone()
                            .unwrap_or_else(|| self.embedding_provider.clone());
                        let _ = self.channel.send_status("selecting tools...").await;
                        match embed_provider.embed(query).await {
                            Ok(query_emb) => {
                                let selected = index.select(
                                    &query_emb,
                                    params.top_k,
                                    params.min_similarity,
                                    &params.always_include,
                                );
                                tracing::info!(
                                    total = self.services.mcp.tools.len(),
                                    selected = selected.len(),
                                    "semantic tool discovery applied"
                                );
                                self.services.mcp.apply_pruned_tools(selected);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    strict = params.strict,
                                    "semantic tool discovery: query embed failed, falling back to all tools: {e:#}"
                                );
                                if !params.strict {
                                    self.services.mcp.sync_executor_tools();
                                }
                                // strict=true: do not sync — executor retains whatever tools it had
                                // (either previously synced or empty). The turn will proceed without
                                // MCP tools rather than silently degrading to the full unfiltered set.
                            }
                        }
                        let _ = self.channel.send_status("").await;
                    } else {
                        // Index not built (build failed at connect time).
                        tracing::warn!(
                            strict = params.strict,
                            "semantic tool discovery: index not available, falling back to all tools"
                        );
                        if !params.strict {
                            self.services.mcp.sync_executor_tools();
                        }
                    }
                }
                zeph_mcp::ToolDiscoveryStrategy::Llm => {
                    if self.services.mcp.pruning_enabled {
                        let pruning_provider = self
                            .services
                            .mcp
                            .pruning_provider
                            .clone()
                            .unwrap_or_else(|| self.provider.clone());
                        let tools_snapshot = self.services.mcp.tools.clone();
                        let params_snapshot = self.services.mcp.pruning_params.clone();
                        match zeph_mcp::prune_tools_cached(
                            &mut self.services.mcp.pruning_cache,
                            &tools_snapshot,
                            query,
                            &params_snapshot,
                            &pruning_provider,
                        )
                        .await
                        {
                            Ok(pruned) => {
                                self.services.mcp.apply_pruned_tools(pruned);
                            }
                            Err(e) => {
                                tracing::warn!("MCP pruning failed, using all tools: {e:#}");
                                self.services.mcp.sync_executor_tools();
                            }
                        }
                    } else {
                        // pruning_enabled=false: pass all tools through.
                        self.services.mcp.sync_executor_tools();
                    }
                }
                zeph_mcp::ToolDiscoveryStrategy::None => {
                    // Pass all tools through without filtering.
                    self.services.mcp.sync_executor_tools();
                }
            }
        }

        // Dynamic tool schema filtering (#2020): compute once per turn, cache for native path.
        // Query embedding is computed here; when strategy=Embedding already computed it above,
        // but providers are stateless so a second embed() call is acceptable for MVP.
        self.services.tool_state.cached_filtered_tool_ids = None;
        if let Some(ref filter) = self.services.tool_state.tool_schema_filter {
            let defs = self.tool_executor.tool_definitions_erased();
            let all_ids: Vec<&str> = defs.iter().map(|d| d.id.as_ref()).collect();
            let descriptions: Vec<(&str, &str)> = defs
                .iter()
                .map(|d| (d.id.as_ref(), d.description.as_ref()))
                .collect();

            let _ = self.channel.send_status("filtering tools...").await;
            match self.embedding_provider.embed(query).await {
                Ok(query_emb) => {
                    let mut result = filter.filter(&all_ids, &descriptions, query, &query_emb);

                    // Apply dependency graph AFTER schema filter (and after any TAFC
                    // augmentation that may have added tools). This ensures hard gates
                    // are the final word on tool availability (MED-04 fix).
                    if let Some(ref dep_graph) = self.services.tool_state.dependency_graph {
                        let dep_config = &self.runtime.config.dependency_config;
                        dep_graph.apply(
                            &mut result,
                            &self.services.tool_state.completed_tool_ids,
                            dep_config.boost_per_dep,
                            dep_config.max_total_boost,
                            &self.services.tool_state.dependency_always_on,
                        );
                        if !result.dependency_exclusions.is_empty() {
                            tracing::info!(
                                excluded = result.dependency_exclusions.len(),
                                "tool dependency gate: excluded tools with unmet requires"
                            );
                            for excl in &result.dependency_exclusions {
                                tracing::debug!(
                                    tool_id = %excl.tool_id,
                                    unmet = ?excl.unmet_requires,
                                    "tool dependency gate exclusion"
                                );
                            }
                        }
                    }

                    tracing::info!(
                        total = all_ids.len(),
                        included = result.included.len(),
                        excluded = result.excluded.len(),
                        dep_excluded = result.dependency_exclusions.len(),
                        "tool schema filter applied"
                    );
                    for (tool_id, score) in &result.scores {
                        tracing::debug!(tool_id, score, "tool similarity score");
                    }
                    for (tool_id, reason) in &result.inclusion_reasons {
                        tracing::debug!(tool_id, ?reason, "tool inclusion reason");
                    }
                    self.services.tool_state.cached_filtered_tool_ids = Some(result.included);
                }
                Err(e) => {
                    tracing::warn!("tool filter: query embed failed, using all tools: {e:#}");
                }
            }
            let _ = self.channel.send_status("").await;
        }

        // BLOCK 1: stable within a session — base prompt + skills + tool catalog
        // Instruction blocks are passed separately and injected in the volatile section.
        #[allow(unused_mut)]
        let mut system_prompt = build_system_prompt_with_instructions(
            &skills_prompt,
            Some(&self.services.session.env_context),
            &self.runtime.instructions.blocks,
        );

        // BLOCK 2: semi-stable within a session — skills catalog, MCP, project context, repo map
        if !catalog_prompt.is_empty() {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&catalog_prompt);
        }

        system_prompt.push_str("\n<!-- cache:stable -->");

        self.append_mcp_prompt(query, &mut system_prompt).await;

        let cwd = match self.services.session.env_context.working_dir.as_str() {
            "" | "unknown" => std::env::current_dir().unwrap_or_default(),
            dir => PathBuf::from(dir),
        };
        let project_configs = crate::project::discover_project_configs(&cwd);
        let project_context = crate::project::load_project_context(&project_configs);
        if !project_context.is_empty() {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&project_context);
        }

        if self.services.index.repo_map_tokens > 0 {
            let now = std::time::Instant::now();
            let map = if let Some((ref cached, generated_at)) = self.services.index.cached_repo_map
                && now.duration_since(generated_at) < self.services.index.repo_map_ttl
            {
                cached.clone()
            } else {
                let cwd2 = cwd.clone();
                let token_budget = self.services.index.repo_map_tokens;
                let tc = Arc::clone(&self.runtime.metrics.token_counter);
                let fresh = tokio::task::spawn_blocking(move || {
                    zeph_index::repo_map::generate_repo_map(&cwd2, token_budget, &tc)
                })
                .await
                .unwrap_or_else(|_| Ok(String::new()))
                .unwrap_or_default();
                self.services.index.cached_repo_map = Some((fresh.clone(), now));
                fresh
            };
            if !map.is_empty() {
                system_prompt.push_str("\n\n");
                system_prompt.push_str(&map);
            }
        }

        // BLOCK 3: volatile — dynamic per-turn content, never cached
        system_prompt.push_str("\n<!-- cache:volatile -->");

        // Inject learned user preferences after the volatile marker so they
        // do not invalidate the stable/semi-stable cache blocks (S2 fix).
        self.inject_learned_preferences(&mut system_prompt).await;

        // If memory_save was used this session, remind the model to use memory_search
        // (not search_code) to recall user-provided facts (#2475).
        if self
            .services
            .tool_state
            .completed_tool_ids
            .contains("memory_save")
        {
            system_prompt.push_str(
                "\n\nFacts provided by the user in this session have been saved with memory_save — use memory_search to recall them, not search_code.",
            );
        }

        // Budget hint injection (#2267): inject remaining cost and tool call budget so the
        // LLM can self-regulate. Only injected when budget_hint_enabled = true (default).
        // Self-suppresses when no budget data sources are available.
        if self.runtime.config.budget_hint_enabled {
            let remaining_cost_cents = self.runtime.metrics.cost_tracker.as_ref().and_then(|ct| {
                let max = ct.max_daily_cents();
                if max > 0.0 {
                    Some((max - ct.current_spend()).max(0.0))
                } else {
                    None
                }
            });
            let total_budget_cents = self.runtime.metrics.cost_tracker.as_ref().and_then(|ct| {
                let max = ct.max_daily_cents();
                if max > 0.0 { Some(max) } else { None }
            });
            let max_tool_calls = self.tool_orchestrator.max_iterations;
            let remaining_tool_calls =
                max_tool_calls.saturating_sub(self.services.tool_state.current_tool_iteration);
            let hint = BudgetHint {
                remaining_cost_cents,
                total_budget_cents,
                remaining_tool_calls,
                max_tool_calls,
            };
            if let Some(xml) = hint.format_xml() {
                system_prompt.push_str("\n\n");
                system_prompt.push_str(&xml);
            }
        }

        tracing::debug!(
            len = system_prompt.len(),
            skills = ?self.services.skill.active_skill_names,
            "system prompt rebuilt"
        );
        tracing::trace!(prompt = %system_prompt, "full system prompt");

        if let Some(msg) = self.msg.messages.first_mut() {
            msg.content = system_prompt;
        }
    }
}

// ── Test-only integration bridges ─────────────────────────────────────────────
//
// These shim methods expose individual context-service operations directly on
// `Agent<C>` so that Category 2 integration tests can drive them in isolation
// without going through the full `prepare_context` pipeline. They are not part
// of the production call path — production code uses `ContextService` methods
// directly via `prepare_context`.
#[cfg(test)]
impl<C: Channel> Agent<C> {
    pub(in crate::agent) fn remove_recall_messages(&mut self) {
        let svc = zeph_agent_context::ContextService::new();
        svc.remove_recall_messages(&mut self.message_window_view());
    }

    pub(in crate::agent) fn remove_correction_messages(&mut self) {
        let svc = zeph_agent_context::ContextService::new();
        svc.remove_correction_messages(&mut self.message_window_view());
    }

    pub(in crate::agent) async fn inject_semantic_recall(
        &mut self,
        query: &str,
        token_budget: usize,
    ) -> Result<(), super::super::error::AgentError> {
        self.remove_recall_messages();

        let (msg, _score) = zeph_agent_context::helpers::fetch_semantic_recall_raw(
            self.services.memory.persistence.memory.as_deref(),
            self.services.memory.persistence.recall_limit,
            self.services.memory.persistence.context_format,
            query,
            token_budget,
            &self.runtime.metrics.token_counter,
            None,
        )
        .await
        .map_err(|e| super::super::error::AgentError::ContextError(format!("{e:#}")))?;
        if let Some(msg) = msg
            && self.msg.messages.len() > 1
        {
            self.msg.messages.insert(1, msg);
        }

        Ok(())
    }

    pub(in crate::agent) fn remove_summary_messages(&mut self) {
        let svc = zeph_agent_context::ContextService::new();
        svc.remove_summary_messages(&mut self.message_window_view());
    }

    pub(in crate::agent) fn remove_cross_session_messages(&mut self) {
        let svc = zeph_agent_context::ContextService::new();
        svc.remove_cross_session_messages(&mut self.message_window_view());
    }

    pub(in crate::agent) async fn inject_cross_session_context(
        &mut self,
        query: &str,
        token_budget: usize,
    ) -> Result<(), super::super::error::AgentError> {
        self.remove_cross_session_messages();

        if let Some(msg) = zeph_agent_context::helpers::fetch_cross_session_raw(
            self.services.memory.persistence.memory.as_deref(),
            self.services.memory.persistence.conversation_id,
            self.services
                .memory
                .persistence
                .cross_session_score_threshold,
            query,
            token_budget,
            &self.runtime.metrics.token_counter,
        )
        .await
        .map_err(|e| super::super::error::AgentError::ContextError(format!("{e:#}")))?
            && self.msg.messages.len() > 1
        {
            self.msg.messages.insert(1, msg);
            tracing::debug!("injected cross-session context");
        }

        Ok(())
    }

    pub(in crate::agent) async fn inject_summaries(
        &mut self,
        token_budget: usize,
    ) -> Result<(), super::super::error::AgentError> {
        self.remove_summary_messages();

        if let Some(msg) = zeph_agent_context::helpers::fetch_summaries_raw(
            self.services.memory.persistence.memory.as_deref(),
            self.services.memory.persistence.conversation_id,
            token_budget,
            &self.runtime.metrics.token_counter,
        )
        .await
        .map_err(|e| super::super::error::AgentError::ContextError(format!("{e:#}")))?
            && self.msg.messages.len() > 1
        {
            self.msg.messages.insert(1, msg);
            tracing::debug!("injected summaries into context");
        }

        Ok(())
    }

    pub(in crate::agent) fn trim_messages_to_budget(&mut self, token_budget: usize) {
        let svc = zeph_agent_context::ContextService::new();
        svc.trim_messages_to_budget(&mut self.message_window_view(), token_budget);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_context::assembler::{MAX_KEEP_TAIL_SCAN, memory_first_keep_tail};
    use zeph_llm::provider::{Message, MessagePart, Role};

    // ── effective_recall_timeout_ms tests (#2514) ────────────────────────────

    #[test]
    fn effective_recall_timeout_ms_nonzero_returns_unchanged() {
        let result = zeph_agent_context::helpers::effective_recall_timeout_ms(500);
        assert_eq!(result, 500, "non-zero value must pass through unchanged");
    }

    #[test]
    fn effective_recall_timeout_ms_nonzero_large_returns_unchanged() {
        let result = zeph_agent_context::helpers::effective_recall_timeout_ms(5000);
        assert_eq!(result, 5000);
    }

    #[test]
    fn effective_recall_timeout_ms_zero_clamps_to_100() {
        let result = zeph_agent_context::helpers::effective_recall_timeout_ms(0);
        assert_eq!(
            result, 100,
            "zero recall_timeout_ms must be clamped to 100ms"
        );
    }

    #[test]
    fn spreading_activation_default_timeout_is_nonzero() {
        // Ensures the default used in production is not accidentally set to zero —
        // which would always trigger the zero-clamp warn path in effective_recall_timeout_ms.
        let result = zeph_agent_context::helpers::effective_recall_timeout_ms(
            zeph_config::memory::SpreadingActivationConfig::default().recall_timeout_ms,
        );
        assert!(
            result > 0,
            "default recall_timeout_ms must produce a non-zero effective value"
        );
    }

    fn sys() -> Message {
        Message::from_legacy(Role::System, "system prompt")
    }

    fn user(text: &str) -> Message {
        Message::from_legacy(Role::User, text)
    }

    fn assistant(text: &str) -> Message {
        Message::from_legacy(Role::Assistant, text)
    }

    fn tool_use_msg() -> Message {
        Message::from_parts(
            Role::Assistant,
            vec![MessagePart::ToolUse {
                id: "tu1".into(),
                name: "shell".into(),
                input: serde_json::json!({}),
            }],
        )
    }

    fn tool_result_msg() -> Message {
        Message::from_parts(
            Role::User,
            vec![MessagePart::ToolResult {
                tool_use_id: "tu1".into(),
                content: "output".into(),
                is_error: false,
            }],
        )
    }

    #[test]
    fn keep_tail_no_tool_calls_returns_two() {
        // Normal conversation: no tool calls at boundary — keep_tail stays 2.
        let msgs = vec![
            sys(),
            user("hello"),
            assistant("hi"),
            user("how are you"),
            assistant("fine"),
        ];
        assert_eq!(memory_first_keep_tail(&msgs, 1), 2);
    }

    #[test]
    fn keep_tail_tool_result_at_boundary_extends_by_one() {
        // Last 2 messages: [tool_result, assistant_reply]
        // first_retained (index len-2) = tool_result  → must extend by 1 to include tool_use
        //   then first_retained becomes tool_use (Assistant) → stop
        let msgs = vec![
            sys(),
            user("q1"),
            assistant("a1"),
            tool_use_msg(),    // index 3: assistant issues tool call
            tool_result_msg(), // index 4: tool result
            assistant("done"), // index 5: assistant reply after tool
        ];
        // len=6, keep_tail starts at 2 → msgs[4]=tool_result → extend to 3 → msgs[3]=tool_use (Assistant) → stop
        assert_eq!(memory_first_keep_tail(&msgs, 1), 3);
    }

    #[test]
    fn keep_tail_multiple_tool_rounds_at_boundary() {
        // Two consecutive tool call/result pairs right before the final reply.
        let msgs = vec![
            sys(),
            user("q1"),
            assistant("a1"),
            tool_use_msg(),    // index 3
            tool_result_msg(), // index 4
            tool_use_msg(),    // index 5: second tool call
            tool_result_msg(), // index 6: second tool result
            assistant("done"), // index 7
        ];
        // len=8
        // keep_tail=2: msgs[6]=tool_result → extend
        // keep_tail=3: msgs[5]=tool_use (Assistant) → stop
        assert_eq!(memory_first_keep_tail(&msgs, 1), 3);
    }

    #[test]
    fn keep_tail_capped_at_available_history() {
        // Only system + one tool_result message (degenerate): keep_tail must not exceed len-history_start.
        let msgs = vec![sys(), tool_result_msg()];
        // len=2, len-history_start=1 → while condition `keep_tail < 1` is false from the start
        assert_eq!(memory_first_keep_tail(&msgs, 1), 2);
    }

    #[test]
    fn keep_tail_capped_at_max_scan_does_not_split_tool_pair() {
        // Build a history: system + (tool_use, tool_result) × 30 pairs + assistant reply.
        // Total: 1 + 60 + 1 = 62 messages. The cap fires at MAX_KEEP_TAIL_SCAN = 50.
        // At that point, keep_tail includes 49 ToolResult messages. The preceding message
        // (index len - 51) is a ToolUse — the fix must extend keep_tail to 51 so the pair
        // is not split.
        let mut msgs = vec![sys()];
        for _ in 0..30 {
            msgs.push(tool_use_msg());
            msgs.push(tool_result_msg());
        }
        msgs.push(assistant("done"));

        let tail = memory_first_keep_tail(&msgs, 1);

        // The result must not exceed MAX_KEEP_TAIL_SCAN + 1 (cap + one extra for ToolUse).
        assert!(
            tail <= MAX_KEEP_TAIL_SCAN + 1,
            "keep_tail {tail} exceeds cap + 1"
        );

        // Verify the first retained message is not a ToolResult without a preceding ToolUse.
        let len = msgs.len();
        let first_retained_idx = len - tail;
        // If the first retained message is a ToolResult, the message just before it must be
        // a ToolUse (or there is no message before it, which is impossible here).
        let first_retained = &msgs[first_retained_idx];
        let is_tool_result = first_retained.role == Role::User
            && first_retained
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolResult { .. }));
        if is_tool_result && first_retained_idx > 0 {
            let preceding = &msgs[first_retained_idx - 1];
            let has_tool_use = preceding.role == Role::Assistant
                && preceding
                    .parts
                    .iter()
                    .any(|p| matches!(p, MessagePart::ToolUse { .. }));
            assert!(
                has_tool_use,
                "ToolResult at index {first_retained_idx} has no preceding ToolUse — pair was split"
            );
        }
    }

    // ── BudgetHint tests (#2267) ─────────────────────────────────────────────

    #[test]
    fn budget_hint_all_none_no_xml_when_max_tools_zero() {
        let hint = BudgetHint {
            remaining_cost_cents: None,
            total_budget_cents: None,
            remaining_tool_calls: 0,
            max_tool_calls: 0,
        };
        assert!(hint.format_xml().is_none());
    }

    #[test]
    fn budget_hint_tool_only_produces_xml() {
        let hint = BudgetHint {
            remaining_cost_cents: None,
            total_budget_cents: None,
            remaining_tool_calls: 7,
            max_tool_calls: 10,
        };
        let xml = hint.format_xml().unwrap();
        assert!(xml.contains("<remaining_tool_calls>7</remaining_tool_calls>"));
        assert!(xml.contains("<max_tool_calls>10</max_tool_calls>"));
        assert!(!xml.contains("remaining_cost_cents"));
    }

    #[test]
    fn budget_hint_full_produces_all_fields() {
        let hint = BudgetHint {
            remaining_cost_cents: Some(42.5),
            total_budget_cents: Some(100.0),
            remaining_tool_calls: 5,
            max_tool_calls: 10,
        };
        let xml = hint.format_xml().unwrap();
        assert!(xml.contains("<remaining_cost_cents>42.50</remaining_cost_cents>"));
        assert!(xml.contains("<total_budget_cents>100.00</total_budget_cents>"));
        assert!(xml.contains("<remaining_tool_calls>5</remaining_tool_calls>"));
        assert!(xml.contains("<max_tool_calls>10</max_tool_calls>"));
    }

    #[test]
    fn budget_hint_zero_max_daily_cents_omits_cost_fields() {
        // max_daily_cents == 0.0 means unlimited — cost fields must be omitted.
        let hint = BudgetHint {
            remaining_cost_cents: None, // caller guards with > 0.0 check
            total_budget_cents: None,
            remaining_tool_calls: 3,
            max_tool_calls: 10,
        };
        let xml = hint.format_xml().unwrap();
        assert!(!xml.contains("remaining_cost_cents"));
        assert!(!xml.contains("total_budget_cents"));
    }

    // ── recall snippet filter (#2620) ────────────────────────────────────────

    /// Mirrors the filter condition in `fetch_semantic_recall` — used to verify
    /// that `[skipped]`/`[stopped]` markers are recognised and that normal
    /// snippets are not accidentally rejected.
    fn recall_is_policy_marker(content: &str) -> bool {
        content.starts_with("[skipped]") || content.starts_with("[stopped]")
    }

    /// Simulates the recall-text assembly loop from `fetch_semantic_recall`,
    /// returning only the snippets that pass the policy-marker filter.
    fn apply_recall_filter(snippets: &[&str]) -> Vec<String> {
        snippets
            .iter()
            .filter(|s| !recall_is_policy_marker(s))
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn recall_filter_skipped_marker_is_excluded() {
        let snippets = ["[skipped] bash was blocked by utility gate"];
        let result = apply_recall_filter(&snippets);
        assert!(
            result.is_empty(),
            "[skipped] snippet must be filtered from recall block"
        );
    }

    #[test]
    fn recall_filter_stopped_marker_is_excluded() {
        let snippets = ["[stopped] execution limit reached"];
        let result = apply_recall_filter(&snippets);
        assert!(
            result.is_empty(),
            "[stopped] snippet must be filtered from recall block"
        );
    }

    #[test]
    fn recall_filter_normal_snippet_passes_through() {
        let snippets = ["total 42\ndrwxr-xr-x  5 user group  160 Jan  1 00:00 src"];
        let result = apply_recall_filter(&snippets);
        assert_eq!(
            result.len(),
            1,
            "normal snippet must not be filtered from recall block"
        );
        assert_eq!(result[0], snippets[0]);
    }

    #[test]
    fn recall_filter_mixed_passes_only_normal_snippets() {
        let snippets = [
            "[skipped] bash blocked",
            "real output line",
            "[stopped] limit hit",
            "another real line",
        ];
        let result = apply_recall_filter(&snippets);
        assert_eq!(result, vec!["real output line", "another real line"]);
    }

    #[test]
    fn recall_filter_empty_content_is_not_a_marker() {
        // Empty string does not start with either marker → must pass through.
        let snippets = [""];
        let result = apply_recall_filter(&snippets);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn recall_filter_partial_prefix_is_not_a_marker() {
        // "[skip]" and "[stop]" are not the recognised markers.
        let snippets = ["[skip] not a real marker", "[stop] also not a marker"];
        let result = apply_recall_filter(&snippets);
        assert_eq!(
            result.len(),
            2,
            "partial prefixes must not be treated as policy markers"
        );
    }

    // ── Blocked-skill catalog filter (GAP-1) ────────────────────────────────

    #[test]
    fn blocked_skill_excluded_from_catalog_filter() {
        use std::collections::HashMap;
        use zeph_common::SkillTrustLevel;
        use zeph_skills::loader::SkillMeta;

        // Simulate the catalog filter: skills whose trust level is Blocked are dropped.
        let mut trust_map: HashMap<String, SkillTrustLevel> = HashMap::new();
        trust_map.insert("blocked-skill".to_owned(), SkillTrustLevel::Blocked);
        trust_map.insert("allowed-skill".to_owned(), SkillTrustLevel::Trusted);

        // Two minimal SkillMeta stubs.
        let make_meta = |name: &str| SkillMeta {
            name: name.to_owned(),
            description: "desc".to_owned(),
            compatibility: None,
            license: None,
            metadata: vec![],
            allowed_tools: vec![],
            requires_secrets: vec![],
            skill_dir: std::path::PathBuf::new(),
            source_url: None,
            git_hash: None,
            category: None,
        };
        let skills = [make_meta("blocked-skill"), make_meta("allowed-skill")];

        // Apply the same filter logic used in the catalog-building path.
        let catalog: Vec<_> = skills
            .iter()
            .filter(|s| {
                !matches!(
                    trust_map.get(s.name.as_str()),
                    Some(SkillTrustLevel::Blocked)
                )
            })
            .collect();

        assert_eq!(
            catalog.len(),
            1,
            "blocked skill must be excluded from catalog"
        );
        assert_eq!(catalog[0].name, "allowed-skill");
    }
}
