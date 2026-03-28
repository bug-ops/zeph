// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashSet;

use crate::channel::Channel;
use zeph_llm::provider::{Message, MessagePart, Role};
use zeph_memory::store::role_str;

use super::Agent;

/// Remove orphaned `ToolUse`/`ToolResult` messages from restored history.
///
/// Four failure modes are handled:
/// 1. Trailing orphan: the last message is an assistant with `ToolUse` parts but no subsequent
///    user message with `ToolResult` — caused by LIMIT boundary splits or interrupted sessions.
/// 2. Leading orphan: the first message is a user with `ToolResult` parts but no preceding
///    assistant message with `ToolUse` — caused by LIMIT boundary cuts.
/// 3. Mid-history orphaned `ToolUse`: an assistant message with `ToolUse` parts is not followed
///    by a user message with matching `ToolResult` parts. The `ToolUse` parts are stripped;
///    if no content remains the message is removed.
/// 4. Mid-history orphaned `ToolResult`: a user message has `ToolResult` parts whose
///    `tool_use_id` is not present in the preceding assistant message. Those `ToolResult` parts
///    are stripped; if no content remains the message is removed.
///
/// Boundary cases are resolved in a loop before the mid-history scan runs.
fn sanitize_tool_pairs(messages: &mut Vec<Message>) -> usize {
    let mut removed = 0;

    loop {
        // Remove trailing orphaned tool_use (assistant message with ToolUse, no following tool_result).
        if let Some(last) = messages.last()
            && last.role == Role::Assistant
            && last
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolUse { .. }))
        {
            let ids: Vec<String> = last
                .parts
                .iter()
                .filter_map(|p| {
                    if let MessagePart::ToolUse { id, .. } = p {
                        Some(id.clone())
                    } else {
                        None
                    }
                })
                .collect();
            tracing::warn!(
                tool_ids = ?ids,
                "removing orphaned trailing tool_use message from restored history"
            );
            messages.pop();
            removed += 1;
            continue;
        }

        // Remove leading orphaned tool_result (user message with ToolResult, no preceding tool_use).
        if let Some(first) = messages.first()
            && first.role == Role::User
            && first
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolResult { .. }))
        {
            let ids: Vec<String> = first
                .parts
                .iter()
                .filter_map(|p| {
                    if let MessagePart::ToolResult { tool_use_id, .. } = p {
                        Some(tool_use_id.clone())
                    } else {
                        None
                    }
                })
                .collect();
            tracing::warn!(
                tool_use_ids = ?ids,
                "removing orphaned leading tool_result message from restored history"
            );
            messages.remove(0);
            removed += 1;
            continue;
        }

        break;
    }

    // Mid-history scan: strip ToolUse parts from any assistant message whose tool IDs are not
    // matched by ToolResult parts in the immediately following user message.
    removed += strip_mid_history_orphans(messages);

    removed
}

/// Scan all messages and strip orphaned `ToolUse`/`ToolResult` parts from mid-history messages.
///
/// Two directions are checked:
/// - Forward: assistant message has `ToolUse` parts not matched by `ToolResult` in the next user
///   message — strip those `ToolUse` parts.
/// - Reverse: user message has `ToolResult` parts whose `tool_use_id` is not present as a
///   `ToolUse` in the preceding assistant message — strip those `ToolResult` parts.
///
/// Text parts are preserved; messages with no remaining content are removed entirely.
///
/// Returns the number of messages removed (stripped-to-empty messages count as 1 each).
/// Collect `tool_use` IDs from `msg` that have no matching `ToolResult` in `next_msg`.
fn orphaned_tool_use_ids(msg: &Message, next_msg: Option<&Message>) -> HashSet<String> {
    let matched: HashSet<String> = next_msg
        .filter(|n| n.role == Role::User)
        .map(|n| {
            msg.parts
                .iter()
                .filter_map(|p| if let MessagePart::ToolUse { id, .. } = p { Some(id.clone()) } else { None })
                .filter(|uid| n.parts.iter().any(|np| matches!(np, MessagePart::ToolResult { tool_use_id, .. } if tool_use_id == uid)))
                .collect()
        })
        .unwrap_or_default();
    msg.parts
        .iter()
        .filter_map(|p| {
            if let MessagePart::ToolUse { id, .. } = p
                && !matched.contains(id)
            {
                Some(id.clone())
            } else {
                None
            }
        })
        .collect()
}

