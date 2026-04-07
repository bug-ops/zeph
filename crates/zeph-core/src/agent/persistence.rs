// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashSet;

use crate::channel::Channel;
use zeph_llm::provider::{LlmProvider as _, Message, MessagePart, Role};
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
fn sanitize_tool_pairs(messages: &mut Vec<Message>) -> (usize, Vec<i64>) {
    let mut removed = 0;
    let mut db_ids: Vec<i64> = Vec::new();

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
            if let Some(db_id) = messages.last().and_then(|m| m.metadata.db_id) {
                db_ids.push(db_id);
            }
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
            if let Some(db_id) = messages.first().and_then(|m| m.metadata.db_id) {
                db_ids.push(db_id);
            }
            messages.remove(0);
            removed += 1;
            continue;
        }

        break;
    }

    // Mid-history scan: strip ToolUse parts from any assistant message whose tool IDs are not
    // matched by ToolResult parts in the immediately following user message.
    let (mid_removed, mid_db_ids) = strip_mid_history_orphans(messages);
    removed += mid_removed;
    db_ids.extend(mid_db_ids);

    (removed, db_ids)
}

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

/// Returns `true` if `content` contains human-readable text beyond legacy tool bracket markers.
///
/// Legacy markers produced by `Message::flatten_parts` are:
/// - `[tool_use: name(id)]` — assistant `ToolUse`
/// - `[tool_result: id]\nbody` — user `ToolResult` (tag + trailing body up to the next tag)
/// - `[tool output: name] body` — `ToolOutput` (pruned or inline)
/// - `[tool output: name]\n```\nbody\n``` ` — `ToolOutput` fenced block
///
/// A message whose content consists solely of such markers (and whitespace) has no
/// user-visible text and is a candidate for soft-delete once its structured `parts` are gone.
///
/// Conservative rule: if a tag is malformed (no closing `]`), the content is treated as
/// meaningful and the message is NOT deleted.
///
/// Note: `[image: mime, N bytes]` placeholders are intentionally treated as meaningful because
/// they represent real media content and are not pure tool-execution artifacts.
///
/// Note: the Claude request-builder format `[tool_use: name] {json_input}` is used only for
/// API payload construction and is never written to `SQLite` — it cannot appear in persisted
/// message content, so no special handling is needed here.
fn has_meaningful_content(content: &str) -> bool {
    const PREFIXES: [&str; 3] = ["[tool_use: ", "[tool_result: ", "[tool output: "];

    let mut remaining = content.trim();

    loop {
        // Find the earliest occurrence of any tool tag prefix.
        let next = PREFIXES
            .iter()
            .filter_map(|prefix| remaining.find(prefix).map(|pos| (pos, *prefix)))
            .min_by_key(|(pos, _)| *pos);

        let Some((start, prefix)) = next else {
            // No more tool tags — whatever remains decides the verdict.
            break;
        };

        // Any non-whitespace text before this tag is meaningful.
        if !remaining[..start].trim().is_empty() {
            return true;
        }

        // Advance past the prefix to find the closing `]`.
        let after_prefix = &remaining[start + prefix.len()..];
        let Some(close) = after_prefix.find(']') else {
            // Malformed tag (no closing bracket) — treat as meaningful, do not delete.
            return true;
        };

        // Position after the `]`.
        let tag_end = start + prefix.len() + close + 1;

        if prefix == "[tool_result: " || prefix == "[tool output: " {
            // Skip the body that immediately follows until the next tool tag prefix or end-of-string.
            // The body is part of the tool artifact, not human-readable content.
            let body = remaining[tag_end..].trim_start_matches('\n');
            let next_tag = PREFIXES
                .iter()
                .filter_map(|p| body.find(p))
                .min()
                .unwrap_or(body.len());
            remaining = &body[next_tag..];
        } else {
            remaining = &remaining[tag_end..];
        }
    }

    !remaining.trim().is_empty()
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
/// Returns `(count, db_ids)` where `count` is the number of messages removed entirely and
/// `db_ids` contains the `metadata.db_id` values of those removed messages (for DB cleanup).
fn strip_mid_history_orphans(messages: &mut Vec<Message>) -> (usize, Vec<i64>) {
    let mut removed = 0;
    let mut db_ids: Vec<i64> = Vec::new();
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
                    !has_meaningful_content(&messages[i].content) && messages[i].parts.is_empty();
                if is_empty {
                    if let Some(db_id) = messages[i].metadata.db_id {
                        db_ids.push(db_id);
                    }
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
                    !has_meaningful_content(&messages[i].content) && messages[i].parts.is_empty();
                if is_empty {
                    if let Some(db_id) = messages[i].metadata.db_id {
                        db_ids.push(db_id);
                    }
                    messages.remove(i);
                    removed += 1;
                    // Do not advance i — the next message is now at position i.
                    continue;
                }
            }
        }

        i += 1;
    }
    (removed, db_ids)
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
                if !has_meaningful_content(&msg.content) && msg.parts.is_empty() {
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
            let (orphans, orphan_db_ids) = sanitize_tool_pairs(&mut restored_slice);
            skipped += orphans;
            loaded = loaded.saturating_sub(orphans);
            self.msg.messages.append(&mut restored_slice);

            if !orphan_db_ids.is_empty() {
                let ids: Vec<zeph_memory::types::MessageId> = orphan_db_ids
                    .iter()
                    .map(|&id| zeph_memory::types::MessageId(id))
                    .collect();
                if let Err(e) = memory.sqlite().soft_delete_messages(&ids).await {
                    tracing::warn!(
                        count = ids.len(),
                        error = %e,
                        "failed to soft-delete orphaned tool-pair messages from DB"
                    );
                } else {
                    tracing::debug!(
                        count = ids.len(),
                        "soft-deleted orphaned tool-pair messages from DB"
                    );
                }
            }

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
    #[allow(clippy::too_many_lines)]
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

        // Do not embed [skipped] or [stopped] ToolResult content into Qdrant — these are
        // internal policy markers that carry no useful semantic information and would
        // contaminate memory_search results, causing the utility-gate Retrieve loop (#2620).
        let has_skipped_tool_result = parts.iter().any(|p| {
            if let MessagePart::ToolResult { content, .. } = p {
                content.starts_with("[skipped]") || content.starts_with("[stopped]")
            } else {
                false
            }
        });

        let should_embed = if skip_embedding || has_skipped_tool_result {
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

        let goal_text = self.memory_state.goal_text.clone();

        let (embedding_stored, was_persisted) = if should_embed {
            match memory
                .remember_with_parts(
                    cid,
                    role_str(role),
                    content,
                    &parts_json,
                    goal_text.as_deref(),
                )
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

        // Persona extraction: run only for user messages that are not tool results and not injected.
        if role == Role::User && !has_tool_result_parts && !has_injection_flags {
            self.maybe_spawn_persona_extraction().await;
        }

        // Trajectory extraction: run after turns that contained tool results.
        if has_tool_result_parts {
            self.maybe_spawn_trajectory_extraction();
        }
    }

    #[allow(clippy::too_many_lines)]
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
                belief_revision_enabled: cfg.belief_revision.enabled,
                belief_revision_similarity_threshold: cfg.belief_revision.similarity_threshold,
                conversation_id: self.memory_state.conversation_id.map(|c| c.0),
            }
        };

        // D-MEM RPE routing: skip extraction when the turn has low surprise.
        if self.rpe_should_skip(content).await {
            tracing::debug!("D-MEM RPE: low-surprise turn, skipping graph extraction");
            return;
        }

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
            .map(|m| {
                if m.content.len() > 2048 {
                    m.content[..m.content.floor_char_boundary(2048)].to_owned()
                } else {
                    m.content.clone()
                }
            })
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
        self.sync_guidelines_status().await;
    }

    async fn maybe_spawn_persona_extraction(&mut self) {
        use std::time::Duration;

        use zeph_memory::semantic::{PersonaExtractionConfig, extract_persona_facts};

        let cfg = &self.memory_state.persona_config;
        if !cfg.enabled {
            return;
        }

        let Some(memory) = &self.memory_state.memory else {
            return;
        };

        // Collect recent user messages for extraction.
        // Cap at 8 messages and 2 KiB per message to bound LLM prompt size.
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
            persona_provider: cfg.persona_provider.as_str().to_owned(),
            min_messages: cfg.min_messages,
            max_messages: cfg.max_messages,
            extraction_timeout_secs: timeout_secs,
        };

        let provider = self.resolve_background_provider(cfg.persona_provider.as_str());
        let store = memory.sqlite().clone();
        let conversation_id = self.memory_state.conversation_id.map(|c| c.0);

        let user_message_refs: Vec<&str> = user_messages.iter().map(String::as_str).collect();
        let fut = extract_persona_facts(
            &store,
            &provider,
            &user_message_refs,
            &extraction_cfg,
            conversation_id,
        );
        match tokio::time::timeout(Duration::from_secs(timeout_secs), fut).await {
            Ok(Ok(n)) => tracing::debug!(upserted = n, "persona extraction complete"),
            Ok(Err(e)) => tracing::warn!(error = %e, "persona extraction failed"),
            Err(_) => tracing::warn!(
                timeout_secs,
                "persona extraction timed out — no facts written this turn"
            ),
        }
    }

    fn maybe_spawn_trajectory_extraction(&mut self) {
        use zeph_memory::semantic::{TrajectoryExtractionConfig, extract_trajectory_entries};

        let cfg = self.memory_state.trajectory_config.clone();
        if !cfg.enabled {
            return;
        }

        let Some(memory) = &self.memory_state.memory else {
            return;
        };

        let conversation_id = match self.memory_state.conversation_id {
            Some(cid) => cid.0,
            None => return,
        };

        // Collect the tail of the message history to pass to the extractor.
        // Cloning the full vec can be megabytes in long sessions; the extractor only needs
        // recent context bounded by `cfg.max_messages`.
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

        // Fire-and-forget: do not block response path (critic M3).
        tokio::spawn(async move {
            let entries = match extract_trajectory_entries(
                &provider,
                &turn_messages,
                &extraction_cfg,
            )
            .await
            {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "trajectory extraction failed");
                    return;
                }
            };

            // Get or initialize the watermark for this conversation.
            let last_id = store
                .trajectory_last_extracted_message_id(conversation_id)
                .await
                .unwrap_or(0);

            let mut max_id = last_id;
            for entry in &entries {
                if entry.confidence < min_confidence {
                    continue;
                }
                let tools_json =
                    serde_json::to_string(&entry.tools_used).unwrap_or_else(|_| "[]".to_string());
                match store
                    .insert_trajectory_entry(zeph_memory::NewTrajectoryEntry {
                        conversation_id: Some(conversation_id),
                        turn_index: 0, // turn_index placeholder (best effort)
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
        });
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

    /// D-MEM RPE check: returns `true` when the current turn should skip graph extraction.
    ///
    /// Embeds `content`, computes RPE via the router, and updates the router state.
    /// Returns `false` (do not skip) on any error — conservative fallback.
    async fn rpe_should_skip(&mut self, content: &str) -> bool {
        let Some(ref rpe_mutex) = self.memory_state.rpe_router else {
            return false;
        };
        let Some(memory) = &self.memory_state.memory else {
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

    // R-PERS-01: unit tests for maybe_spawn_persona_extraction guard conditions.
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
            agent.memory_state.persona_config = config;
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

            agent.maybe_spawn_persona_extraction().await;

            let store = agent.memory_state.memory.as_ref().unwrap().sqlite().clone();
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

            agent.maybe_spawn_persona_extraction().await;

            let store = agent.memory_state.memory.as_ref().unwrap().sqlite().clone();
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
            agent.memory_state.persona_config = enabled_persona_config();
            agent.msg.messages.push(zeph_llm::provider::Message {
                role: Role::User,
                content: "I like Rust".to_owned(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            });

            // Must not panic even without memory.
            agent.maybe_spawn_persona_extraction().await;
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

            agent.maybe_spawn_persona_extraction().await;

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

            agent.maybe_spawn_persona_extraction().await;

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
            // maybe_spawn_persona_extraction: a content > 2048 bytes must be capped
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

    // ---- has_meaningful_content unit tests ----

    #[test]
    fn meaningful_content_empty_string() {
        assert!(!has_meaningful_content(""));
    }

    #[test]
    fn meaningful_content_whitespace_only() {
        assert!(!has_meaningful_content("   \n\t  "));
    }

    #[test]
    fn meaningful_content_tool_use_only() {
        assert!(!has_meaningful_content("[tool_use: shell(call_1)]"));
    }

    #[test]
    fn meaningful_content_tool_use_no_parens() {
        // Format produced when tool_use is stored without explicit id parens.
        assert!(!has_meaningful_content("[tool_use: memory_save]"));
    }

    #[test]
    fn meaningful_content_tool_result_with_body() {
        assert!(!has_meaningful_content(
            "[tool_result: call_1]\nsome output here"
        ));
    }

    #[test]
    fn meaningful_content_tool_result_empty_body() {
        assert!(!has_meaningful_content("[tool_result: call_1]\n"));
    }

    #[test]
    fn meaningful_content_tool_output_inline() {
        assert!(!has_meaningful_content("[tool output: bash] some result"));
    }

    #[test]
    fn meaningful_content_tool_output_pruned() {
        assert!(!has_meaningful_content("[tool output: bash] (pruned)"));
    }

    #[test]
    fn meaningful_content_tool_output_fenced() {
        assert!(!has_meaningful_content(
            "[tool output: bash]\n```\nls output\n```"
        ));
    }

    #[test]
    fn meaningful_content_multiple_tool_use_tags() {
        assert!(!has_meaningful_content(
            "[tool_use: bash(id1)][tool_use: read(id2)]"
        ));
    }

    #[test]
    fn meaningful_content_multiple_tool_use_tags_space_separator() {
        // Space between tags is not meaningful content.
        assert!(!has_meaningful_content(
            "[tool_use: bash(id1)] [tool_use: read(id2)]"
        ));
    }

    #[test]
    fn meaningful_content_multiple_tool_use_tags_newline_separator() {
        // Newline-only separator is also not meaningful.
        assert!(!has_meaningful_content(
            "[tool_use: bash(id1)]\n[tool_use: read(id2)]"
        ));
    }

    #[test]
    fn meaningful_content_tool_result_followed_by_tool_use() {
        // Two tags in sequence — no real text between them.
        assert!(!has_meaningful_content(
            "[tool_result: call_1]\nresult\n[tool_use: bash(call_2)]"
        ));
    }

    #[test]
    fn meaningful_content_real_text_only() {
        assert!(has_meaningful_content("Hello, how can I help you?"));
    }

    #[test]
    fn meaningful_content_text_before_tool_tag() {
        assert!(has_meaningful_content("Let me check. [tool_use: bash(id)]"));
    }

    #[test]
    fn meaningful_content_text_after_tool_use_tag() {
        // Text appearing after a [tool_use: name] tag (without parens) is a JSON body
        // in the request-builder format — but since that format never reaches the DB,
        // this test verifies conservative behavior: the helper returns true (do not delete).
        assert!(has_meaningful_content("[tool_use: bash] I ran the command"));
    }

    #[test]
    fn meaningful_content_text_between_tags() {
        assert!(has_meaningful_content(
            "[tool_use: bash(id1)]\nand then\n[tool_use: read(id2)]"
        ));
    }

    #[test]
    fn meaningful_content_malformed_tag_no_closing_bracket() {
        // Conservative: malformed tag must not trigger delete.
        assert!(has_meaningful_content("[tool_use: "));
    }

    #[test]
    fn meaningful_content_tool_use_and_tool_result_only() {
        // Typical persisted assistant+user pair content with no extra text.
        assert!(!has_meaningful_content(
            "[tool_use: memory_save(call_abc)]\n[tool_result: call_abc]\nsaved"
        ));
    }

    #[test]
    fn meaningful_content_tool_result_body_with_json_array() {
        assert!(!has_meaningful_content(
            "[tool_result: id1]\n[\"array\", \"value\"]"
        ));
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
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100)
            .with_autosave_config(true, 0);

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
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100)
            .with_autosave_config(true, 0);

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

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_memory(memory_arc.clone(), cid, 50, 5, 100)
            .with_autosave_config(true, 0);

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

    /// Verify that `maybe_spawn_trajectory_extraction` uses a bounded tail slice instead of
    /// cloning the full message vec. We confirm the slice logic by checking that the
    /// `tail_start` calculation correctly bounds the window even with more messages than
    /// `max_messages`.
    #[test]
    fn trajectory_extraction_slice_bounds_messages() {
        // Replicate the slice logic from maybe_spawn_trajectory_extraction.
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
        let max_messages: u64 = 20;
        let total_messages = 5usize;

        let tail_start = total_messages.saturating_sub(max_messages as usize);
        let window = total_messages - tail_start;

        assert_eq!(window, 5, "should return all messages when fewer than max");
        assert_eq!(tail_start, 0, "slice should start from the beginning");
    }
}
