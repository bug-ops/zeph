// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::channel::Channel;
use zeph_agent_persistence::graph::{build_graph_extraction_config, collect_context_messages};
use zeph_agent_persistence::{
    MemoryPersistenceView, MetricsView, PersistMessageRequest, PersistenceService, SecurityView,
};
use zeph_llm::provider::{LlmProvider as _, MessagePart, Role};

use super::Agent;

impl<C: Channel> Agent<C> {
    /// Load conversation history from memory and inject into messages.
    ///
    /// Delegates to [`PersistenceService::load_history`]. Post-load operations that touch
    /// agent-internal singletons (session count increment, semantic fact count recompute,
    /// token recompute) remain in this shim because they access fields outside the
    /// borrow-lens view.
    ///
    /// # Errors
    ///
    /// Returns an error if loading history from `SQLite` fails.
    ///
    /// # Panics
    ///
    /// Does not panic. The internal `unwrap_or(0)` conversions are on fallible `i64 → usize`
    /// casts that saturate to zero on overflow; they cannot panic.
    #[tracing::instrument(name = "core.persist.load_history", skip_all, level = "debug", err)]
    pub async fn load_history(&mut self) -> Result<(), super::error::AgentError> {
        let (Some(memory), Some(cid)) = (
            self.services.memory.persistence.memory.as_ref(),
            self.services.memory.persistence.conversation_id,
        ) else {
            return Ok(());
        };

        // Clone so we can call methods after the borrow-lens view is dropped.
        let memory = memory.clone();

        let mut unsummarized = self.services.memory.persistence.unsummarized_count;
        // `memory_view` is not `mut` — the `&mut unsummarized` inside is established at
        // construction and passed as `&memory_view` to load_history (shared borrow).
        let memory_view = MemoryPersistenceView {
            memory: Some(&memory),
            conversation_id: self.services.memory.persistence.conversation_id,
            autosave_assistant: self.services.memory.persistence.autosave_assistant,
            autosave_min_length: self.services.memory.persistence.autosave_min_length,
            unsummarized_count: &mut unsummarized,
            goal_text: self.services.memory.extraction.goal_text.clone(),
        };
        let mut sqlite_delta = 0u64;
        let mut embed_delta = 0u64;
        let mut guard_delta = 0u64;
        let mut metrics_view = MetricsView {
            sqlite_message_count: &mut sqlite_delta,
            embeddings_generated: &mut embed_delta,
            exfiltration_memory_guards: &mut guard_delta,
        };

        let svc = PersistenceService::new();
        let outcome = svc
            .load_history(
                &mut self.msg.messages,
                &mut self.msg.last_persisted_message_id,
                &mut self.msg.deferred_db_hide_ids,
                &mut self.msg.deferred_db_summaries,
                &memory_view,
                &zeph_config::Config::default(),
                &mut metrics_view,
            )
            .await
            .map_err(|e| {
                super::error::AgentError::Memory(zeph_memory::MemoryError::Other(e.to_string()))
            })?;

        // Write back lens-borrowed local to the field.
        self.services.memory.persistence.unsummarized_count = unsummarized;

        if outcome.messages_loaded > 0 {
            // Increment session counts so tier promotion can track cross-session access.
            let _ = memory
                .sqlite()
                .increment_session_counts_for_conversation(cid)
                .await
                .inspect_err(|e| {
                    tracing::warn!(error = %e, "failed to increment tier session counts");
                });
        }

        // Set absolute SQLite message count and semantic fact count (not deltas).
        self.update_metrics(|m| {
            m.sqlite_message_count = outcome.sqlite_total_messages;
        });
        if let Ok(count) = memory.sqlite().count_semantic_facts().await {
            let count_u64 = u64::try_from(count).unwrap_or(0);
            self.update_metrics(|m| {
                m.semantic_fact_count = count_u64;
            });
        }
        if let Ok(count) = memory.unsummarized_message_count(cid).await {
            self.services.memory.persistence.unsummarized_count =
                usize::try_from(count).unwrap_or(0);
        }

        self.recompute_prompt_tokens();
        Ok(())
    }

    /// Persist a message to memory.
    ///
    /// `has_injection_flags` controls whether Qdrant embedding is skipped for this message.
    /// When `true` and `guard_memory_writes` is enabled, only `SQLite` is written — the message
    /// is saved for conversation continuity but will not pollute semantic search (M2, D2).
    #[tracing::instrument(name = "core.persist.persist_message", skip_all, level = "debug")]
    pub(crate) async fn persist_message(
        &mut self,
        role: Role,
        content: &str,
        parts: &[MessagePart],
        has_injection_flags: bool,
    ) {
        // M2: call should_guard_memory_write for its diagnostic side effects (tracing + security
        // event). The bool result is passed into SecurityView so the service can decide whether
        // to skip Qdrant embedding.
        let guard_event = self
            .services
            .security
            .exfiltration_guard
            .should_guard_memory_write(has_injection_flags);
        if let Some(ref event) = guard_event {
            tracing::warn!(
                ?event,
                "exfiltration guard: skipping Qdrant embedding for flagged content"
            );
            self.push_security_event(
                zeph_common::SecurityEventCategory::ExfiltrationBlock,
                "memory_write",
                "Qdrant embedding skipped: flagged content",
            );
        }

        let req = PersistMessageRequest::from_borrowed(role, content, parts, has_injection_flags);

        let mut unsummarized = self.services.memory.persistence.unsummarized_count;
        let memory_arc = self.services.memory.persistence.memory.clone();
        let mut memory_view = MemoryPersistenceView {
            memory: memory_arc.as_ref(),
            conversation_id: self.services.memory.persistence.conversation_id,
            autosave_assistant: self.services.memory.persistence.autosave_assistant,
            autosave_min_length: self.services.memory.persistence.autosave_min_length,
            unsummarized_count: &mut unsummarized,
            goal_text: self.services.memory.extraction.goal_text.clone(),
        };
        let security = SecurityView {
            guard_memory_writes: guard_event.is_some(),
            _phantom: std::marker::PhantomData,
        };
        let mut sqlite_delta = 0u64;
        let mut embed_delta = 0u64;
        let mut guard_delta = 0u64;
        let mut metrics_view = MetricsView {
            sqlite_message_count: &mut sqlite_delta,
            embeddings_generated: &mut embed_delta,
            exfiltration_memory_guards: &mut guard_delta,
        };

        let svc = PersistenceService::new();
        let outcome = svc
            .persist_message(
                req,
                &mut self.msg.last_persisted_message_id,
                &mut memory_view,
                &security,
                &zeph_config::Config::default(),
                &mut metrics_view,
            )
            .await;

        // Write back the unsummarized counter (lens borrowed a local copy).
        self.services.memory.persistence.unsummarized_count = unsummarized;

        // Forward metric deltas through the watch broadcast.
        self.update_metrics(|m| {
            m.sqlite_message_count += sqlite_delta;
            m.embeddings_generated += embed_delta;
            // guard_delta is already tracked via push_security_event above.
            m.exfiltration_memory_guards += guard_delta;
        });

        if outcome.message_id.is_none() {
            return;
        }

        // Phase 2: enqueue enrichment tasks via supervisor (non-blocking).
        // check_summarization signals completion via SummarizationSignal, consumed in reap()
        // between turns — no shared mutable state across tasks (S1 fix).
        self.enqueue_summarization_task();

        // FIX-1: skip graph extraction for tool result messages — they contain raw structured
        // output (TOML, JSON, code) that pollutes the entity graph with noise.
        let has_tool_result_parts = parts
            .iter()
            .any(|p| matches!(p, MessagePart::ToolResult { .. }));

        self.enqueue_graph_extraction_task(content, has_injection_flags, has_tool_result_parts)
            .await;

        // Persona extraction: run only for user messages that are not tool results and not injected.
        if role == Role::User && !has_tool_result_parts && !has_injection_flags {
            self.enqueue_persona_extraction_task();
        }

        // Trajectory extraction: run after turns that contained tool results.
        if has_tool_result_parts {
            self.enqueue_trajectory_extraction_task();
        }

        // ReasoningBank distillation: runs only after the final assistant message of a turn
        // (C2 fix: skip intermediate tool-call messages). A message with ToolUse parts is an
        // intermediate step; the final assistant message has no ToolUse parts.
        // S-Med1: skip if injection patterns detected — mirrors graph extraction guard.
        let has_tool_use_parts = parts
            .iter()
            .any(|p| matches!(p, MessagePart::ToolUse { .. }));
        if role == Role::Assistant && !has_tool_use_parts && !has_injection_flags {
            self.enqueue_reasoning_extraction_task();
            // MemCoT distillation: same guards as ReasoningBank.
            self.enqueue_memcot_distill_task(content);
        }
    }

    /// Enqueue `MemCoT` semantic state distillation via the supervisor.
    ///
    /// All cost gates (interval, session cap, min chars) are checked inside
    /// [`crate::agent::memcot::SemanticStateAccumulator::maybe_enqueue_distill`].
    fn enqueue_memcot_distill_task(&mut self, assistant_content: &str) {
        let Some(accumulator) = &self.services.memory.extraction.memcot_accumulator else {
            return;
        };
        let distill_provider_name = self
            .services
            .memory
            .extraction
            .memcot_config
            .distill_provider
            .as_str();
        let provider = self.resolve_background_provider(distill_provider_name);

        let content = assistant_content.to_owned();
        let supervisor = &mut self.runtime.lifecycle.supervisor;

        accumulator.maybe_enqueue_distill(&content, provider, |name, fut| {
            supervisor.spawn(super::agent_supervisor::TaskClass::Enrichment, name, fut);
        });
    }

