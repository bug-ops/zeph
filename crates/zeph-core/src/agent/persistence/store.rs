// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Message persistence and post-persist scheduling.
//!
//! [`Agent::persist_message`] writes a message through [`PersistenceService`], forwards metric
//! deltas, and — once the message is stored — fans out the background enrichment tasks
//! (summarization, graph/persona/trajectory/reasoning extraction, `MemCoT` distillation).

use super::super::Agent;
use crate::channel::Channel;
use zeph_agent_persistence::{
    MemoryPersistenceView, MetricsView, PersistMessageRequest, PersistenceService, SecurityView,
};
use zeph_llm::provider::{MessagePart, Role};

impl<C: Channel> Agent<C> {
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

        // INV-SP-1 (spec-068 §13): the durable event log must be appended and flushed before the
        // SQLite `messages` projection is written — the projection must never lead the log. A
        // failed session-log write is logged and the turn proceeds; a crash between the two
        // leaves the log ahead of the projection, which INV-SP-3 reconciles on next open.
        if let Some(sink) = self.services.session.session_sink.clone() {
            tracing::debug!("persist_message: session_sink.record_message start");
            if let Err(e) = sink.record_message(role, content, parts).await {
                tracing::warn!(error = %e, "failed to append session event log entry");
            }
            tracing::debug!("persist_message: session_sink.record_message done");
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
        // PAAC secret masking (#5437) is structural at the provider boundary —
        // `resolve_background_provider` returns an already-masked provider.
        let provider = self.resolve_background_provider(distill_provider_name);

        let content = assistant_content.to_owned();
        let supervisor = &mut self.runtime.lifecycle.supervisor;

        accumulator.maybe_enqueue_distill(&content, provider, |name, fut| {
            supervisor.spawn(
                super::super::agent_supervisor::TaskClass::Enrichment,
                name,
                fut,
            );
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

        self.runtime
            .lifecycle
            .supervisor
            .spawn_summarization("summarization", async move {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    memory.summarize(cid, batch_size),
                )
                .await
                {
                    Ok(Ok(Some(outcome))) => {
                        tracing::info!(
                            "background summarization: created summary {} for conversation {cid} \
                         ({} messages folded)",
                            outcome.summary_id,
                            outcome.messages_folded
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
}
