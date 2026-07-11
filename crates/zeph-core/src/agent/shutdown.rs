// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Graceful shutdown: session-summary generation and orphaned tool-use flush.
//!
//! Extracted from `agent/mod.rs` (#4923). Holds the shutdown lifecycle: building a
//! structured session summary via the LLM (with plain-text fallback), persisting it,
//! and emitting tombstone `ToolResult` parts for any unpaired `ToolUse` left in history.

use super::Agent;
use crate::channel::Channel;
use zeph_llm::provider::{LlmProvider, Message, MessageMetadata, Role};

impl<C: Channel> Agent<C> {
    /// Call the LLM to generate a structured session summary with a configurable timeout.
    ///
    /// Falls back to plain-text chat if structured output fails or times out. Returns `None` on
    /// any failure, logging a warning — callers must treat `None` as "skip storage".
    ///
    /// Each LLM attempt is bounded by `shutdown_summary_timeout_secs`; in the worst case
    /// (structured call times out and plain-text fallback also times out) this adds up to
    /// `2 * shutdown_summary_timeout_secs` of shutdown latency.
    async fn call_llm_for_session_summary(
        &self,
        chat_messages: &[Message],
    ) -> Option<zeph_memory::StructuredSummary> {
        let provider = self.resolve_background_provider(
            &self.services.memory.compaction.shutdown_summary_provider,
        );
        let timeout_dur = std::time::Duration::from_secs(
            self.services
                .memory
                .compaction
                .shutdown_summary_timeout_secs,
        );
        match tokio::time::timeout(
            timeout_dur,
            provider.chat_typed_erased::<zeph_memory::StructuredSummary>(chat_messages),
        )
        .await
        {
            Ok(Ok(s)) => Some(s),
            Ok(Err(e)) => {
                tracing::warn!(
                    "shutdown summary: structured LLM call failed, falling back to plain: {e:#}"
                );
                self.plain_text_summary_fallback(&provider, chat_messages, timeout_dur)
                    .await
            }
            Err(_) => {
                tracing::warn!(
                    "shutdown summary: structured LLM call timed out after {}s, falling back to plain",
                    self.services
                        .memory
                        .compaction
                        .shutdown_summary_timeout_secs
                );
                self.plain_text_summary_fallback(&provider, chat_messages, timeout_dur)
                    .await
            }
        }
    }
    async fn plain_text_summary_fallback(
        &self,
        provider: &zeph_llm::any::AnyProvider,
        chat_messages: &[Message],
        timeout_dur: std::time::Duration,
    ) -> Option<zeph_memory::StructuredSummary> {
        match tokio::time::timeout(timeout_dur, provider.chat(chat_messages)).await {
            Ok(Ok(plain)) => Some(zeph_memory::StructuredSummary {
                summary: plain,
                key_facts: vec![],
                entities: vec![],
            }),
            Ok(Err(e)) => {
                tracing::warn!("shutdown summary: plain LLM fallback failed: {e:#}");
                None
            }
            Err(_) => {
                tracing::warn!("shutdown summary: plain LLM fallback timed out");
                None
            }
        }
    }
    /// Persist tombstone `ToolResult` messages for any assistant `ToolUse` parts that were written
    /// to the DB during this session but never paired with a `ToolResult` (e.g. because stdin
    /// closed while tool execution was in progress). Without this the next session startup strips
    /// those assistant messages and emits orphan warnings.
    pub(super) async fn flush_orphaned_tool_use_on_shutdown(&mut self) {
        use zeph_llm::provider::{MessagePart, Role};

        // Walk messages in reverse: if the last assistant message (ignoring any trailing
        // system messages) has ToolUse parts and is NOT immediately followed by a user
        // message whose ToolResult ids cover those ToolUse ids, persist tombstones.
        let msgs = &self.msg.messages;
        // Find last assistant message index.
        let Some(asst_idx) = msgs.iter().rposition(|m| m.role == Role::Assistant) else {
            return;
        };
        let asst_msg = &msgs[asst_idx];
        let tool_use_ids: Vec<(&str, &str, &serde_json::Value)> = asst_msg
            .parts
            .iter()
            .filter_map(|p| {
                if let MessagePart::ToolUse { id, name, input } = p {
                    Some((id.as_str(), name.as_str(), input))
                } else {
                    None
                }
            })
            .collect();
        if tool_use_ids.is_empty() {
            return;
        }

        // Check whether a following user message already pairs all ToolUse ids.
        let paired_ids: std::collections::HashSet<&str> = msgs
            .get(asst_idx + 1..)
            .into_iter()
            .flatten()
            .filter(|m| m.role == Role::User)
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| {
                if let MessagePart::ToolResult { tool_use_id, .. } = p {
                    Some(tool_use_id.as_str())
                } else {
                    None
                }
            })
            .collect();