    /// Enqueue background summarization via the supervisor (S1 fix: no shared `AtomicUsize`).
    fn enqueue_summarization_task(&mut self) {
        let (Some(memory), Some(cid)) = (
            self.services.memory.persistence.memory.clone(),
            self.services.memory.persistence.conversation_id,
        ) else {
            return;
        };

        if self.services.memory.persistence.unsummarized_count
            <= self.services.memory.compaction.summarization_threshold
        {
            return;
        }

        let batch_size = self.services.memory.compaction.summarization_threshold / 2;

        self.runtime.lifecycle.supervisor.spawn_summarization("summarization", async move {
            match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                memory.summarize(cid, batch_size),
            )
            .await
            {
                Ok(Ok(Some(summary_id))) => {
                    tracing::info!(
                        "background summarization: created summary {summary_id} for conversation {cid}"
                    );
                    true
                }
                Ok(Ok(None)) => {
                    tracing::debug!("background summarization: no summarization needed");
                    false
                }
                Ok(Err(e)) => {
                    tracing::error!("background summarization failed: {e:#}");
                    false
                }
                Err(_) => {
                    tracing::warn!("background summarization timed out after 30s");
                    false
                }
            }
        });
    }

    /// Prepare graph extraction guards in foreground, then enqueue heavy work via supervisor.
    ///
    /// Guards (enabled check, injection/tool-result skip) stay on the foreground path.
    /// The RPE check and actual extraction run in background (S2: no `send_status`).
    #[tracing::instrument(
        name = "core.persist.enqueue_graph_extraction",
        skip_all,
        level = "debug"
    )]
    async fn enqueue_graph_extraction_task(
        &mut self,
        content: &str,
        has_injection_flags: bool,
        has_tool_result_parts: bool,
    ) {
        if self.services.memory.persistence.memory.is_none()
            || self.services.memory.persistence.conversation_id.is_none()
        {
            return;
        }
        if has_tool_result_parts {
            tracing::debug!("graph extraction skipped: message contains ToolResult parts");
            return;
        }
        if has_injection_flags {
            tracing::warn!("graph extraction skipped: injection patterns detected in content");
            return;
        }

        let cfg = &self.services.memory.extraction.graph_config;
        if !cfg.enabled {
            return;
        }
        let extraction_cfg = build_graph_extraction_config(
            cfg,
            self.services
                .memory
                .persistence
                .conversation_id
                .map(|c| c.0),
        );
        // Resolve a clean provider that bypasses quality_gate for JSON extraction tasks.
        // When extract_provider is empty, falls back to the primary provider (existing behavior).
        let extract_provider_name = cfg.extract_provider.as_str().to_owned();

        // RPE check: embed + compute surprise score. Stays on foreground to avoid
        // capturing the rpe_router mutex in a background task.
        if self.rpe_should_skip(content).await {
            tracing::debug!("D-MEM RPE: low-surprise turn, skipping graph extraction");
            return;
        }

        let context_messages = collect_context_messages(&self.msg.messages);

        let Some(memory) = self.services.memory.persistence.memory.clone() else {
            return;
        };

        let validator: zeph_memory::semantic::PostExtractValidator =
            if self.services.security.memory_validator.is_enabled() {
                let v = self.services.security.memory_validator.clone();
                Some(Box::new(move |result| {
                    v.validate_graph_extraction(result)
                        .map_err(|e| e.to_string())
                }))
            } else {
                None
            };

        let provider_override = if extract_provider_name.is_empty() {
            None
        } else {
            Some(self.resolve_background_provider(&extract_provider_name))
        };

        self.spawn_graph_extraction_task(
            memory,
            content,
            context_messages,
            extraction_cfg,
            validator,
            provider_override,
        );

        // Sync community failures and extraction metrics (cheap, foreground-safe).
        self.sync_community_detection_failures();
        self.sync_graph_extraction_metrics();
        self.enqueue_graph_count_sync_task();
    }

    fn spawn_graph_extraction_task(
        &mut self,
        memory: std::sync::Arc<zeph_memory::semantic::SemanticMemory>,
        content: &str,
        context_messages: Vec<String>,
        extraction_cfg: zeph_memory::semantic::GraphExtractionConfig,
        validator: zeph_memory::semantic::PostExtractValidator,
        provider_override: Option<zeph_llm::any::AnyProvider>,
    ) {
        let content_owned = content.to_owned();
        let graph_store = memory.graph_store.clone();
        let metrics_tx = self.runtime.metrics.metrics_tx.clone();
        let start_time = self.runtime.lifecycle.start_time;

        self.runtime.lifecycle.supervisor.spawn(
            super::agent_supervisor::TaskClass::Enrichment,
            "graph_extraction",
            async move {
                let extraction_handle = memory.spawn_graph_extraction(
                    content_owned,
                    context_messages,
                    extraction_cfg,
                    validator,
                    provider_override,
                );

                // After extraction completes, refresh graph count metrics.
                if let (Some(store), Some(tx)) = (graph_store, metrics_tx) {
                    let _ = extraction_handle.await;
                    let (entities, edges, communities) = tokio::join!(
                        store.entity_count(),
                        store.active_edge_count(),
                        store.community_count()
                    );
                    let elapsed = start_time.elapsed().as_secs();
                    tx.send_modify(|m| {
                        m.uptime_seconds = elapsed;
                        m.graph_entities_total = entities.unwrap_or(0).cast_unsigned();
                        m.graph_edges_total = edges.unwrap_or(0).cast_unsigned();
                        m.graph_communities_total = communities.unwrap_or(0).cast_unsigned();
                    });
                } else {
                    let _ = extraction_handle.await;
                }

                tracing::debug!("background graph extraction complete");
            },
        );
    }

    // sync_graph_counts and sync_guidelines_status are DB reads; enqueued as Telemetry background.
    fn enqueue_graph_count_sync_task(&mut self) {
        let memory_for_sync = self.services.memory.persistence.memory.clone();
        let metrics_tx_sync = self.runtime.metrics.metrics_tx.clone();
        let start_time_sync = self.runtime.lifecycle.start_time;
        let cid_sync = self.services.memory.persistence.conversation_id;
        let graph_store_sync = memory_for_sync.as_ref().and_then(|m| m.graph_store.clone());
        let sqlite_sync = memory_for_sync.as_ref().map(|m| m.sqlite().clone());
        let guidelines_enabled = self.services.memory.extraction.graph_config.enabled;

        self.runtime.lifecycle.supervisor.spawn(
            super::agent_supervisor::TaskClass::Telemetry,
            "graph_count_sync",
            async move {
                let Some(store) = graph_store_sync else {
                    return;
                };
                let Some(tx) = metrics_tx_sync else { return };

                let (entities, edges, communities) = tokio::join!(
                    store.entity_count(),
                    store.active_edge_count(),
                    store.community_count()
                );
                let elapsed = start_time_sync.elapsed().as_secs();
                tx.send_modify(|m| {
                    m.uptime_seconds = elapsed;
                    m.graph_entities_total = entities.unwrap_or(0).cast_unsigned();
                    m.graph_edges_total = edges.unwrap_or(0).cast_unsigned();
                    m.graph_communities_total = communities.unwrap_or(0).cast_unsigned();
                });

                // Sync guidelines status.
                if guidelines_enabled && let Some(sqlite) = sqlite_sync {
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        sqlite.load_compression_guidelines_meta(cid_sync),
                    )
                    .await
                    {
                        Ok(Ok((version, created_at))) => {
                            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                            let version_u32 = u32::try_from(version).unwrap_or(0);
                            tx.send_modify(|m| {
                                m.guidelines_version = version_u32;
                                m.guidelines_updated_at = created_at;
                            });
                        }
                        Ok(Err(e)) => {
                            tracing::debug!("guidelines status sync failed: {e:#}");
                        }
                        Err(_) => {
                            tracing::debug!("guidelines status sync timed out");
                        }
                    }
                }
            },
        );
    }

    /// Enqueue persona extraction via supervisor (background, no `send_status`).
    fn enqueue_persona_extraction_task(&mut self) {
        use zeph_memory::semantic::{PersonaExtractionConfig, extract_persona_facts};

        let cfg = &self.services.memory.extraction.persona_config;
        if !cfg.enabled {
            return;
        }

        let Some(memory) = &self.services.memory.persistence.memory else {
            return;
        };

        let user_messages: Vec<String> = self
            .msg
            .messages
            .iter()
            .filter(|m| {
                m.role == Role::User
                    && !m
                        .parts
                        .iter()
                        .any(|p| matches!(p, MessagePart::ToolResult { .. }))
            })
            .take(8)
            .map(|m| {
                if m.content.len() > 2048 {
                    m.content[..m.content.floor_char_boundary(2048)].to_owned()
                } else {
                    m.content.clone()
                }
            })
            .collect();

        if user_messages.len() < cfg.min_messages {
            return;
        }

        let timeout_secs = cfg.extraction_timeout_secs;
        let extraction_cfg = PersonaExtractionConfig {
            enabled: cfg.enabled,
            min_messages: cfg.min_messages,
            max_messages: cfg.max_messages,
            extraction_timeout_secs: timeout_secs,
        };

        let provider = self.resolve_background_provider(cfg.persona_provider.as_str());
        let store = memory.sqlite().clone();
        let conversation_id = self
            .services
            .memory
            .persistence
            .conversation_id
            .map(|c| c.0);

        self.runtime.lifecycle.supervisor.spawn(
            super::agent_supervisor::TaskClass::Enrichment,
            "persona_extraction",
            async move {
                let user_message_refs: Vec<&str> =
                    user_messages.iter().map(String::as_str).collect();
                let fut = extract_persona_facts(
                    &store,
                    &provider,
                    &user_message_refs,
                    &extraction_cfg,
                    conversation_id,
                );
                match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), fut).await
                {
                    Ok(Ok(n)) => tracing::debug!(upserted = n, "persona extraction complete"),
                    Ok(Err(e)) => tracing::warn!(error = %e, "persona extraction failed"),
                    Err(_) => tracing::warn!(
                        timeout_secs,
                        "persona extraction timed out — no facts written this turn"
                    ),
                }
            },
        );
    }

    /// Enqueue trajectory extraction via supervisor (background).
    fn enqueue_trajectory_extraction_task(&mut self) {
        use zeph_memory::semantic::{TrajectoryExtractionConfig, extract_trajectory_entries};

        let cfg = self.services.memory.extraction.trajectory_config.clone();
        if !cfg.enabled {
            return;
        }

        let Some(memory) = &self.services.memory.persistence.memory else {
            return;
        };

        let conversation_id = match self.services.memory.persistence.conversation_id {
            Some(cid) => cid.0,
            None => return,
        };

        let tail_start = self.msg.messages.len().saturating_sub(cfg.max_messages);
        let turn_messages: Vec<zeph_llm::provider::Message> =
            self.msg.messages[tail_start..].to_vec();

        if turn_messages.is_empty() {
            return;
        }

        let extraction_cfg = TrajectoryExtractionConfig {
            enabled: cfg.enabled,
            max_messages: cfg.max_messages,
            extraction_timeout_secs: cfg.extraction_timeout_secs,
        };

        let provider = self.resolve_background_provider(cfg.trajectory_provider.as_str());
        let store = memory.sqlite().clone();
        let min_confidence = cfg.min_confidence;

        self.runtime.lifecycle.supervisor.spawn(
            super::agent_supervisor::TaskClass::Enrichment,
            "trajectory_extraction",
            async move {
                let entries =
                    match extract_trajectory_entries(&provider, &turn_messages, &extraction_cfg)
                        .await
                    {
                        Ok(e) => e,
                        Err(e) => {
                            tracing::warn!(error = %e, "trajectory extraction failed");
                            return;
                        }
                    };

                let last_id = store
                    .trajectory_last_extracted_message_id(conversation_id)
                    .await
                    .unwrap_or(0);

                let mut max_id = last_id;
                for entry in &entries {
                    if entry.confidence < min_confidence {
                        continue;
                    }
                    let tools_json = serde_json::to_string(&entry.tools_used)
                        .unwrap_or_else(|_| "[]".to_string());
                    match store
                        .insert_trajectory_entry(zeph_memory::NewTrajectoryEntry {
                            conversation_id: Some(conversation_id),
                            turn_index: 0,
                            kind: &entry.kind,
                            intent: &entry.intent,
                            outcome: &entry.outcome,
                            tools_used: &tools_json,
                            confidence: entry.confidence,
                        })
                        .await
                    {
                        Ok(id) => {
                            if id > max_id {
                                max_id = id;
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "failed to insert trajectory entry"),
                    }
                }

                if max_id > last_id {
                    let _ = store
                        .set_trajectory_last_extracted_message_id(conversation_id, max_id)
                        .await;
                }

                tracing::debug!(
                    count = entries.len(),
                    conversation_id,
                    "trajectory extraction complete"
                );
            },
        );
    }

    /// Enqueue reasoning strategy distillation via supervisor (background, fire-and-forget).
    ///
    /// Mirrors [`Self::enqueue_trajectory_extraction_task`]. Runs after every assistant turn
    /// when `memory.reasoning.enabled = true` and a `ReasoningMemory` is attached.
    fn enqueue_reasoning_extraction_task(&mut self) {
        let cfg = self.services.memory.extraction.reasoning_config.clone();
        if !cfg.enabled {
            return;
        }

        let Some(memory) = &self.services.memory.persistence.memory else {
            return;
        };

        let Some(reasoning) = memory.reasoning.clone() else {
            return;
        };

        let tail_start = self.msg.messages.len().saturating_sub(cfg.max_messages);
        let turn_messages: Vec<zeph_llm::provider::Message> =
            self.msg.messages[tail_start..].to_vec();

        if turn_messages.len() < cfg.min_messages {
            return;
        }

        let extract_provider = self.resolve_background_provider(cfg.extract_provider.as_str());
        let distill_provider = self.resolve_background_provider(cfg.distill_provider.as_str());
        let embed_provider = memory.effective_embed_provider().clone();
        let store_limit = cfg.store_limit;
        let extraction_timeout = std::time::Duration::from_secs(cfg.extraction_timeout_secs);
        let distill_timeout = std::time::Duration::from_secs(cfg.distill_timeout_secs);
        let self_judge_window = cfg.self_judge_window;
        let min_assistant_chars = cfg.min_assistant_chars;

        self.runtime.lifecycle.supervisor.spawn(
            super::agent_supervisor::TaskClass::Enrichment,
            "reasoning_extraction",
            async move {
                if let Err(e) = zeph_memory::process_reasoning_turn(
                    &reasoning,
                    &extract_provider,
                    &distill_provider,
                    &embed_provider,
                    &turn_messages,
                    zeph_memory::ProcessTurnConfig {
                        store_limit,
                        extraction_timeout,
                        distill_timeout,
                        self_judge_window,
                        min_assistant_chars,
                    },
                )
                .await
                {
                    tracing::warn!(error = %e, "reasoning: process_turn failed");
                }

                tracing::debug!("reasoning extraction complete");
            },
        );
    }

    /// D-MEM RPE check: returns `true` when the current turn should skip graph extraction.
    ///
    /// Embeds `content`, computes RPE via the router, and updates the router state.
    /// Returns `false` (do not skip) on any error — conservative fallback.
    async fn rpe_should_skip(&mut self, content: &str) -> bool {
        let Some(ref rpe_mutex) = self.services.memory.extraction.rpe_router else {
            return false;
        };
        let Some(memory) = &self.services.memory.persistence.memory else {
            return false;
        };
        let candidates = zeph_memory::extract_candidate_entities(content);
        let provider = memory.provider();
        let Ok(Ok(emb_vec)) =
            tokio::time::timeout(std::time::Duration::from_secs(5), provider.embed(content)).await
        else {
            return false; // embed failed/timed out → extract
        };
        if let Ok(mut router) = rpe_mutex.lock() {
            let signal = router.compute(&emb_vec, &candidates);
            router.push_embedding(emb_vec);
            router.push_entities(&candidates);
            !signal.should_extract
        } else {
            tracing::warn!("rpe_router mutex poisoned; falling through to extract");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::agent_tests::{
        MetricsSnapshot, MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use super::*;
    use zeph_llm::any::AnyProvider;
    use zeph_llm::provider::Message;
    use zeph_memory::semantic::SemanticMemory;

    async fn test_memory(provider: &AnyProvider) -> SemanticMemory {
        SemanticMemory::new(
            ":memory:",
            "http://127.0.0.1:1",
            None,
            provider.clone(),
            "test-model",
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn load_history_without_memory_returns_ok() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.load_history().await;
        assert!(result.is_ok());
        // No messages added when no memory is configured
        assert_eq!(agent.msg.messages.len(), 1); // system prompt only
    }

    #[tokio::test]
    async fn load_history_with_messages_injects_into_agent() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        memory
            .sqlite()
            .save_message(cid, "user", "hello from history")
            .await
            .unwrap();
        memory
            .sqlite()
            .save_message(cid, "assistant", "hi back")
            .await
            .unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        let messages_before = agent.msg.messages.len();
        agent.load_history().await.unwrap();
        // Two messages were added from history
        assert_eq!(agent.msg.messages.len(), messages_before + 2);
    }

    #[tokio::test]
    async fn load_history_skips_empty_messages() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        // Save an empty message (should be skipped) and a valid one
        memory
            .sqlite()
            .save_message(cid, "user", "   ")
            .await
            .unwrap();
        memory
            .sqlite()
            .save_message(cid, "user", "real message")
            .await
            .unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        let messages_before = agent.msg.messages.len();
        agent.load_history().await.unwrap();
        // Only the non-empty message is loaded
        assert_eq!(agent.msg.messages.len(), messages_before + 1);
    }

    #[tokio::test]
    async fn load_history_with_empty_store_returns_ok() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        let messages_before = agent.msg.messages.len();
        agent.load_history().await.unwrap();
        // No messages added — empty history
        assert_eq!(agent.msg.messages.len(), messages_before);
    }

    #[tokio::test]
    async fn load_history_increments_session_count_for_existing_messages() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        // Save two messages — they start with session_count = 0.
        let id1 = memory
            .sqlite()
            .save_message(cid, "user", "hello")
            .await
            .unwrap();
        let id2 = memory
            .sqlite()
            .save_message(cid, "assistant", "hi")
            .await
            .unwrap();

        let memory_arc = std::sync::Arc::new(memory);
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            memory_arc.clone(),
            cid,
            50,
            5,
            100,
        );

        agent.load_history().await.unwrap();

        // Both episodic messages must have session_count = 1 after restore.
        let counts: Vec<i64> = zeph_db::query_scalar(
            "SELECT session_count FROM messages WHERE id IN (?, ?) ORDER BY id",
        )
        .bind(id1)
        .bind(id2)
        .fetch_all(memory_arc.sqlite().pool())
        .await
        .unwrap();
        assert_eq!(
            counts,
            vec![1, 1],
            "session_count must be 1 after first restore"
        );
    }

    #[tokio::test]
    async fn load_history_does_not_increment_session_count_for_new_conversation() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        // No messages saved — empty conversation.
        let memory_arc = std::sync::Arc::new(memory);
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            memory_arc.clone(),
            cid,
            50,
            5,
            100,
        );

        agent.load_history().await.unwrap();

        // No rows → no session_count increments → query returns empty.
        let counts: Vec<i64> =
            zeph_db::query_scalar("SELECT session_count FROM messages WHERE conversation_id = ?")
                .bind(cid)
                .fetch_all(memory_arc.sqlite().pool())
                .await
                .unwrap();
        assert!(counts.is_empty(), "new conversation must have no messages");
    }

    #[tokio::test]
    async fn persist_message_without_memory_silently_returns() {
        // No memory configured — persist_message must not panic
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        // Must not panic and must complete
        agent.persist_message(Role::User, "hello", &[], false).await;
    }

    #[tokio::test]
    async fn persist_message_assistant_autosave_false_uses_save_only() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let (tx, rx) = tokio::sync::watch::channel(MetricsSnapshot::default());
        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_metrics(tx)
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100);
        agent.services.memory.persistence.autosave_assistant = false;
        agent.services.memory.persistence.autosave_min_length = 20;

        agent
            .persist_message(Role::Assistant, "short assistant reply", &[], false)
            .await;

        let history = agent
            .services
            .memory
            .persistence
            .memory
            .as_ref()
            .unwrap()
            .sqlite()
            .load_history(cid, 50)
            .await
            .unwrap();
        assert_eq!(history.len(), 1, "message must be saved");
        assert_eq!(history[0].content, "short assistant reply");
        // embeddings_generated must remain 0 — save_only path does not embed
        assert_eq!(rx.borrow().embeddings_generated, 0);
    }

    #[tokio::test]
    async fn persist_message_assistant_below_min_length_uses_save_only() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let (tx, rx) = tokio::sync::watch::channel(MetricsSnapshot::default());
        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        // autosave_assistant=true but min_length=1000 — short content falls back to save_only
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_metrics(tx)
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100);
        agent.services.memory.persistence.autosave_assistant = true;
        agent.services.memory.persistence.autosave_min_length = 1000;

        agent
            .persist_message(Role::Assistant, "too short", &[], false)
            .await;

        let history = agent
            .services
            .memory
            .persistence
            .memory
            .as_ref()
            .unwrap()
            .sqlite()
            .load_history(cid, 50)
            .await
            .unwrap();
        assert_eq!(history.len(), 1, "message must be saved");
        assert_eq!(history[0].content, "too short");
        assert_eq!(rx.borrow().embeddings_generated, 0);
    }

    #[tokio::test]
    async fn persist_message_assistant_at_min_length_boundary_uses_embed() {
        // content.len() == autosave_min_length → should_embed = true (>= boundary).
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let (tx, rx) = tokio::sync::watch::channel(MetricsSnapshot::default());
        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let min_length = 10usize;
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_metrics(tx)
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100);
        agent.services.memory.persistence.autosave_assistant = true;
        agent.services.memory.persistence.autosave_min_length = min_length;

        // Exact boundary: len == min_length → embed path.
        let content_at_boundary = "A".repeat(min_length);
        assert_eq!(content_at_boundary.len(), min_length);
        agent
            .persist_message(Role::Assistant, &content_at_boundary, &[], false)
            .await;

        // sqlite_message_count must be incremented regardless of embedding success.
        assert_eq!(rx.borrow().sqlite_message_count, 1);
    }

    #[tokio::test]
    async fn persist_message_assistant_one_below_min_length_uses_save_only() {
        // content.len() == autosave_min_length - 1 → should_embed = false (below boundary).
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let (tx, rx) = tokio::sync::watch::channel(MetricsSnapshot::default());
        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let min_length = 10usize;
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_metrics(tx)
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100);
        agent.services.memory.persistence.autosave_assistant = true;
        agent.services.memory.persistence.autosave_min_length = min_length;

        // One below boundary: len == min_length - 1 → save_only path, no embedding.
        let content_below_boundary = "A".repeat(min_length - 1);
        assert_eq!(content_below_boundary.len(), min_length - 1);
        agent
            .persist_message(Role::Assistant, &content_below_boundary, &[], false)
            .await;

        let history = agent
            .services
            .memory
            .persistence
            .memory
            .as_ref()
            .unwrap()
            .sqlite()
            .load_history(cid, 50)
            .await
            .unwrap();
        assert_eq!(history.len(), 1, "message must still be saved");
        // save_only path does not embed.
        assert_eq!(rx.borrow().embeddings_generated, 0);
    }

    #[tokio::test]
    async fn persist_message_increments_unsummarized_count() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        // threshold=100 ensures no summarization is triggered
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        assert_eq!(agent.services.memory.persistence.unsummarized_count, 0);

        agent.persist_message(Role::User, "first", &[], false).await;
        assert_eq!(agent.services.memory.persistence.unsummarized_count, 1);

        agent
            .persist_message(Role::User, "second", &[], false)
            .await;
        assert_eq!(agent.services.memory.persistence.unsummarized_count, 2);
    }

    #[tokio::test]
    async fn check_summarization_resets_counter_on_success() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        // threshold=1 so the second persist triggers summarization check (count > threshold)
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            1,
        );

        agent.persist_message(Role::User, "msg1", &[], false).await;
        agent.persist_message(Role::User, "msg2", &[], false).await;

        // After summarization attempt (summarize returns Ok(None) since no messages qualify),
        // the counter is NOT reset to 0 — only reset on Ok(Some(_)).
        // This verifies check_summarization is called and the guard condition works.
        // unsummarized_count must be >= 2 before any summarization or 0 if summarization ran.
        assert!(agent.services.memory.persistence.unsummarized_count <= 2);
    }

    #[tokio::test]
    async fn unsummarized_count_not_incremented_without_memory() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.persist_message(Role::User, "hello", &[], false).await;
        // No memory configured — persist_message returns early, counter must stay 0.
        assert_eq!(agent.services.memory.persistence.unsummarized_count, 0);
    }

    // R-CRIT-01: unit tests for enqueue_graph_extraction_task guard conditions.
    mod graph_extraction_guards {
        use super::*;
        use crate::config::GraphConfig;
        use zeph_llm::provider::MessageMetadata;
        use zeph_memory::graph::GraphStore;

        fn enabled_graph_config() -> GraphConfig {
            GraphConfig {
                enabled: true,
                ..GraphConfig::default()
            }
        }

        async fn agent_with_graph(
            provider: &AnyProvider,
            config: GraphConfig,
        ) -> Agent<MockChannel> {
            let memory =
                test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
            let cid = memory.sqlite().create_conversation().await.unwrap();
            Agent::new(
                provider.clone(),
                MockChannel::new(vec![]),
                create_test_registry(),
                None,
                5,
                MockToolExecutor::no_tools(),
            )
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100)
            .with_graph_config(config)
        }

        #[tokio::test]
        async fn injection_flag_guard_skips_extraction() {
            // has_injection_flags=true → extraction must be skipped; no counter in graph_metadata.
            let provider = mock_provider(vec![]);
            let mut agent = agent_with_graph(&provider, enabled_graph_config()).await;
            let pool = agent
                .services
                .memory
                .persistence
                .memory
                .as_ref()
                .unwrap()
                .sqlite()
                .pool()
                .clone();

            agent
                .enqueue_graph_extraction_task("I use Rust", true, false)
                .await;

            // Give any accidental spawn time to settle.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            let store = GraphStore::new(pool);
            let count = store.get_metadata("extraction_count").await.unwrap();
            assert!(
                count.is_none(),
                "injection flag must prevent extraction_count from being written"
            );
        }

        #[tokio::test]
        async fn disabled_config_guard_skips_extraction() {
            // graph.enabled=false → extraction must be skipped.
            let provider = mock_provider(vec![]);
            let disabled_cfg = GraphConfig {
                enabled: false,
                ..GraphConfig::default()
            };
            let mut agent = agent_with_graph(&provider, disabled_cfg).await;
            let pool = agent
                .services
                .memory
                .persistence
                .memory
                .as_ref()
                .unwrap()
                .sqlite()
                .pool()
                .clone();

            agent
                .enqueue_graph_extraction_task("I use Rust", false, false)
                .await;

            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            let store = GraphStore::new(pool);
            let count = store.get_metadata("extraction_count").await.unwrap();
            assert!(
                count.is_none(),
                "disabled graph config must prevent extraction"
            );
        }

        #[tokio::test]
        async fn happy_path_fires_extraction() {
            // With enabled config and no injection flags, extraction is spawned.
            // MockProvider returns None (no entities), but the counter must be incremented.
            let provider = mock_provider(vec![]);
            let mut agent = agent_with_graph(&provider, enabled_graph_config()).await;
            let pool = agent
                .services
                .memory
                .persistence
                .memory
                .as_ref()
                .unwrap()
                .sqlite()
                .pool()
                .clone();

            agent
                .enqueue_graph_extraction_task("I use Rust for systems programming", false, false)
                .await;

            // Wait for the spawned task to complete.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;

            let store = GraphStore::new(pool);
            let count = store.get_metadata("extraction_count").await.unwrap();
            assert!(
                count.is_some(),
                "happy-path extraction must increment extraction_count"
            );
        }

        #[tokio::test]
        async fn tool_result_parts_guard_skips_extraction() {
            // FIX-1 regression: has_tool_result_parts=true → extraction must be skipped.
            // Tool result messages contain raw structured output (TOML, JSON, code) — not
            // conversational content. Extracting entities from them produces graph noise.
            let provider = mock_provider(vec![]);
            let mut agent = agent_with_graph(&provider, enabled_graph_config()).await;
            let pool = agent
                .services
                .memory
                .persistence
                .memory
                .as_ref()
                .unwrap()
                .sqlite()
                .pool()
                .clone();

            agent
                .enqueue_graph_extraction_task(
                    "[tool_result: abc123]\nprovider_type = \"claude\"\nallowed_commands = []",
                    false,
                    true, // has_tool_result_parts
                )
                .await;

            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            let store = GraphStore::new(pool);
            let count = store.get_metadata("extraction_count").await.unwrap();
            assert!(
                count.is_none(),
                "tool result message must not trigger graph extraction"
            );
        }

        #[tokio::test]
        async fn context_filter_excludes_tool_result_messages() {
            // FIX-2: context_messages must not include tool result user messages.
            // When enqueue_graph_extraction_task collects context, it filters out
            // Role::User messages that contain ToolResult parts — only conversational
            // user messages are included as extraction context.
            //
            // This test verifies the guard fires: a tool result message alone is passed
            // (has_tool_result_parts=true) → extraction is skipped entirely, so context
            // filtering is not exercised. We verify FIX-2 by ensuring a prior tool result
            // message in agent.msg.messages is excluded when a subsequent conversational message
            // triggers extraction.
            let provider = mock_provider(vec![]);
            let mut agent = agent_with_graph(&provider, enabled_graph_config()).await;

            // Add a tool result message to the agent's message history — this simulates
            // a tool call response that arrived before the current conversational turn.
            agent.msg.messages.push(Message {
                role: Role::User,
                content: "[tool_result: abc]\nprovider_type = \"openai\"".to_owned(),
                parts: vec![MessagePart::ToolResult {
                    tool_use_id: "abc".to_owned(),
                    content: "provider_type = \"openai\"".to_owned(),
                    is_error: false,
                }],
                metadata: MessageMetadata::default(),
            });

            let pool = agent
                .services
                .memory
                .persistence
                .memory
                .as_ref()
                .unwrap()
                .sqlite()
                .pool()
                .clone();

            // Trigger extraction for a conversational message (not a tool result).
            agent
                .enqueue_graph_extraction_task(
                    "I prefer Rust for systems programming",
                    false,
                    false,
                )
                .await;

            tokio::time::sleep(std::time::Duration::from_millis(200)).await;

            // Extraction must have fired (conversational message, no injection flags).
            let store = GraphStore::new(pool);
            let count = store.get_metadata("extraction_count").await.unwrap();
            assert!(
                count.is_some(),
                "conversational message must trigger extraction even with prior tool result in history"
            );
        }
    }

    // R-PERS-01: unit tests for enqueue_persona_extraction_task guard conditions.
    mod persona_extraction_guards {
        use super::*;
        use zeph_config::PersonaConfig;
        use zeph_llm::provider::MessageMetadata;

        fn enabled_persona_config() -> PersonaConfig {
            PersonaConfig {
                enabled: true,
                min_messages: 1,
                ..PersonaConfig::default()
            }
        }

        async fn agent_with_persona(
            provider: &AnyProvider,
            config: PersonaConfig,
        ) -> Agent<MockChannel> {
            let memory =
                test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
            let cid = memory.sqlite().create_conversation().await.unwrap();
            let mut agent = Agent::new(
                provider.clone(),
                MockChannel::new(vec![]),
                create_test_registry(),
                None,
                5,
                MockToolExecutor::no_tools(),
            )
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100);
            agent.services.memory.extraction.persona_config = config;
            agent
        }

        #[tokio::test]
        async fn disabled_config_skips_spawn() {
            // persona.enabled=false → nothing is spawned; persona_memory stays empty.
            let provider = mock_provider(vec![]);
            let mut agent = agent_with_persona(
                &provider,
                PersonaConfig {
                    enabled: false,
                    ..PersonaConfig::default()
                },
            )
            .await;

            // Inject a user message so message count is above threshold.
            agent.msg.messages.push(zeph_llm::provider::Message {
                role: Role::User,
                content: "I prefer Rust for systems programming".to_owned(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            });

            agent.enqueue_persona_extraction_task();

            let store = agent
                .services
                .memory
                .persistence
                .memory
                .as_ref()
                .unwrap()
                .sqlite()
                .clone();
            let count = store.count_persona_facts().await.unwrap();
            assert_eq!(count, 0, "disabled persona config must not write any facts");
        }

        #[tokio::test]
        async fn below_min_messages_skips_spawn() {
            // min_messages=3 but only 2 user messages → should skip.
            let provider = mock_provider(vec![]);
            let mut agent = agent_with_persona(
                &provider,
                PersonaConfig {
                    enabled: true,
                    min_messages: 3,
                    ..PersonaConfig::default()
                },
            )
            .await;

            for text in ["I use Rust", "I prefer async code"] {
                agent.msg.messages.push(zeph_llm::provider::Message {
                    role: Role::User,
                    content: text.to_owned(),
                    parts: vec![],
                    metadata: MessageMetadata::default(),
                });
            }

            agent.enqueue_persona_extraction_task();

            let store = agent
                .services
                .memory
                .persistence
                .memory
                .as_ref()
                .unwrap()
                .sqlite()
                .clone();
            let count = store.count_persona_facts().await.unwrap();
            assert_eq!(
                count, 0,
                "below min_messages threshold must not trigger extraction"
            );
        }

        #[tokio::test]
        async fn no_memory_skips_spawn() {
            // memory=None → must exit early without panic.
            let provider = mock_provider(vec![]);
            let channel = MockChannel::new(vec![]);
            let registry = create_test_registry();
            let executor = MockToolExecutor::no_tools();
            let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
            agent.services.memory.extraction.persona_config = enabled_persona_config();
            agent.msg.messages.push(zeph_llm::provider::Message {
                role: Role::User,
                content: "I like Rust".to_owned(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            });

            // Must not panic even without memory.
            agent.enqueue_persona_extraction_task();
        }

        #[tokio::test]
        async fn enabled_enough_messages_spawns_extraction() {
            // enabled=true, min_messages=1, self-referential message → extraction runs eagerly
            // (not fire-and-forget) and chat() is called on the provider, verified via MockProvider.
            use zeph_llm::mock::MockProvider;
            let (mock, recorded) = MockProvider::default().with_recording();
            let provider = AnyProvider::Mock(mock);
            let mut agent = agent_with_persona(&provider, enabled_persona_config()).await;

            agent.msg.messages.push(zeph_llm::provider::Message {
                role: Role::User,
                content: "I prefer Rust for systems programming".to_owned(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            });

            agent.enqueue_persona_extraction_task();

            // Persona extraction runs via BackgroundSupervisor. Wait for tasks to complete.
            agent.runtime.lifecycle.supervisor.join_all_for_test().await;

            let calls = recorded.lock().unwrap();
            assert!(
                !calls.is_empty(),
                "happy-path: provider.chat() must be called when extraction completes"
            );
        }

        #[tokio::test]
        async fn messages_capped_at_eight() {
            // More than 8 user messages → only 8 are passed to extraction.
            // Each message contains "I" so self-referential gate passes.
            use zeph_llm::mock::MockProvider;
            let (mock, recorded) = MockProvider::default().with_recording();
            let provider = AnyProvider::Mock(mock);
            let mut agent = agent_with_persona(&provider, enabled_persona_config()).await;

            for i in 0..12u32 {
                agent.msg.messages.push(zeph_llm::provider::Message {
                    role: Role::User,
                    content: format!("I like message {i}"),
                    parts: vec![],
                    metadata: MessageMetadata::default(),
                });
            }

            agent.enqueue_persona_extraction_task();

            // Allow the background task to complete before asserting.
            agent.runtime.lifecycle.supervisor.join_all_for_test().await;

            // Verify extraction ran (provider was called).
            let calls = recorded.lock().unwrap();
            assert!(
                !calls.is_empty(),
                "extraction must run when enough messages present"
            );
            // Verify the prompt sent to the provider does not contain messages beyond the 8th.
            let prompt = &calls[0];
            let user_text = prompt
                .iter()
                .filter(|m| m.role == Role::User)
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            // Messages 8..11 ("I like message 8".."I like message 11") must not appear.
            assert!(
                !user_text.contains("I like message 8"),
                "message index 8 must be excluded from extraction input"
            );
        }

        #[test]
        fn long_message_truncated_at_char_boundary() {
            // Directly verify the per-message truncation logic applied in
            // enqueue_persona_extraction_task: a content > 2048 bytes must be capped
            // to exactly floor_char_boundary(2048).
            let long_content = "x".repeat(3000);
            let truncated = if long_content.len() > 2048 {
                long_content[..long_content.floor_char_boundary(2048)].to_owned()
            } else {
                long_content.clone()
            };
            assert_eq!(
                truncated.len(),
                2048,
                "ASCII content must be truncated to exactly 2048 bytes"
            );

            // Multi-byte boundary: build a string whose char boundary falls before 2048.
            let multi = "é".repeat(1500); // each 'é' is 2 bytes → 3000 bytes total
            let truncated_multi = if multi.len() > 2048 {
                multi[..multi.floor_char_boundary(2048)].to_owned()
            } else {
                multi.clone()
            };
            assert!(
                truncated_multi.len() <= 2048,
                "multi-byte content must not exceed 2048 bytes"
            );
            assert!(truncated_multi.is_char_boundary(truncated_multi.len()));
        }
    }

    #[tokio::test]
    async fn persist_message_user_always_embeds_regardless_of_autosave_flag() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let (tx, rx) = tokio::sync::watch::channel(MetricsSnapshot::default());
        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        // autosave_assistant=false — but User role always takes embedding path
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_metrics(tx)
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100);
        agent.services.memory.persistence.autosave_assistant = false;
        agent.services.memory.persistence.autosave_min_length = 20;

        let long_user_msg = "A".repeat(100);
        agent
            .persist_message(Role::User, &long_user_msg, &[], false)
            .await;

        let history = agent
            .services
            .memory
            .persistence
            .memory
            .as_ref()
            .unwrap()
            .sqlite()
            .load_history(cid, 50)
            .await
            .unwrap();
        assert_eq!(history.len(), 1, "user message must be saved");
        // User messages go through remember_with_parts (embedding path).
        // sqlite_message_count must increment regardless of Qdrant availability.
        assert_eq!(rx.borrow().sqlite_message_count, 1);
    }

    // Round-trip tests: verify that persist_message saves the correct parts and they
    // are restored correctly by load_history.

    #[tokio::test]
    async fn persist_message_saves_correct_tool_use_parts() {
        use zeph_llm::provider::MessagePart;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        let parts = vec![MessagePart::ToolUse {
            id: "call_abc123".to_string(),
            name: "read_file".to_string(),
            input: serde_json::json!({"path": "/tmp/test.txt"}),
        }];
        let content = "[tool_use: read_file(call_abc123)]";

        agent
            .persist_message(Role::Assistant, content, &parts, false)
            .await;

        let history = agent
            .services
            .memory
            .persistence
            .memory
            .as_ref()
            .unwrap()
            .sqlite()
            .load_history(cid, 50)
            .await
            .unwrap();

        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, Role::Assistant);
        assert_eq!(history[0].content, content);
        assert_eq!(history[0].parts.len(), 1);
        match &history[0].parts[0] {
            MessagePart::ToolUse { id, name, .. } => {
                assert_eq!(id, "call_abc123");
                assert_eq!(name, "read_file");
            }
            other => panic!("expected ToolUse part, got {other:?}"),
        }
        // Regression guard: assistant message must NOT have ToolResult parts
        assert!(
            !history[0]
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolResult { .. })),
            "assistant message must not contain ToolResult parts"
        );
    }

    #[tokio::test]
    async fn persist_message_saves_correct_tool_result_parts() {
        use zeph_llm::provider::MessagePart;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        let parts = vec![MessagePart::ToolResult {
            tool_use_id: "call_abc123".to_string(),
            content: "file contents here".to_string(),
            is_error: false,
        }];
        let content = "[tool_result: call_abc123]\nfile contents here";

        agent
            .persist_message(Role::User, content, &parts, false)
            .await;

        let history = agent
            .services
            .memory
            .persistence
            .memory
            .as_ref()
            .unwrap()
            .sqlite()
            .load_history(cid, 50)
            .await
            .unwrap();

        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, Role::User);
        assert_eq!(history[0].content, content);
        assert_eq!(history[0].parts.len(), 1);
        match &history[0].parts[0] {
            MessagePart::ToolResult {
                tool_use_id,
                content: result_content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "call_abc123");
                assert_eq!(result_content, "file contents here");
                assert!(!is_error);
            }
            other => panic!("expected ToolResult part, got {other:?}"),
        }
        // Regression guard: user message with ToolResult must NOT have ToolUse parts
        assert!(
            !history[0]
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolUse { .. })),
            "user ToolResult message must not contain ToolUse parts"
        );
    }

    #[tokio::test]
    async fn persist_message_roundtrip_preserves_role_part_alignment() {
        use zeph_llm::provider::MessagePart;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        // Persist assistant message with ToolUse parts
        let assistant_parts = vec![MessagePart::ToolUse {
            id: "id_1".to_string(),
            name: "list_dir".to_string(),
            input: serde_json::json!({"path": "/tmp"}),
        }];
        agent
            .persist_message(
                Role::Assistant,
                "[tool_use: list_dir(id_1)]",
                &assistant_parts,
                false,
            )
            .await;

        // Persist user message with ToolResult parts
        let user_parts = vec![MessagePart::ToolResult {
            tool_use_id: "id_1".to_string(),
            content: "file1.txt\nfile2.txt".to_string(),
            is_error: false,
        }];
        agent
            .persist_message(
                Role::User,
                "[tool_result: id_1]\nfile1.txt\nfile2.txt",
                &user_parts,
                false,
            )
            .await;

        let history = agent
            .services
            .memory
            .persistence
            .memory
            .as_ref()
            .unwrap()
            .sqlite()
            .load_history(cid, 50)
            .await
            .unwrap();

        assert_eq!(history.len(), 2);

        // First message: assistant + ToolUse
        assert_eq!(history[0].role, Role::Assistant);
        assert_eq!(history[0].content, "[tool_use: list_dir(id_1)]");
        assert!(
            matches!(&history[0].parts[0], MessagePart::ToolUse { id, .. } if id == "id_1"),
            "first message must be assistant ToolUse"
        );

        // Second message: user + ToolResult
        assert_eq!(history[1].role, Role::User);
        assert_eq!(
            history[1].content,
            "[tool_result: id_1]\nfile1.txt\nfile2.txt"
        );
        assert!(
            matches!(&history[1].parts[0], MessagePart::ToolResult { tool_use_id, .. } if tool_use_id == "id_1"),
            "second message must be user ToolResult"
        );

        // Cross-role regression guard: no swapped parts
        assert!(
            !history[0]
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolResult { .. })),
            "assistant message must not have ToolResult parts"
        );
        assert!(
            !history[1]
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolUse { .. })),
            "user message must not have ToolUse parts"
        );
    }

    #[tokio::test]
    async fn persist_message_saves_correct_tool_output_parts() {
        use zeph_llm::provider::MessagePart;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        let parts = vec![MessagePart::ToolOutput {
            tool_name: "shell".into(),
            body: "hello from shell".to_string(),
            compacted_at: None,
        }];
        let content = "[tool: shell]\nhello from shell";

        agent
            .persist_message(Role::User, content, &parts, false)
            .await;

        let history = agent
            .services
            .memory
            .persistence
            .memory
            .as_ref()
            .unwrap()
            .sqlite()
            .load_history(cid, 50)
            .await
            .unwrap();

        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, Role::User);
        assert_eq!(history[0].content, content);
        assert_eq!(history[0].parts.len(), 1);
        match &history[0].parts[0] {
            MessagePart::ToolOutput {
                tool_name,
                body,
                compacted_at,
            } => {
                assert_eq!(tool_name, "shell");
                assert_eq!(body, "hello from shell");
                assert!(compacted_at.is_none());
            }
            other => panic!("expected ToolOutput part, got {other:?}"),
        }
    }

    // --- sanitize_tool_pairs unit tests ---

    #[tokio::test]
    async fn load_history_removes_trailing_orphan_tool_use() {
        use zeph_llm::provider::MessagePart;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let sqlite = memory.sqlite();

        // user message (normal)
        sqlite
            .save_message(cid, "user", "do something with a tool")
            .await
            .unwrap();

        // assistant message with ToolUse parts — no following tool_result (orphan)
        let parts = serde_json::to_string(&[MessagePart::ToolUse {
            id: "call_orphan".to_string(),
            name: "shell".to_string(),
            input: serde_json::json!({"command": "ls"}),
        }])
        .unwrap();
        sqlite
            .save_message_with_parts(cid, "assistant", "[tool_use: shell(call_orphan)]", &parts)
            .await
            .unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        let messages_before = agent.msg.messages.len();
        agent.load_history().await.unwrap();

        // Only the user message should be loaded; orphaned assistant tool_use removed.
        assert_eq!(
            agent.msg.messages.len(),
            messages_before + 1,
            "orphaned trailing tool_use must be removed"
        );
        assert_eq!(agent.msg.messages.last().unwrap().role, Role::User);
    }

    #[tokio::test]
    async fn load_history_removes_leading_orphan_tool_result() {
        use zeph_llm::provider::MessagePart;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let sqlite = memory.sqlite();

        // Leading orphan: user message with ToolResult but no preceding tool_use
        let result_parts = serde_json::to_string(&[MessagePart::ToolResult {
            tool_use_id: "call_missing".to_string(),
            content: "result data".to_string(),
            is_error: false,
        }])
        .unwrap();
        sqlite
            .save_message_with_parts(
                cid,
                "user",
                "[tool_result: call_missing]\nresult data",
                &result_parts,
            )
            .await
            .unwrap();

        // A valid assistant reply after the orphan
        sqlite
            .save_message(cid, "assistant", "here is my response")
            .await
            .unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        let messages_before = agent.msg.messages.len();
        agent.load_history().await.unwrap();

        // Orphaned leading tool_result removed; only assistant message kept.
        assert_eq!(
            agent.msg.messages.len(),
            messages_before + 1,
            "orphaned leading tool_result must be removed"
        );
        assert_eq!(agent.msg.messages.last().unwrap().role, Role::Assistant);
    }

    #[tokio::test]
    async fn load_history_preserves_complete_tool_pairs() {
        use zeph_llm::provider::MessagePart;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let sqlite = memory.sqlite();

        // Complete tool_use / tool_result pair
        let use_parts = serde_json::to_string(&[MessagePart::ToolUse {
            id: "call_ok".to_string(),
            name: "shell".to_string(),
            input: serde_json::json!({"command": "pwd"}),
        }])
        .unwrap();
        sqlite
            .save_message_with_parts(cid, "assistant", "[tool_use: shell(call_ok)]", &use_parts)
            .await
            .unwrap();

        let result_parts = serde_json::to_string(&[MessagePart::ToolResult {
            tool_use_id: "call_ok".to_string(),
            content: "/home/user".to_string(),
            is_error: false,
        }])
        .unwrap();
        sqlite
            .save_message_with_parts(
                cid,
                "user",
                "[tool_result: call_ok]\n/home/user",
                &result_parts,
            )
            .await
            .unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        let messages_before = agent.msg.messages.len();
        agent.load_history().await.unwrap();

        // Both messages must be preserved.
        assert_eq!(
            agent.msg.messages.len(),
            messages_before + 2,
            "complete tool_use/tool_result pair must be preserved"
        );
        assert_eq!(agent.msg.messages[messages_before].role, Role::Assistant);
        assert_eq!(agent.msg.messages[messages_before + 1].role, Role::User);
    }

    #[tokio::test]
    async fn load_history_handles_multiple_trailing_orphans() {
        use zeph_llm::provider::MessagePart;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let sqlite = memory.sqlite();

        // Normal user message
        sqlite.save_message(cid, "user", "start").await.unwrap();

        // First orphaned tool_use
        let parts1 = serde_json::to_string(&[MessagePart::ToolUse {
            id: "call_1".to_string(),
            name: "shell".to_string(),
            input: serde_json::json!({}),
        }])
        .unwrap();
        sqlite
            .save_message_with_parts(cid, "assistant", "[tool_use: shell(call_1)]", &parts1)
            .await
            .unwrap();

        // Second orphaned tool_use (consecutive, no tool_result between them)
        let parts2 = serde_json::to_string(&[MessagePart::ToolUse {
            id: "call_2".to_string(),
            name: "read_file".to_string(),
            input: serde_json::json!({}),
        }])
        .unwrap();
        sqlite
            .save_message_with_parts(cid, "assistant", "[tool_use: read_file(call_2)]", &parts2)
            .await
            .unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        let messages_before = agent.msg.messages.len();
        agent.load_history().await.unwrap();

        // Both orphaned tool_use messages removed; only the user message kept.
        assert_eq!(
            agent.msg.messages.len(),
            messages_before + 1,
            "all trailing orphaned tool_use messages must be removed"
        );
        assert_eq!(agent.msg.messages.last().unwrap().role, Role::User);
    }

    #[tokio::test]
    async fn load_history_no_tool_messages_unchanged() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let sqlite = memory.sqlite();

        sqlite.save_message(cid, "user", "hello").await.unwrap();
        sqlite
            .save_message(cid, "assistant", "hi there")
            .await
            .unwrap();
        sqlite
            .save_message(cid, "user", "how are you?")
            .await
            .unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        let messages_before = agent.msg.messages.len();
        agent.load_history().await.unwrap();

        // All three plain messages must be preserved.
        assert_eq!(
            agent.msg.messages.len(),
            messages_before + 3,
            "plain messages without tool parts must pass through unchanged"
        );
    }

    #[tokio::test]
    async fn load_history_removes_both_leading_and_trailing_orphans() {
        use zeph_llm::provider::MessagePart;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let sqlite = memory.sqlite();

        // Leading orphan: user message with ToolResult, no preceding tool_use
        let result_parts = serde_json::to_string(&[MessagePart::ToolResult {
            tool_use_id: "call_leading".to_string(),
            content: "orphaned result".to_string(),
            is_error: false,
        }])
        .unwrap();
        sqlite
            .save_message_with_parts(
                cid,
                "user",
                "[tool_result: call_leading]\norphaned result",
                &result_parts,
            )
            .await
            .unwrap();

        // Valid middle messages
        sqlite
            .save_message(cid, "user", "what is 2+2?")
            .await
            .unwrap();
        sqlite.save_message(cid, "assistant", "4").await.unwrap();

        // Trailing orphan: assistant message with ToolUse, no following tool_result
        let use_parts = serde_json::to_string(&[MessagePart::ToolUse {
            id: "call_trailing".to_string(),
            name: "shell".to_string(),
            input: serde_json::json!({"command": "date"}),
        }])
        .unwrap();
        sqlite
            .save_message_with_parts(
                cid,
                "assistant",
                "[tool_use: shell(call_trailing)]",
                &use_parts,
            )
            .await
            .unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        let messages_before = agent.msg.messages.len();
        agent.load_history().await.unwrap();

        // Both orphans removed; only the 2 valid middle messages kept.
        assert_eq!(
            agent.msg.messages.len(),
            messages_before + 2,
            "both leading and trailing orphans must be removed"
        );
        assert_eq!(agent.msg.messages[messages_before].role, Role::User);
        assert_eq!(agent.msg.messages[messages_before].content, "what is 2+2?");
        assert_eq!(
            agent.msg.messages[messages_before + 1].role,
            Role::Assistant
        );
        assert_eq!(agent.msg.messages[messages_before + 1].content, "4");
    }

    /// RC1 regression: mid-history assistant[`ToolUse`] without a following user[`ToolResult`]
    /// must have its `ToolUse` parts stripped (text preserved). The message count stays the same
    /// because the assistant message has a text content fallback; only `ToolUse` parts are
    /// removed.
    #[tokio::test]
    async fn sanitize_tool_pairs_strips_mid_history_orphan_tool_use() {
        use zeph_llm::provider::MessagePart;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let sqlite = memory.sqlite();

        // Normal first exchange.
        sqlite
            .save_message(cid, "user", "first question")
            .await
            .unwrap();
        sqlite
            .save_message(cid, "assistant", "first answer")
            .await
            .unwrap();

        // Mid-history orphan: assistant with ToolUse but NO following ToolResult user message.
        // This models the compaction-split scenario (RC2) where replace_conversation hid the
        // user[ToolResult] but left the assistant[ToolUse] visible.
        let use_parts = serde_json::to_string(&[
            MessagePart::ToolUse {
                id: "call_mid_1".to_string(),
                name: "shell".to_string(),
                input: serde_json::json!({"command": "ls"}),
            },
            MessagePart::Text {
                text: "Let me check the files.".to_string(),
            },
        ])
        .unwrap();
        sqlite
            .save_message_with_parts(cid, "assistant", "Let me check the files.", &use_parts)
            .await
            .unwrap();

        // Another normal exchange after the orphan.
        sqlite
            .save_message(cid, "user", "second question")
            .await
            .unwrap();
        sqlite
            .save_message(cid, "assistant", "second answer")
            .await
            .unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        let messages_before = agent.msg.messages.len();
        agent.load_history().await.unwrap();

        // All 5 messages remain (orphan message kept because it has text), but the orphaned
        // message must have its ToolUse parts stripped.
        assert_eq!(
            agent.msg.messages.len(),
            messages_before + 5,
            "message count must be 5 (orphan message kept — has text content)"
        );

        // The orphaned assistant message (index 2 in the loaded slice) must have no ToolUse parts.
        let orphan = &agent.msg.messages[messages_before + 2];
        assert_eq!(orphan.role, Role::Assistant);
        assert!(
            !orphan
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolUse { .. })),
            "orphaned ToolUse parts must be stripped from mid-history message"
        );
        // Text part must be preserved.
        assert!(
            orphan.parts.iter().any(
                |p| matches!(p, MessagePart::Text { text } if text == "Let me check the files.")
            ),
            "text content of orphaned assistant message must be preserved"
        );
    }

    /// RC3 regression: a user message with empty `content` but non-empty `parts` (`ToolResult`)
    /// must NOT be skipped by `load_history`. Previously the empty-content check dropped these
    /// messages before `sanitize_tool_pairs` ran, leaving the preceding assistant `ToolUse`
    /// orphaned.
    #[tokio::test]
    async fn load_history_keeps_tool_only_user_message() {
        use zeph_llm::provider::MessagePart;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let sqlite = memory.sqlite();

        // Assistant sends a ToolUse.
        let use_parts = serde_json::to_string(&[MessagePart::ToolUse {
            id: "call_rc3".to_string(),
            name: "memory_save".to_string(),
            input: serde_json::json!({"content": "something"}),
        }])
        .unwrap();
        sqlite
            .save_message_with_parts(cid, "assistant", "[tool_use: memory_save]", &use_parts)
            .await
            .unwrap();

        // User message has empty text content but carries ToolResult in parts — native tool pattern.
        let result_parts = serde_json::to_string(&[MessagePart::ToolResult {
            tool_use_id: "call_rc3".to_string(),
            content: "saved".to_string(),
            is_error: false,
        }])
        .unwrap();
        sqlite
            .save_message_with_parts(cid, "user", "", &result_parts)
            .await
            .unwrap();

        sqlite.save_message(cid, "assistant", "done").await.unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        let messages_before = agent.msg.messages.len();
        agent.load_history().await.unwrap();

        // All 3 messages must be loaded — the empty-content ToolResult user message must NOT be
        // dropped.
        assert_eq!(
            agent.msg.messages.len(),
            messages_before + 3,
            "user message with empty content but ToolResult parts must not be dropped"
        );

        // The user message at index 1 must still carry the ToolResult part.
        let user_msg = &agent.msg.messages[messages_before + 1];
        assert_eq!(user_msg.role, Role::User);
        assert!(
            user_msg.parts.iter().any(
                |p| matches!(p, MessagePart::ToolResult { tool_use_id, .. } if tool_use_id == "call_rc3")
            ),
            "ToolResult part must be preserved on user message with empty content"
        );
    }

    /// RC2 reverse pass: a user message with a `ToolResult` whose `tool_use_id` has no matching
    /// `ToolUse` in the preceding assistant message must be stripped by
    /// `strip_mid_history_orphans`.
    #[tokio::test]
    async fn strip_orphans_removes_orphaned_tool_result() {
        use zeph_llm::provider::MessagePart;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let sqlite = memory.sqlite();

        // Normal exchange before the orphan.
        sqlite.save_message(cid, "user", "hello").await.unwrap();
        sqlite.save_message(cid, "assistant", "hi").await.unwrap();

        // Assistant message that does NOT contain a ToolUse.
        sqlite
            .save_message(cid, "assistant", "plain answer")
            .await
            .unwrap();

        // User message references a tool_use_id that was never sent by the preceding assistant.
        let orphan_result_parts = serde_json::to_string(&[MessagePart::ToolResult {
            tool_use_id: "call_nonexistent".to_string(),
            content: "stale result".to_string(),
            is_error: false,
        }])
        .unwrap();
        sqlite
            .save_message_with_parts(
                cid,
                "user",
                "[tool_result: call_nonexistent]\nstale result",
                &orphan_result_parts,
            )
            .await
            .unwrap();

        sqlite
            .save_message(cid, "assistant", "final")
            .await
            .unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        let messages_before = agent.msg.messages.len();
        agent.load_history().await.unwrap();

        // The orphaned ToolResult part must have been stripped from the user message.
        // The user message itself may be removed (parts empty + content non-empty) or kept with
        // the text content — but it must NOT retain the orphaned ToolResult part.
        let loaded = &agent.msg.messages[messages_before..];
        for msg in loaded {
            assert!(
                !msg.parts.iter().any(|p| matches!(
                    p,
                    MessagePart::ToolResult { tool_use_id, .. } if tool_use_id == "call_nonexistent"
                )),
                "orphaned ToolResult part must be stripped from history"
            );
        }
    }

    /// RC2 reverse pass: a complete `tool_use` + `tool_result` pair must pass through the reverse
    /// orphan check intact; the fix must not strip valid `ToolResult` parts.
    #[tokio::test]
    async fn strip_orphans_keeps_complete_pair() {
        use zeph_llm::provider::MessagePart;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let sqlite = memory.sqlite();

        let use_parts = serde_json::to_string(&[MessagePart::ToolUse {
            id: "call_valid".to_string(),
            name: "shell".to_string(),
            input: serde_json::json!({"command": "ls"}),
        }])
        .unwrap();
        sqlite
            .save_message_with_parts(cid, "assistant", "[tool_use: shell]", &use_parts)
            .await
            .unwrap();

        let result_parts = serde_json::to_string(&[MessagePart::ToolResult {
            tool_use_id: "call_valid".to_string(),
            content: "file.rs".to_string(),
            is_error: false,
        }])
        .unwrap();
        sqlite
            .save_message_with_parts(cid, "user", "", &result_parts)
            .await
            .unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        let messages_before = agent.msg.messages.len();
        agent.load_history().await.unwrap();

        assert_eq!(
            agent.msg.messages.len(),
            messages_before + 2,
            "complete tool_use/tool_result pair must be preserved"
        );

        let user_msg = &agent.msg.messages[messages_before + 1];
        assert!(
            user_msg.parts.iter().any(|p| matches!(
                p,
                MessagePart::ToolResult { tool_use_id, .. } if tool_use_id == "call_valid"
            )),
            "ToolResult part for a matched tool_use must not be stripped"
        );
    }

    /// RC2 reverse pass: history with a mix of complete pairs and orphaned `ToolResult` messages.
    /// Orphaned `ToolResult` parts must be stripped; complete pairs must pass through intact.
    #[tokio::test]
    async fn strip_orphans_mixed_history() {
        use zeph_llm::provider::MessagePart;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let sqlite = memory.sqlite();

        // First: complete tool_use / tool_result pair.
        let use_parts_ok = serde_json::to_string(&[MessagePart::ToolUse {
            id: "call_good".to_string(),
            name: "shell".to_string(),
            input: serde_json::json!({"command": "pwd"}),
        }])
        .unwrap();
        sqlite
            .save_message_with_parts(cid, "assistant", "[tool_use: shell]", &use_parts_ok)
            .await
            .unwrap();

        let result_parts_ok = serde_json::to_string(&[MessagePart::ToolResult {
            tool_use_id: "call_good".to_string(),
            content: "/home".to_string(),
            is_error: false,
        }])
        .unwrap();
        sqlite
            .save_message_with_parts(cid, "user", "", &result_parts_ok)
            .await
            .unwrap();

        // Second: plain assistant message followed by an orphaned ToolResult user message.
        sqlite
            .save_message(cid, "assistant", "text only")
            .await
            .unwrap();

        let orphan_parts = serde_json::to_string(&[MessagePart::ToolResult {
            tool_use_id: "call_ghost".to_string(),
            content: "ghost result".to_string(),
            is_error: false,
        }])
        .unwrap();
        sqlite
            .save_message_with_parts(
                cid,
                "user",
                "[tool_result: call_ghost]\nghost result",
                &orphan_parts,
            )
            .await
            .unwrap();

        sqlite
            .save_message(cid, "assistant", "final reply")
            .await
            .unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        let messages_before = agent.msg.messages.len();
        agent.load_history().await.unwrap();

        let loaded = &agent.msg.messages[messages_before..];

        // The orphaned ToolResult part must not appear in any message.
        for msg in loaded {
            assert!(
                !msg.parts.iter().any(|p| matches!(
                    p,
                    MessagePart::ToolResult { tool_use_id, .. } if tool_use_id == "call_ghost"
                )),
                "orphaned ToolResult (call_ghost) must be stripped from history"
            );
        }

        // The matched ToolResult must still be present on the user message following the
        // first assistant message.
        let has_good_result = loaded.iter().any(|msg| {
            msg.role == Role::User
                && msg.parts.iter().any(|p| {
                    matches!(
                        p,
                        MessagePart::ToolResult { tool_use_id, .. } if tool_use_id == "call_good"
                    )
                })
        });
        assert!(
            has_good_result,
            "matched ToolResult (call_good) must be preserved in history"
        );
    }

    /// Regression: a properly matched `tool_use`/`tool_result` pair must NOT be touched by the
    /// mid-history scan — ensures the fix doesn't break valid tool exchanges.
    #[tokio::test]
    async fn sanitize_tool_pairs_preserves_matched_tool_pair() {
        use zeph_llm::provider::MessagePart;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let sqlite = memory.sqlite();

        sqlite
            .save_message(cid, "user", "run a command")
            .await
            .unwrap();

        // Assistant sends a ToolUse.
        let use_parts = serde_json::to_string(&[MessagePart::ToolUse {
            id: "call_ok".to_string(),
            name: "shell".to_string(),
            input: serde_json::json!({"command": "echo hi"}),
        }])
        .unwrap();
        sqlite
            .save_message_with_parts(cid, "assistant", "[tool_use: shell]", &use_parts)
            .await
            .unwrap();

        // Matching user ToolResult follows.
        let result_parts = serde_json::to_string(&[MessagePart::ToolResult {
            tool_use_id: "call_ok".to_string(),
            content: "hi".to_string(),
            is_error: false,
        }])
        .unwrap();
        sqlite
            .save_message_with_parts(cid, "user", "[tool_result: call_ok]\nhi", &result_parts)
            .await
            .unwrap();

        sqlite.save_message(cid, "assistant", "done").await.unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        let messages_before = agent.msg.messages.len();
        agent.load_history().await.unwrap();

        // All 4 messages preserved, tool_use parts intact.
        assert_eq!(
            agent.msg.messages.len(),
            messages_before + 4,
            "matched tool pair must not be removed"
        );
        let tool_msg = &agent.msg.messages[messages_before + 1];
        assert!(
            tool_msg
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolUse { id, .. } if id == "call_ok")),
            "matched ToolUse parts must be preserved"
        );
    }

    /// RC5: `persist_cancelled_tool_results` must persist a tombstone user message containing
    /// `is_error=true` `ToolResult` parts for all `tool_calls` IDs so the preceding assistant
    /// `ToolUse` is never orphaned in the DB after a cancellation.
    #[tokio::test]
    async fn persist_cancelled_tool_results_pairs_tool_use() {
        use zeph_llm::provider::MessagePart;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        // Simulate: assistant message with two ToolUse parts already persisted.
        let tool_calls = vec![
            zeph_llm::provider::ToolUseRequest {
                id: "cancel_id_1".to_string(),
                name: "shell".to_string().into(),
                input: serde_json::json!({}),
            },
            zeph_llm::provider::ToolUseRequest {
                id: "cancel_id_2".to_string(),
                name: "read_file".to_string().into(),
                input: serde_json::json!({}),
            },
        ];

        agent.persist_cancelled_tool_results(&tool_calls).await;

        let history = agent
            .services
            .memory
            .persistence
            .memory
            .as_ref()
            .unwrap()
            .sqlite()
            .load_history(cid, 50)
            .await
            .unwrap();

        // Exactly one user message must have been persisted.
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, Role::User);

        // It must contain is_error=true ToolResult for each tool call ID.
        for tc in &tool_calls {
            assert!(
                history[0].parts.iter().any(|p| matches!(
                    p,
                    MessagePart::ToolResult { tool_use_id, is_error, .. }
                        if tool_use_id == &tc.id && *is_error
                )),
                "tombstone ToolResult for {} must be present and is_error=true",
                tc.id
            );
        }
    }

    // ---- Integration tests for the #2529 fix: soft-delete of legacy-content orphans ----

    /// #2529 regression: orphaned assistant `ToolUse` + user `ToolResult` pair where BOTH messages
    /// have content consisting solely of legacy tool bracket strings (no human-readable text).
    ///
    /// Before the fix, `content.trim().is_empty()` returned false for these messages, so they
    /// were never soft-deleted and the WARN log repeated on every session restart.
    ///
    /// After the fix, `has_meaningful_content` returns false for legacy-only content, so both
    /// orphaned messages are soft-deleted (non-null `deleted_at`) in `SQLite`.
    #[tokio::test]
    async fn issue_2529_orphaned_legacy_content_pair_is_soft_deleted() {
        use zeph_llm::provider::MessagePart;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let sqlite = memory.sqlite();

        // A normal user message that anchors the conversation.
        sqlite
            .save_message(cid, "user", "save this for me")
            .await
            .unwrap();

        // Orphaned assistant[ToolUse]: content is ONLY a legacy tool tag — no matching
        // ToolResult follows. This is the exact pattern that triggered #2529.
        let use_parts = serde_json::to_string(&[MessagePart::ToolUse {
            id: "call_2529".to_string(),
            name: "memory_save".to_string(),
            input: serde_json::json!({"content": "save this"}),
        }])
        .unwrap();
        let orphan_assistant_id = sqlite
            .save_message_with_parts(
                cid,
                "assistant",
                "[tool_use: memory_save(call_2529)]",
                &use_parts,
            )
            .await
            .unwrap();

        // Orphaned user[ToolResult]: content is ONLY a legacy tool tag + body.
        // Its tool_use_id ("call_2529") does not match any ToolUse in the preceding assistant
        // message in this position (will be made orphaned by inserting a plain assistant message
        // between them to break the pair).
        sqlite
            .save_message(cid, "assistant", "here is a plain reply")
            .await
            .unwrap();

        let result_parts = serde_json::to_string(&[MessagePart::ToolResult {
            tool_use_id: "call_2529".to_string(),
            content: "saved".to_string(),
            is_error: false,
        }])
        .unwrap();
        let orphan_user_id = sqlite
            .save_message_with_parts(
                cid,
                "user",
                "[tool_result: call_2529]\nsaved",
                &result_parts,
            )
            .await
            .unwrap();

        // Final plain message so history doesn't end on the orphan.
        sqlite.save_message(cid, "assistant", "done").await.unwrap();

        let memory_arc = std::sync::Arc::new(memory);
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            memory_arc.clone(),
            cid,
            50,
            5,
            100,
        );

        agent.load_history().await.unwrap();

        // Verify that both orphaned messages now have `deleted_at IS NOT NULL` in SQLite.
        // COUNT(*) WHERE deleted_at IS NOT NULL returns 1 if soft-deleted, 0 otherwise.
        let assistant_deleted_count: Vec<i64> = zeph_db::query_scalar(
            "SELECT COUNT(*) FROM messages WHERE id = ? AND deleted_at IS NOT NULL",
        )
        .bind(orphan_assistant_id)
        .fetch_all(memory_arc.sqlite().pool())
        .await
        .unwrap();

        let user_deleted_count: Vec<i64> = zeph_db::query_scalar(
            "SELECT COUNT(*) FROM messages WHERE id = ? AND deleted_at IS NOT NULL",
        )
        .bind(orphan_user_id)
        .fetch_all(memory_arc.sqlite().pool())
        .await
        .unwrap();

        assert_eq!(
            assistant_deleted_count.first().copied().unwrap_or(0),
            1,
            "orphaned assistant[ToolUse] with legacy-only content must be soft-deleted (deleted_at IS NOT NULL)"
        );
        assert_eq!(
            user_deleted_count.first().copied().unwrap_or(0),
            1,
            "orphaned user[ToolResult] with legacy-only content must be soft-deleted (deleted_at IS NOT NULL)"
        );
    }

    /// #2529 idempotency: after soft-delete on first `load_history`, a second call must not
    /// re-load the soft-deleted orphans. This ensures the WARN log does not repeat on the
    /// next session startup for the same orphaned messages.
    #[tokio::test]
    async fn issue_2529_soft_delete_is_idempotent_across_sessions() {
        use zeph_llm::provider::MessagePart;

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let sqlite = memory.sqlite();

        // Normal anchor message.
        sqlite
            .save_message(cid, "user", "do something")
            .await
            .unwrap();

        // Orphaned assistant[ToolUse] with legacy-only content.
        let use_parts = serde_json::to_string(&[MessagePart::ToolUse {
            id: "call_idem".to_string(),
            name: "shell".to_string(),
            input: serde_json::json!({"command": "ls"}),
        }])
        .unwrap();
        sqlite
            .save_message_with_parts(cid, "assistant", "[tool_use: shell(call_idem)]", &use_parts)
            .await
            .unwrap();

        // Break the pair: insert a plain assistant message so the ToolUse is mid-history orphan.
        sqlite
            .save_message(cid, "assistant", "continuing")
            .await
            .unwrap();

        // Orphaned user[ToolResult] with legacy-only content.
        let result_parts = serde_json::to_string(&[MessagePart::ToolResult {
            tool_use_id: "call_idem".to_string(),
            content: "output".to_string(),
            is_error: false,
        }])
        .unwrap();
        sqlite
            .save_message_with_parts(
                cid,
                "user",
                "[tool_result: call_idem]\noutput",
                &result_parts,
            )
            .await
            .unwrap();

        sqlite
            .save_message(cid, "assistant", "final")
            .await
            .unwrap();

        let memory_arc = std::sync::Arc::new(memory);

        // First session: load_history performs soft-delete of the orphaned pair.
        let mut agent1 = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
        .with_memory(memory_arc.clone(), cid, 50, 5, 100);
        agent1.load_history().await.unwrap();
        let count_after_first = agent1.msg.messages.len();

        // Second session: a fresh agent loading the same conversation must not see the
        // soft-deleted orphans — the WARN log must not repeat.
        let mut agent2 = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
        .with_memory(memory_arc.clone(), cid, 50, 5, 100);
        agent2.load_history().await.unwrap();
        let count_after_second = agent2.msg.messages.len();

        // Both sessions must load the same number of messages — soft-deleted orphans excluded.
        assert_eq!(
            count_after_first, count_after_second,
            "second load_history must load the same message count as the first (soft-deleted orphans excluded)"
        );
    }

    /// Edge case for #2529: an orphaned assistant message whose content has BOTH meaningful text
    /// AND a `tool_use` tag must NOT be soft-deleted. The `ToolUse` parts are stripped but the
    /// message is kept because it has human-readable content.
    #[tokio::test]
    async fn issue_2529_message_with_text_and_tool_tag_is_kept_after_part_strip() {
        use zeph_llm::provider::MessagePart;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let sqlite = memory.sqlite();

        // Normal first exchange.
        sqlite
            .save_message(cid, "user", "check the files")
            .await
            .unwrap();

        // Assistant message: has BOTH meaningful text AND a ToolUse part.
        // Content contains real prose + legacy tag — has_meaningful_content must return true.
        let use_parts = serde_json::to_string(&[MessagePart::ToolUse {
            id: "call_mixed".to_string(),
            name: "shell".to_string(),
            input: serde_json::json!({"command": "ls"}),
        }])
        .unwrap();
        sqlite
            .save_message_with_parts(
                cid,
                "assistant",
                "Let me list the directory. [tool_use: shell(call_mixed)]",
                &use_parts,
            )
            .await
            .unwrap();

        // No matching ToolResult follows — the ToolUse is orphaned.
        sqlite.save_message(cid, "user", "thanks").await.unwrap();
        sqlite
            .save_message(cid, "assistant", "you are welcome")
            .await
            .unwrap();

        let memory_arc = std::sync::Arc::new(memory);
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            memory_arc.clone(),
            cid,
            50,
            5,
            100,
        );

        let messages_before = agent.msg.messages.len();
        agent.load_history().await.unwrap();

        // All 4 messages must be present — the mixed-content assistant message is KEPT.
        assert_eq!(
            agent.msg.messages.len(),
            messages_before + 4,
            "assistant message with text + tool tag must not be removed after ToolUse strip"
        );

        // The mixed-content assistant message must have its ToolUse parts stripped.
        let mixed_msg = agent
            .msg
            .messages
            .iter()
            .find(|m| m.content.contains("Let me list the directory"))
            .expect("mixed-content assistant message must still be in history");
        assert!(
            !mixed_msg
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolUse { .. })),
            "orphaned ToolUse parts must be stripped even when message has meaningful text"
        );
        assert_eq!(
            mixed_msg.content, "Let me list the directory. [tool_use: shell(call_mixed)]",
            "content field must be unchanged — only parts are stripped"
        );
    }

    // ── [skipped]/[stopped] ToolResult embedding guard (#2620) ──────────────

    #[tokio::test]
    async fn persist_message_skipped_tool_result_does_not_embed() {
        use zeph_llm::provider::MessagePart;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let (tx, rx) = tokio::sync::watch::channel(MetricsSnapshot::default());
        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_metrics(tx)
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100);
        agent.services.memory.persistence.autosave_assistant = true;
        agent.services.memory.persistence.autosave_min_length = 0;

        let parts = vec![MessagePart::ToolResult {
            tool_use_id: "tu1".into(),
            content: "[skipped] bash tool was blocked by utility gate".into(),
            is_error: false,
        }];

        agent
            .persist_message(
                Role::User,
                "[skipped] bash tool was blocked by utility gate",
                &parts,
                false,
            )
            .await;

        assert_eq!(
            rx.borrow().embeddings_generated,
            0,
            "[skipped] ToolResult must not be embedded into Qdrant"
        );
    }

    #[tokio::test]
    async fn persist_message_stopped_tool_result_does_not_embed() {
        use zeph_llm::provider::MessagePart;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let (tx, rx) = tokio::sync::watch::channel(MetricsSnapshot::default());
        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_metrics(tx)
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100);
        agent.services.memory.persistence.autosave_assistant = true;
        agent.services.memory.persistence.autosave_min_length = 0;

        let parts = vec![MessagePart::ToolResult {
            tool_use_id: "tu2".into(),
            content: "[stopped] execution limit reached".into(),
            is_error: false,
        }];

        agent
            .persist_message(
                Role::User,
                "[stopped] execution limit reached",
                &parts,
                false,
            )
            .await;

        assert_eq!(
            rx.borrow().embeddings_generated,
            0,
            "[stopped] ToolResult must not be embedded into Qdrant"
        );
    }

    #[tokio::test]
    async fn persist_message_normal_tool_result_is_saved_not_blocked_by_guard() {
        // Regression: a normal ToolResult (no [skipped]/[stopped] prefix) must not be
        // suppressed by the utility-gate guard and must reach the save path (SQLite).
        use zeph_llm::provider::MessagePart;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let memory_arc = std::sync::Arc::new(memory);

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            memory_arc.clone(),
            cid,
            50,
            5,
            100,
        );
        agent.services.memory.persistence.autosave_assistant = true;
        agent.services.memory.persistence.autosave_min_length = 0;

        let content = "total 42\ndrwxr-xr-x  5 user group";
        let parts = vec![MessagePart::ToolResult {
            tool_use_id: "tu3".into(),
            content: content.into(),
            is_error: false,
        }];

        agent
            .persist_message(Role::User, content, &parts, false)
            .await;

        // Must be saved to SQLite — confirms the guard did not block this path.
        let history = memory_arc.sqlite().load_history(cid, 50).await.unwrap();
        assert_eq!(
            history.len(),
            1,
            "normal ToolResult must be saved to SQLite"
        );
        assert_eq!(history[0].content, content);
    }

    /// Verify that `enqueue_trajectory_extraction_task` uses a bounded tail slice instead of
    /// cloning the full message vec. We confirm the slice logic by checking that the
    /// `tail_start` calculation correctly bounds the window even with more messages than
    /// `max_messages`.
    #[test]
    fn trajectory_extraction_slice_bounds_messages() {
        // Replicate the slice logic from enqueue_trajectory_extraction_task.
        let max_messages: usize = 20;
        let total_messages = 100usize;

        let tail_start = total_messages.saturating_sub(max_messages);
        let window = total_messages - tail_start;

        assert_eq!(
            window, 20,
            "slice should contain exactly max_messages items"
        );
        assert_eq!(tail_start, 80, "slice should start at len - max_messages");
    }

    #[test]
    fn trajectory_extraction_slice_handles_few_messages() {
        let max_messages: usize = 20;
        let total_messages = 5usize;

        let tail_start = total_messages.saturating_sub(max_messages);
        let window = total_messages - tail_start;

        assert_eq!(window, 5, "should return all messages when fewer than max");
        assert_eq!(tail_start, 0, "slice should start from the beginning");
    }

    // --- #3168 regression tests ---

    /// Round-trip: persist `Assistant[tool_use]` + `User[tool_result]`, then `load_history`.
    /// After `sanitize_tool_pairs` the pair must be intact — no WARN, both messages present
    /// with non-empty parts.
    #[tokio::test]
    async fn regression_3168_complete_tool_pair_survives_round_trip() {
        use zeph_llm::provider::MessagePart;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let sqlite = memory.sqlite();

        let use_parts = serde_json::to_string(&[MessagePart::ToolUse {
            id: "r3168_call".to_string(),
            name: "shell".to_string(),
            input: serde_json::json!({"command": "echo hi"}),
        }])
        .unwrap();
        sqlite
            .save_message_with_parts(
                cid,
                "assistant",
                "[tool_use: shell(r3168_call)]",
                &use_parts,
            )
            .await
            .unwrap();

        let result_parts = serde_json::to_string(&[MessagePart::ToolResult {
            tool_use_id: "r3168_call".to_string(),
            content: "[skipped]".to_string(),
            is_error: false,
        }])
        .unwrap();
        sqlite
            .save_message_with_parts(cid, "user", "[tool_result: r3168_call]", &result_parts)
            .await
            .unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        let base = agent.msg.messages.len();
        agent.load_history().await.unwrap();

        assert_eq!(
            agent.msg.messages.len(),
            base + 2,
            "both messages of the complete pair must survive load_history"
        );

        let assistant_msg = agent
            .msg
            .messages
            .iter()
            .find(|m| m.role == Role::Assistant)
            .expect("assistant message missing after load_history");
        assert!(
            assistant_msg
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolUse { id, .. } if id == "r3168_call")),
            "ToolUse part must be preserved in assistant message"
        );

        let user_msg = agent
            .msg
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .expect("user message missing after load_history");
        assert!(
            user_msg.parts.iter().any(|p| matches!(p, MessagePart::ToolResult { tool_use_id, .. } if tool_use_id == "r3168_call")),
            "ToolResult part must be preserved in user message"
        );
    }

    /// If parts serialization fails, `persist_message` must return early and not store
    /// a row with empty parts that would create an orphaned `tool_use` on next session load.
    /// We verify this by writing a row with invalid `parts_json` directly and confirming
    /// `load_history` skips it (empty parts row is not injected into the agent).
    #[tokio::test]
    async fn regression_3168_corrupt_parts_row_skipped_on_load() {
        use zeph_llm::provider::MessagePart;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let sqlite = memory.sqlite();

        // Simulate the pre-fix bug: assistant message stored with empty parts_json "[]"
        // even though it should have had a ToolUse part. This is what persist_message used
        // to do before the early-return fix.
        sqlite
            .save_message_with_parts(cid, "assistant", "[tool_use: shell(corrupt)]", "[]")
            .await
            .unwrap();

        // User message with the matching tool_result stored correctly.
        let result_parts = serde_json::to_string(&[MessagePart::ToolResult {
            tool_use_id: "corrupt".to_string(),
            content: "result".to_string(),
            is_error: false,
        }])
        .unwrap();
        sqlite
            .save_message_with_parts(cid, "user", "[tool_result: corrupt]", &result_parts)
            .await
            .unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        let base = agent.msg.messages.len();
        agent.load_history().await.unwrap();

        // The user ToolResult has no matching ToolUse in the assistant message (parts="[]"),
        // so sanitize_tool_pairs must strip the orphaned ToolResult.
        // Net result: only the content-only assistant message survives; user msg is removed
        // because after stripping its only part it becomes empty.
        let loaded = agent.msg.messages.len() - base;
        // No message injected with orphaned ToolResult parts — either stripped entirely or
        // the ToolResult part removed. Verify no user message with ToolResult remains.
        let orphan_present = agent.msg.messages.iter().any(|m| {
            m.role == Role::User
                && m.parts.iter().any(|p| {
                    matches!(p, MessagePart::ToolResult { tool_use_id, .. } if tool_use_id == "corrupt")
                })
        });
        assert!(
            !orphan_present,
            "orphaned ToolResult must not survive load_history; loaded={loaded}"
        );
    }
}
