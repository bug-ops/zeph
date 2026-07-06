// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Background memory-extraction scheduling.
//!
//! These methods run the foreground guards (enabled checks, injection / tool-result skips,
//! RPE surprise gating) and enqueue the heavy extraction work on the supervisor. They are
//! invoked from [`super::store`] after a message is persisted. Each method degrades to a no-op
//! when its feature is disabled or no memory is attached.

use super::super::Agent;
use crate::channel::Channel;
use zeph_agent_persistence::graph::{build_graph_extraction_config, collect_context_messages};
use zeph_llm::provider::{LlmProvider as _, MessagePart, Role};

impl<C: Channel> Agent<C> {
    /// Prepare graph extraction guards in foreground, then enqueue heavy work via supervisor.
    ///
    /// Guards (enabled check, injection/tool-result skip) stay on the foreground path.
    /// The RPE check and actual extraction run in background (S2: no `send_status`).
    #[tracing::instrument(
        name = "core.persist.enqueue_graph_extraction",
        skip_all,
        level = "debug"
    )]
    pub(super) async fn enqueue_graph_extraction_task(
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
        let embed_timeout_secs = self
            .services
            .memory
            .persistence
            .memory
            .as_ref()
            .map_or(5, |m| m.embed_timeout().as_secs());
        let turn_index = u32::try_from(self.services.sidequest.turn_counter).unwrap_or(u32::MAX);
        let extraction_cfg = build_graph_extraction_config(
            cfg,
            self.services
                .memory
                .persistence
                .conversation_id
                .map(|c| c.0),
            embed_timeout_secs,
            Some(turn_index),
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
        let cancel = self.runtime.lifecycle.cancel_token.child_token();

        self.runtime.lifecycle.supervisor.spawn(
            super::super::agent_supervisor::TaskClass::Enrichment,
            "graph_extraction",
            async move {
                let extraction_handle = memory.spawn_graph_extraction(
                    content_owned,
                    context_messages,
                    extraction_cfg,
                    validator,
                    provider_override,
                    cancel,
                );

                // After extraction completes, refresh graph count metrics.
                if let (Some(store), Some(tx)) = (graph_store, metrics_tx) {
                    let _ = extraction_handle.await;
                    let (entities, edges, communities) =
                        super::super::utils::fetch_graph_counts(&store).await;
                    let elapsed = start_time.elapsed().as_secs();
                    tx.send_modify(|m| {
                        m.uptime_seconds = elapsed;
                        m.graph_entities_total = entities;
                        m.graph_edges_total = edges;
                        m.graph_communities_total = communities;
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
            super::super::agent_supervisor::TaskClass::Telemetry,
            "graph_count_sync",
            async move {
                let Some(store) = graph_store_sync else {
                    return;
                };
                let Some(tx) = metrics_tx_sync else { return };

                let (entities, edges, communities) =
                    super::super::utils::fetch_graph_counts(&store).await;
                let elapsed = start_time_sync.elapsed().as_secs();
                tx.send_modify(|m| {
                    m.uptime_seconds = elapsed;
                    m.graph_entities_total = entities;
                    m.graph_edges_total = edges;
                    m.graph_communities_total = communities;
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
    pub(super) fn enqueue_persona_extraction_task(&mut self) {
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
            super::super::agent_supervisor::TaskClass::Enrichment,
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
    pub(super) fn enqueue_trajectory_extraction_task(&mut self) {
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
            super::super::agent_supervisor::TaskClass::Enrichment,
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
    pub(super) fn enqueue_reasoning_extraction_task(&mut self) {
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
        let embed_timeout = memory.embed_timeout();
        let self_judge_window = cfg.self_judge_window;
        let min_assistant_chars = cfg.min_assistant_chars;

        self.runtime.lifecycle.supervisor.spawn(
            super::super::agent_supervisor::TaskClass::Enrichment,
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
                        embed_timeout,
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
    #[tracing::instrument(name = "core.persist.rpe_should_skip", skip_all, level = "debug")]
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