        let unpaired: Vec<zeph_llm::provider::ToolUseRequest> = tool_use_ids
            .iter()
            .filter(|(id, _, _)| !paired_ids.contains(*id))
            .map(|(id, name, input)| zeph_llm::provider::ToolUseRequest {
                id: (*id).to_owned(),
                name: (*name).to_owned().into(),
                input: (*input).clone(),
            })
            .collect();

        if unpaired.is_empty() {
            return;
        }

        tracing::info!(
            count = unpaired.len(),
            "shutdown: persisting tombstone ToolResults for unpaired in-flight tool calls"
        );
        // Splice immediately after the orphaned assistant message rather than appending at the
        // true end: a later turn may already have appended its own message past `asst_idx` by
        // the time shutdown runs (see #5646), and appending there would still leave the ToolUse
        // not immediately followed by its ToolResult.
        self.persist_cancelled_tool_results(&unpaired, Some(asst_idx + 1))
            .await;
    }
    /// Generate and store a lightweight session summary at shutdown when no hard compaction fired.
    ///
    /// Guards:
    /// - `self.runtime.config.bare` must be `false` (#5551 — bare mode never fires shutdown LLM calls)
    /// - `shutdown_summary` config must be enabled
    /// - `conversation_id` must be set (memory must be attached)
    /// - no existing session summary in the store (primary guard — resilient to failed Qdrant writes)
    /// - at least `shutdown_summary_min_messages` user-turn messages in history
    ///
    /// All errors are logged as warnings and swallowed — shutdown must never fail.
    pub(super) async fn maybe_store_shutdown_summary(&mut self) {
        if self.runtime.config.bare {
            return;
        }
        if !self.services.memory.compaction.shutdown_summary {
            return;
        }
        let Some(memory) = self.services.memory.persistence.memory.clone() else {
            return;
        };
        let Some(conversation_id) = self.services.memory.persistence.conversation_id else {
            return;
        };

        // Primary guard: check if a summary already exists (handles failed Qdrant writes too).
        match memory.has_session_summary(conversation_id).await {
            Ok(true) => {
                tracing::debug!("shutdown summary: session already has a summary, skipping");
                return;
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!("shutdown summary: failed to check existing summary: {e:#}");
                return;
            }
        }

        // Count user-turn messages only (skip system prompt at index 0).
        let user_count = self
            .msg
            .messages
            .iter()
            .skip(1)
            .filter(|m| m.role == Role::User)
            .count();
        let min_messages = self
            .services
            .memory
            .compaction
            .shutdown_summary_min_messages;
        if user_count < min_messages {
            tracing::debug!(
                user_count,
                min = min_messages,
                "shutdown summary: too few user messages, skipping"
            );
            return;
        }

        self.channel
            .send_status_best_effort("Saving session summary...")
            .await;

        // Collect last N messages (skip system prompt at index 0).
        let max = self
            .services
            .memory
            .compaction
            .shutdown_summary_max_messages;
        if max == 0 {
            tracing::debug!("shutdown summary: max_messages=0, skipping");
            return;
        }
        let non_system: Vec<_> = self.msg.messages.iter().skip(1).collect();
        let slice = if non_system.len() > max {
            &non_system[non_system.len() - max..]
        } else {
            &non_system[..]
        };

        let msgs_for_prompt: Vec<(zeph_memory::MessageId, String, String)> = slice
            .iter()
            .map(|m| {
                let role = match m.role {
                    Role::Assistant => "assistant".to_owned(),
                    Role::System => "system".to_owned(),
                    Role::User | _ => "user".to_owned(),
                };
                (zeph_memory::MessageId(0), role, m.content.clone())
            })
            .collect();

        let prompt = zeph_memory::build_summarization_prompt(&msgs_for_prompt);
        let chat_messages = vec![Message {
            role: Role::User,
            content: prompt,
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let Some(structured) = self.call_llm_for_session_summary(&chat_messages).await else {
            self.channel.send_status_best_effort("").await;
            return;
        };

        if let Err(e) = memory
            .store_shutdown_summary(conversation_id, &structured.summary, &structured.key_facts)
            .await
        {
            tracing::warn!("shutdown summary: storage failed: {e:#}");
        } else {
            tracing::info!(
                conversation_id = conversation_id.0,
                "shutdown summary stored"
            );
        }

        self.channel.send_status_best_effort("").await;
    }
    /// Gracefully shut down the agent and persist state.
    ///
    /// Performs the following cleanup:
    ///
    /// 1. **Message persistence** — Deferred database writes (hide/summary operations)
    ///    are flushed to memory or disk
    /// 2. **Provider state** — LLM router state (e.g., Thompson sampling counters) is saved
    ///    to the vault
    /// 3. **Sub-agents** — All active sub-agent tasks are terminated
    /// 4. **MCP servers** — All connected Model Context Protocol servers are shut down
    /// 5. **Metrics finalization** — Compaction metrics and session metrics are recorded
    /// 6. **Memory finalization** — Vector stores and semantic indices are flushed
    /// 7. **Skill state** — Self-learning engine saves evolved skill definitions
    ///
    /// Call this before dropping the agent to ensure no data loss.
    #[tracing::instrument(name = "core.agent.shutdown", skip_all, level = "debug")]
    pub async fn shutdown(&mut self) {
        self.channel
            .send_status_best_effort("Shutting down...")
            .await;

        // CRIT-1: persist Thompson state accumulated during this session.
        self.provider.save_router_state().await;

        // Persist AdaptOrch Beta-arm table alongside Thompson state.
        if let Some(ref advisor) = self.services.orchestration.topology_advisor
            && let Err(e) = advisor.save().await
        {
            tracing::warn!(error = %e, "adaptorch: failed to persist state");
        }

        if let Some(ref mut mgr) = self.services.orchestration.subagent_manager {
            mgr.shutdown_all();
        }

        if let Some(ref manager) = self.services.mcp.manager {
            manager.shutdown_all_shared().await;
        }

        // Finalize compaction trajectory: push the last open segment into the Vec.
        // This segment would otherwise only be pushed when the next hard compaction fires,
        // which never happens at session end.
        if let Some(turns) = self.context_manager.turns_since_last_hard_compaction() {
            self.update_metrics(|m| {
                m.compaction_turns_after_hard.push(turns);
            });
            self.context_manager
                .set_turns_since_last_hard_compaction(None);
        }

        if let Some(ref tx) = self.runtime.metrics.metrics_tx {
            let m = tx.borrow();
            if m.filter_applications > 0 {
                #[allow(clippy::cast_precision_loss)]
                let pct = if m.filter_raw_tokens > 0 {
                    m.filter_saved_tokens as f64 / m.filter_raw_tokens as f64 * 100.0
                } else {
                    0.0
                };
                tracing::info!(
                    raw_tokens = m.filter_raw_tokens,
                    saved_tokens = m.filter_saved_tokens,
                    applications = m.filter_applications,
                    "tool output filtering saved ~{} tokens ({pct:.0}%)",
                    m.filter_saved_tokens,
                );
            }
            if m.compaction_hard_count > 0 {
                tracing::info!(
                    hard_compactions = m.compaction_hard_count,
                    turns_after_hard = ?m.compaction_turns_after_hard,
                    "hard compaction trajectory"
                );
            }
        }

        // Flush tombstone ToolResults for any assistant ToolUse that was persisted but never
        // paired with a ToolResult (e.g. stdin EOF mid-execution). Without this the next session
        // startup strips the orphaned ToolUse and emits warnings.
        self.flush_orphaned_tool_use_on_shutdown().await;

        // Signal the experiment CancellationToken first so the task can clean up gracefully,
        // then abort the handle to guarantee it does not outlive the agent regardless.
        if let Some(ref token) = self.services.experiments.cancel {
            token.cancel();
        }
        if let Some(h) = self.services.experiments.handle.take() {
            h.abort();
        }

        // Signal cooperative cancellation to the graph-extraction background task before the
        // hard abort below. This lets the task exit at a clean checkpoint (e.g. after the
        // community-refresh select arm fires) rather than being cut mid-write.
        if let Some(memory) = self.services.memory.persistence.memory.as_ref() {
            memory.cancel_graph_extraction();
        }

        // Forcibly abort in-flight Enrichment and Telemetry tasks tracked by the supervisor.
        self.runtime.lifecycle.supervisor.abort_all();

        // Abort background task handles not tracked by BackgroundSupervisor.
        // Per the Await Discipline rule, fire-and-forget handles must be aborted on shutdown.
        if let Some(h) = self.services.compression.pending_task_goal.take() {
            h.abort();
        }
        if let Some(h) = self.services.compression.pending_sidequest_result.take() {
            h.abort();
        }
        if let Some(h) = self.services.compression.pending_subgoal.take() {
            h.abort();
        }
        self.flush_durable_writer().await;

        // Abort learning tasks (JoinSet detached at turn boundaries but not on shutdown).
        self.services.learning_engine.learning_tasks.abort_all();

        // Await the AutoSkill trace extraction task so it is not silently dropped.
        // Bounded to avoid hanging shutdown when the LLM call inside the task stalls.
        if let Some(h) = self.services.learning_engine.trace_extraction_handle.take() {
            let deadline = std::time::Duration::from_mins(2);
            match tokio::time::timeout(deadline, h.join()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!("trace_extraction: task error at shutdown: {e}"),
                Err(_) => tracing::warn!(
                    "trace_extraction: timed out at shutdown ({}s), aborting",
                    deadline.as_secs()
                ),
            }
        }

        // Abort the heuristic promotion loop (periodic task; abort is safe because
        // promotion_already_evaluated ensures idempotent retry on next startup).
        if let Some(h) = self
            .services
            .learning_engine
            .heuristic_promotion_handle
            .take()
        {
            h.abort();
        }

        // Drain pending shadow sentinel DB writes before final teardown.
        if let Some(ref sentinel) = self.services.security.shadow_sentinel {
            sentinel.drain_pending().await;
        }

        // Allow cancelled tasks to release their HTTP connections before the summary LLM call.
        // abort_all() posts cancellation signals but does not drain tasks; aborted futures only
        // observe cancellation at their next .await point. Without yielding here the summary
        // call races in-flight enrichment HTTP connections for the same API rate-limit budget.
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }

        self.maybe_store_shutdown_summary().await;
        self.maybe_store_session_digest().await;

        tracing::info!("agent shutdown complete");
    }

    /// Flush buffered durable journal entries then abort the writer task, for both the P2
    /// (orchestration) and P1 (agent-turn, #5452) durable adapters.
    ///
    /// `flush()` has a built-in ack timeout; the outer 2 s cap ensures shutdown never
    /// hangs beyond that. Errors are logged as warnings — shutdown must not fail.
    async fn flush_durable_writer(&mut self) {
        let flush_deadline = std::time::Duration::from_secs(2);
        if let Some(ref writer) = self.services.orchestration.durable_writer {
            match tokio::time::timeout(flush_deadline, writer.flush()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "durable writer: flush on shutdown failed");
                }
                Err(_) => tracing::warn!("durable writer: flush timed out on shutdown"),
            }
        }
        if let Some(h) = self.services.orchestration.durable_writer_task.take() {
            h.abort();
        }
        if let Some(ref writer) = self.services.session.durable_writer {
            match tokio::time::timeout(flush_deadline, writer.flush()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "durable agent_turns writer: flush on shutdown failed");
                }
                Err(_) => tracing::warn!("durable agent_turns writer: flush timed out on shutdown"),
            }
        }
        if let Some(h) = self.services.session.durable_writer_task.take() {
            h.abort();
        }
    }
}