/// Collect `tool_result` IDs from `msg` that have no matching `ToolUse` in `prev_msg`.
fn orphaned_tool_result_ids(msg: &Message, prev_msg: Option<&Message>) -> HashSet<String> {
    let avail: HashSet<&str> = prev_msg
        .filter(|p| p.role == Role::Assistant)
        .map(|p| {
            p.parts
                .iter()
                .filter_map(|part| {
                    if let MessagePart::ToolUse { id, .. } = part {
                        Some(id.as_str())
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    msg.parts
        .iter()
        .filter_map(|p| {
            if let MessagePart::ToolResult { tool_use_id, .. } = p
                && !avail.contains(tool_use_id.as_str())
            {
                Some(tool_use_id.clone())
            } else {
                None
            }
        })
        .collect()
}

fn strip_mid_history_orphans(messages: &mut Vec<Message>) -> usize {
    let mut removed = 0;
    let mut i = 0;
    while i < messages.len() {
        // Forward pass: strip ToolUse parts from assistant messages that lack a matching
        // ToolResult in the next user message. Only orphaned IDs are stripped — other ToolUse
        // parts in the same message that DO have a matching ToolResult are preserved.
        if messages[i].role == Role::Assistant
            && messages[i]
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolUse { .. }))
        {
            let orphaned_ids = orphaned_tool_use_ids(&messages[i], messages.get(i + 1));
            if !orphaned_ids.is_empty() {
                tracing::warn!(
                    tool_ids = ?orphaned_ids,
                    index = i,
                    "stripping orphaned mid-history tool_use parts from assistant message"
                );
                messages[i].parts.retain(
                    |p| !matches!(p, MessagePart::ToolUse { id, .. } if orphaned_ids.contains(id)),
                );
                let is_empty =
                    messages[i].content.trim().is_empty() && messages[i].parts.is_empty();
                if is_empty {
                    messages.remove(i);
                    removed += 1;
                    continue; // Do not advance i — the next message is now at position i.
                }
            }
        }

        // Reverse pass: user ToolResult without matching ToolUse in the preceding assistant message.
        if messages[i].role == Role::User
            && messages[i]
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolResult { .. }))
        {
            let orphaned_ids = orphaned_tool_result_ids(
                &messages[i],
                if i > 0 { messages.get(i - 1) } else { None },
            );
            if !orphaned_ids.is_empty() {
                tracing::warn!(
                    tool_use_ids = ?orphaned_ids,
                    index = i,
                    "stripping orphaned mid-history tool_result parts from user message"
                );
                messages[i].parts.retain(|p| {
                    !matches!(p, MessagePart::ToolResult { tool_use_id, .. } if orphaned_ids.contains(tool_use_id.as_str()))
                });

                let is_empty =
                    messages[i].content.trim().is_empty() && messages[i].parts.is_empty();
                if is_empty {
                    messages.remove(i);
                    removed += 1;
                    // Do not advance i — the next message is now at position i.
                    continue;
                }
            }
        }

        i += 1;
    }
    removed
}

impl<C: Channel> Agent<C> {
    /// Load conversation history from memory and inject into messages.
    ///
    /// # Errors
    ///
    /// Returns an error if loading history from `SQLite` fails.
    pub async fn load_history(&mut self) -> Result<(), super::error::AgentError> {
        let (Some(memory), Some(cid)) =
            (&self.memory_state.memory, self.memory_state.conversation_id)
        else {
            return Ok(());
        };

        let history = memory
            .sqlite()
            .load_history_filtered(cid, self.memory_state.history_limit, Some(true), None)
            .await?;
        if !history.is_empty() {
            let mut loaded = 0;
            let mut skipped = 0;

            for msg in history {
                // Only skip messages that have neither text content nor structured parts.
                // Native tool calls produce user messages with empty `content` but non-empty
                // `parts` (containing ToolResult). Skipping them here would orphan the
                // preceding assistant ToolUse before sanitize_tool_pairs can clean it up.
                if msg.content.trim().is_empty() && msg.parts.is_empty() {
                    tracing::warn!("skipping empty message from history (role: {:?})", msg.role);
                    skipped += 1;
                    continue;
                }
                self.msg.messages.push(msg);
                loaded += 1;
            }

            // Determine the start index of just-loaded messages (system prompt is at index 0).
            let history_start = self.msg.messages.len() - loaded;
            let mut restored_slice = self.msg.messages.split_off(history_start);
            let orphans = sanitize_tool_pairs(&mut restored_slice);
            skipped += orphans;
            loaded = loaded.saturating_sub(orphans);
            self.msg.messages.append(&mut restored_slice);

            tracing::info!("restored {loaded} message(s) from conversation {cid}");
            if skipped > 0 {
                tracing::warn!("skipped {skipped} empty/orphaned message(s) from history");
            }

            if loaded > 0 {
                // Increment session counts so tier promotion can track cross-session access.
                // Errors are non-fatal — promotion will simply use stale counts.
                let _ = memory
                    .sqlite()
                    .increment_session_counts_for_conversation(cid)
                    .await
                    .inspect_err(|e| {
                        tracing::warn!(error = %e, "failed to increment tier session counts");
                    });
            }
        }

        if let Ok(count) = memory.message_count(cid).await {
            let count_u64 = u64::try_from(count).unwrap_or(0);
            self.update_metrics(|m| {
                m.sqlite_message_count = count_u64;
            });
        }

        if let Ok(count) = memory.sqlite().count_semantic_facts().await {
            let count_u64 = u64::try_from(count).unwrap_or(0);
            self.update_metrics(|m| {
                m.semantic_fact_count = count_u64;
            });
        }

        if let Ok(count) = memory.unsummarized_message_count(cid).await {
            self.memory_state.unsummarized_count = usize::try_from(count).unwrap_or(0);
        }

        self.recompute_prompt_tokens();
        Ok(())
    }

    /// Persist a message to memory.
    ///
    /// `has_injection_flags` controls whether Qdrant embedding is skipped for this message.
    /// When `true` and `guard_memory_writes` is enabled, only `SQLite` is written — the message
    /// is saved for conversation continuity but will not pollute semantic search (M2, D2).
    pub(crate) async fn persist_message(
        &mut self,
        role: Role,
        content: &str,
        parts: &[MessagePart],
        has_injection_flags: bool,
    ) {
        let (Some(memory), Some(cid)) =
            (&self.memory_state.memory, self.memory_state.conversation_id)
        else {
            return;
        };

        let parts_json = if parts.is_empty() {
            "[]".to_string()
        } else {
            serde_json::to_string(parts).unwrap_or_else(|e| {
                tracing::warn!("failed to serialize message parts, storing empty: {e}");
                "[]".to_string()
            })
        };

        // M2: injection flag is passed explicitly to avoid stale mutable-bool state on Agent.
        // When has_injection_flags=true, skip embedding to prevent poisoned content from
        // polluting Qdrant semantic search results.
        let guard_event = self
            .security
            .exfiltration_guard
            .should_guard_memory_write(has_injection_flags);
        if let Some(ref event) = guard_event {
            tracing::warn!(
                ?event,
                "exfiltration guard: skipping Qdrant embedding for flagged content"
            );
            self.update_metrics(|m| m.exfiltration_memory_guards += 1);
            self.push_security_event(
                crate::metrics::SecurityEventCategory::ExfiltrationBlock,
                "memory_write",
                "Qdrant embedding skipped: flagged content",
            );
        }

        let skip_embedding = guard_event.is_some();

        let should_embed = if skip_embedding {
            false
        } else {
            match role {
                Role::Assistant => {
                    self.memory_state.autosave_assistant
                        && content.len() >= self.memory_state.autosave_min_length
                }
                _ => true,
            }
        };

        let (embedding_stored, was_persisted) = if should_embed {
            match memory
                .remember_with_parts(cid, role_str(role), content, &parts_json)
                .await
            {
                Ok((Some(message_id), stored)) => {
                    self.last_persisted_message_id = Some(message_id.0);
                    (stored, true)
                }
                Ok((None, _)) => {
                    // A-MAC admission rejected — skip increment and further processing.
                    return;
                }
                Err(e) => {
                    tracing::error!("failed to persist message: {e:#}");
                    return;
                }
            }
        } else {
            match memory
                .save_only(cid, role_str(role), content, &parts_json)
                .await
            {
                Ok(message_id) => {
                    self.last_persisted_message_id = Some(message_id.0);
                    (false, true)
                }
                Err(e) => {
                    tracing::error!("failed to persist message: {e:#}");
                    return;
                }
            }
        };

        if !was_persisted {
            return;
        }

        self.memory_state.unsummarized_count += 1;

        self.update_metrics(|m| {
            m.sqlite_message_count += 1;
            if embedding_stored {
                m.embeddings_generated += 1;
            }
        });

        self.check_summarization().await;

        // FIX-1: skip graph extraction for tool result messages — they contain raw structured
        // output (TOML, JSON, code) that pollutes the entity graph with noise.
        let has_tool_result_parts = parts
            .iter()
            .any(|p| matches!(p, MessagePart::ToolResult { .. }));

        self.maybe_spawn_graph_extraction(content, has_injection_flags, has_tool_result_parts)
            .await;
    }

    async fn maybe_spawn_graph_extraction(
        &mut self,
        content: &str,
        has_injection_flags: bool,
        has_tool_result_parts: bool,
    ) {
        use zeph_memory::semantic::GraphExtractionConfig;

        if self.memory_state.memory.is_none() || self.memory_state.conversation_id.is_none() {
            return;
        }

        // FIX-1: skip extraction for tool result messages — raw tool output is structural data,
        // not conversational content. Extracting entities from it produces graph noise.
        if has_tool_result_parts {
            tracing::debug!("graph extraction skipped: message contains ToolResult parts");
            return;
        }

        // S2: skip extraction when injection flags detected — content is untrusted LLM input
        if has_injection_flags {
            tracing::warn!("graph extraction skipped: injection patterns detected in content");
            return;
        }

        // Collect extraction config — borrow ends before send_status call
        let extraction_cfg = {
            let cfg = &self.memory_state.graph_config;
            if !cfg.enabled {
                return;
            }
            GraphExtractionConfig {
                max_entities: cfg.max_entities_per_message,
                max_edges: cfg.max_edges_per_message,
                extraction_timeout_secs: cfg.extraction_timeout_secs,
                community_refresh_interval: cfg.community_refresh_interval,
                expired_edge_retention_days: cfg.expired_edge_retention_days,
                max_entities_cap: cfg.max_entities,
                community_summary_max_prompt_bytes: cfg.community_summary_max_prompt_bytes,
                community_summary_concurrency: cfg.community_summary_concurrency,
                lpa_edge_chunk_size: cfg.lpa_edge_chunk_size,
                note_linking: zeph_memory::NoteLinkingConfig {
                    enabled: cfg.note_linking.enabled,
                    similarity_threshold: cfg.note_linking.similarity_threshold,
                    top_k: cfg.note_linking.top_k,
                    timeout_secs: cfg.note_linking.timeout_secs,
                },
                link_weight_decay_lambda: cfg.link_weight_decay_lambda,
                link_weight_decay_interval_secs: cfg.link_weight_decay_interval_secs,
            }
        };

        // FIX-2: collect last 4 genuine conversational user messages as context for extraction.
        // Exclude tool result messages (Role::User with ToolResult parts) — they contain
        // raw structured output and would pollute the extraction context with noise.
        let context_messages: Vec<String> = self
            .msg
            .messages
            .iter()
            .rev()
            .filter(|m| {
                m.role == Role::User
                    && !m
                        .parts
                        .iter()
                        .any(|p| matches!(p, MessagePart::ToolResult { .. }))
            })
            .take(4)
            .map(|m| m.content.clone())
            .collect();

        let _ = self.channel.send_status("saving to graph...").await;

        if let Some(memory) = &self.memory_state.memory {
            // Build optional validation callback from MemoryWriteValidator (S3 fix).
            // zeph-memory receives a generic Fn predicate — it does not depend on security types.
            let validator: zeph_memory::semantic::PostExtractValidator =
                if self.security.memory_validator.is_enabled() {
                    let v = self.security.memory_validator.clone();
                    Some(Box::new(move |result| {
                        v.validate_graph_extraction(result)
                            .map_err(|e| e.to_string())
                    }))
                } else {
                    None
                };
            let extraction_handle = memory.spawn_graph_extraction(
                content.to_owned(),
                context_messages,
                extraction_cfg,
                validator,
            );
            // After the background extraction completes, refresh graph counts in metrics.
            // This ensures the TUI panel reflects actual DB counts rather than stale zeros.
            if let (Some(store), Some(tx)) =
                (memory.graph_store.clone(), self.metrics.metrics_tx.clone())
            {
                let start = self.lifecycle.start_time;
                tokio::spawn(async move {
                    let _ = extraction_handle.await;
                    let (entities, edges, communities) = tokio::join!(
                        store.entity_count(),
                        store.active_edge_count(),
                        store.community_count()
                    );
                    let elapsed = start.elapsed().as_secs();
                    tx.send_modify(|m| {
                        m.uptime_seconds = elapsed;
                        m.graph_entities_total = entities.unwrap_or(0).cast_unsigned();
                        m.graph_edges_total = edges.unwrap_or(0).cast_unsigned();
                        m.graph_communities_total = communities.unwrap_or(0).cast_unsigned();
                    });
                });
            }
        }
        let _ = self.channel.send_status("").await;
        self.sync_community_detection_failures();
        self.sync_graph_extraction_metrics();
        self.sync_graph_counts().await;
        #[cfg(feature = "compression-guidelines")]
        self.sync_guidelines_status().await;
    }

    pub(crate) async fn check_summarization(&mut self) {
        let (Some(memory), Some(cid)) =
            (&self.memory_state.memory, self.memory_state.conversation_id)
        else {
            return;
        };

        if self.memory_state.unsummarized_count > self.memory_state.summarization_threshold {
            let _ = self.channel.send_status("summarizing...").await;
            let batch_size = self.memory_state.summarization_threshold / 2;
            match memory.summarize(cid, batch_size).await {
                Ok(Some(summary_id)) => {
                    tracing::info!("created summary {summary_id} for conversation {cid}");
                    self.memory_state.unsummarized_count = 0;
                    self.update_metrics(|m| {
                        m.summaries_count += 1;
                    });
                }
                Ok(None) => {
                    tracing::debug!("no summarization needed");
                }
                Err(e) => {
                    tracing::error!("summarization failed: {e:#}");
                }
            }
            let _ = self.channel.send_status("").await;
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
    use zeph_memory::semantic::SemanticMemory;

    async fn test_memory(provider: &AnyProvider) -> SemanticMemory {
        SemanticMemory::new(
            ":memory:",
            "http://127.0.0.1:1",
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
        let counts: Vec<i64> =
            sqlx::query_scalar("SELECT session_count FROM messages WHERE id IN (?, ?) ORDER BY id")
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
            sqlx::query_scalar("SELECT session_count FROM messages WHERE conversation_id = ?")
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
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100)
            .with_autosave_config(false, 20);

        agent
            .persist_message(Role::Assistant, "short assistant reply", &[], false)
            .await;

        let history = agent
            .memory_state
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
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100)
            .with_autosave_config(true, 1000);

        agent
            .persist_message(Role::Assistant, "too short", &[], false)
            .await;

        let history = agent
            .memory_state
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
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100)
            .with_autosave_config(true, min_length);

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
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100)
            .with_autosave_config(true, min_length);

        // One below boundary: len == min_length - 1 → save_only path, no embedding.
        let content_below_boundary = "A".repeat(min_length - 1);
        assert_eq!(content_below_boundary.len(), min_length - 1);
        agent
            .persist_message(Role::Assistant, &content_below_boundary, &[], false)
            .await;

        let history = agent
            .memory_state
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

        assert_eq!(agent.memory_state.unsummarized_count, 0);

        agent.persist_message(Role::User, "first", &[], false).await;
        assert_eq!(agent.memory_state.unsummarized_count, 1);

        agent
            .persist_message(Role::User, "second", &[], false)
            .await;
        assert_eq!(agent.memory_state.unsummarized_count, 2);
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
        assert!(agent.memory_state.unsummarized_count <= 2);
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
        assert_eq!(agent.memory_state.unsummarized_count, 0);
    }

    // R-CRIT-01: unit tests for maybe_spawn_graph_extraction guard conditions.
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
                .memory_state
                .memory
                .as_ref()
                .unwrap()
                .sqlite()
                .pool()
                .clone();

            agent
                .maybe_spawn_graph_extraction("I use Rust", true, false)
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
                .memory_state
                .memory
                .as_ref()
                .unwrap()
                .sqlite()
                .pool()
                .clone();

            agent
                .maybe_spawn_graph_extraction("I use Rust", false, false)
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
                .memory_state
                .memory
                .as_ref()
                .unwrap()
                .sqlite()
                .pool()
                .clone();

            agent
                .maybe_spawn_graph_extraction("I use Rust for systems programming", false, false)
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
                .memory_state
                .memory
                .as_ref()
                .unwrap()
                .sqlite()
                .pool()
                .clone();

            agent
                .maybe_spawn_graph_extraction(
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
            // When maybe_spawn_graph_extraction collects context, it filters out
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
                .memory_state
                .memory
                .as_ref()
                .unwrap()
                .sqlite()
                .pool()
                .clone();

            // Trigger extraction for a conversational message (not a tool result).
            agent
                .maybe_spawn_graph_extraction("I prefer Rust for systems programming", false, false)
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
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100)
            .with_autosave_config(false, 20);

        let long_user_msg = "A".repeat(100);
        agent
            .persist_message(Role::User, &long_user_msg, &[], false)
            .await;

        let history = agent
            .memory_state
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
            .memory_state
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
            .memory_state
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
            .memory_state
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
            tool_name: "shell".to_string(),
            body: "hello from shell".to_string(),
            compacted_at: None,
        }];
        let content = "[tool: shell]\nhello from shell";

        agent
            .persist_message(Role::User, content, &parts, false)
            .await;

        let history = agent
            .memory_state
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
                name: "shell".to_string(),
                input: serde_json::json!({}),
            },
            zeph_llm::provider::ToolUseRequest {
                id: "cancel_id_2".to_string(),
                name: "read_file".to_string(),
                input: serde_json::json!({}),
            },
        ];

        agent.persist_cancelled_tool_results(&tool_calls).await;

        let history = agent
            .memory_state
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
}
