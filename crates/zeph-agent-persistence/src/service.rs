// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`PersistenceService`] — stateless façade for agent message persistence.

use zeph_llm::provider::{Message, MessagePart, Role};

use crate::embed::{serialize_parts_json, should_embed_message, write_message_to_memory};
use crate::error::PersistenceError;
use crate::request::{LoadHistoryOutcome, PersistMessageOutcome, PersistMessageRequest};
use crate::sanitize::sanitize_tool_pairs;
use crate::state::{MemoryPersistenceView, MetricsView, SecurityView};

/// Stateless façade for agent message persistence operations.
///
/// This struct has no fields. All inputs flow through method parameters, which allows the
/// borrow checker to see disjoint `&mut` borrows at the call site without hiding them inside
/// an opaque bundle.
///
/// Methods are `&self` — the type exists only to namespace the operations and to give callers
/// a single import.
///
/// # Examples
///
/// ```no_run
/// use zeph_agent_persistence::service::PersistenceService;
///
/// let svc = PersistenceService::new();
/// // call svc.persist_message(...) or svc.load_history(...)
/// ```
#[derive(Debug, Default)]
pub struct PersistenceService;

impl PersistenceService {
    /// Create a new stateless `PersistenceService`.
    ///
    /// This is a zero-cost constructor — the struct has no fields.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Load conversation history from `SemanticMemory` into `messages`.
    ///
    /// Sanitizes orphaned tool-use/tool-result pairs and soft-deletes their `SQLite` rows.
    /// Updates `last_persisted_message_id` and populates `deferred_hide_ids` with IDs to be
    /// soft-deleted in a subsequent call.
    ///
    /// Returns counts and `SQLite`/semantic totals.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError`] if `SQLite` fails during history load or soft-delete.
    #[allow(clippy::too_many_arguments)]
    pub async fn load_history(
        &self,
        messages: &mut Vec<Message>,
        last_persisted_message_id: &mut Option<i64>,
        deferred_hide_ids: &mut Vec<i64>,
        _deferred_summaries: &mut Vec<String>,
        memory_view: &MemoryPersistenceView<'_>,
        _config: &zeph_config::Config,
        _metrics: &mut MetricsView<'_>,
    ) -> Result<LoadHistoryOutcome, PersistenceError> {
        let (Some(memory), Some(cid)) = (memory_view.memory, memory_view.conversation_id) else {
            return Ok(LoadHistoryOutcome {
                messages_loaded: 0,
                orphan_pairs_removed: 0,
                deferred_hide_ids_count: 0,
                sqlite_total_messages: 0,
                semantic_total_messages: 0,
            });
        };

        let history = memory
            .sqlite()
            .load_history_filtered(cid, 50, Some(true), None)
            .await?;

        if history.is_empty() {
            return Ok(LoadHistoryOutcome {
                messages_loaded: 0,
                orphan_pairs_removed: 0,
                deferred_hide_ids_count: 0,
                sqlite_total_messages: 0,
                semantic_total_messages: 0,
            });
        }

        let mut loaded = 0;
        let mut skipped = 0;

        for msg in history {
            use crate::sanitize::has_meaningful_content;
            if !has_meaningful_content(&msg.content) && msg.parts.is_empty() {
                tracing::warn!("skipping empty message from history (role: {:?})", msg.role);
                skipped += 1;
                continue;
            }
            messages.push(msg);
            loaded += 1;
        }

        let history_start = messages.len() - loaded;
        let mut restored_slice = messages.split_off(history_start);
        let (orphans, orphan_db_ids) = sanitize_tool_pairs(&mut restored_slice);
        skipped += orphans;
        let loaded = loaded.saturating_sub(orphans);
        messages.append(&mut restored_slice);

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
                deferred_hide_ids.extend(orphan_db_ids.iter().copied());
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

        // Update last_persisted_message_id from the last loaded message.
        if let Some(db_id) = messages.last().and_then(|m| m.metadata.db_id) {
            *last_persisted_message_id = Some(db_id);
        }

        let sqlite_total = memory.message_count(cid).await.unwrap_or(0);
        let sqlite_total_u64 = u64::try_from(sqlite_total).unwrap_or(0);
        let semantic_total = memory.sqlite().count_semantic_facts().await.unwrap_or(0);
        let semantic_total_u64 = u64::try_from(semantic_total).unwrap_or(0);

        Ok(LoadHistoryOutcome {
            messages_loaded: loaded,
            orphan_pairs_removed: orphans,
            deferred_hide_ids_count: deferred_hide_ids.len(),
            sqlite_total_messages: sqlite_total_u64,
            semantic_total_messages: semantic_total_u64,
        })
    }

    /// Persist exactly one message to `SemanticMemory`.
    ///
    /// Embedding is decided by `request.has_injection_flags` AND `security.guard_memory_writes`.
    /// Updates `last_persisted_message_id`, `memory_view.unsummarized_count`, and metrics counters.
    ///
    /// Returns a [`PersistMessageOutcome`] even on write failure (the outcome's `message_id`
    /// will be `None`). Callers should log failures but continue — conversation continuity
    /// is more important than individual message persistence.
    pub async fn persist_message(
        &self,
        request: PersistMessageRequest,
        last_persisted_message_id: &mut Option<i64>,
        memory_view: &mut MemoryPersistenceView<'_>,
        security: &SecurityView<'_>,
        _config: &zeph_config::Config,
        metrics: &mut MetricsView<'_>,
    ) -> PersistMessageOutcome {
        let (Some(memory), Some(cid)) = (memory_view.memory, memory_view.conversation_id) else {
            return PersistMessageOutcome {
                message_id: None,
                embedded: false,
                redaction_applied: false,
                bytes_written: 0,
            };
        };

        let Some(parts_json) = serialize_parts_json(&request.parts, request.role) else {
            return PersistMessageOutcome {
                message_id: None,
                embedded: false,
                redaction_applied: false,
                bytes_written: 0,
            };
        };

        let guard_active = security.guard_memory_writes && request.has_injection_flags;
        if guard_active {
            tracing::warn!("exfiltration guard: skipping Qdrant embedding for flagged content");
            *metrics.exfiltration_memory_guards += 1;
        }

        let should_embed = should_embed_message(
            guard_active,
            &request.parts,
            request.role,
            memory_view.autosave_assistant,
            memory_view.autosave_min_length,
            request.content.len(),
        );

        let goal_text = memory_view.goal_text.clone();

        tracing::debug!(
            "persist_message: calling remember_with_parts, embed dispatched to background"
        );

        let Some((embedding_stored, message_id)) = write_message_to_memory(
            memory,
            cid,
            request.role,
            &request.content,
            &parts_json,
            goal_text.as_deref(),
            should_embed,
        )
        .await
        else {
            return PersistMessageOutcome {
                message_id: None,
                embedded: false,
                redaction_applied: false,
                bytes_written: 0,
            };
        };

        *last_persisted_message_id = Some(message_id);
        *memory_view.unsummarized_count += 1;

        *metrics.sqlite_message_count += 1;
        if embedding_stored {
            *metrics.embeddings_generated += 1;
        }

        tracing::debug!("persist_message: db insert complete, embedding running in background");
        memory.reap_embed_tasks();

        PersistMessageOutcome {
            message_id: Some(message_id),
            embedded: embedding_stored,
            redaction_applied: false,
            bytes_written: request.content.len() + parts_json.len(),
        }
    }
}

/// Trait allowing `zeph-core` to provide a thin bridge type for parts that the
/// `PersistenceService` needs from the agent's `SecurityState`.
///
/// Callers build a [`SecurityView`] from their own private types — this trait is
/// a convenience for the construction pattern.
pub trait IntoSecurityView {
    /// Convert `self` into a [`SecurityView`] borrow-lens.
    fn as_security_view(&self) -> SecurityView<'_>;
}

/// Trait allowing `zeph-core` to provide a thin bridge type for the metrics state.
pub trait IntoMetricsView {
    /// Convert `self` into a [`MetricsView`] borrow-lens.
    fn as_metrics_view(&mut self) -> MetricsView<'_>;
}

// Convenience so callers can drop role/parts into a request with minimal syntax.
impl PersistMessageRequest {
    /// Build from a role string as used by the legacy `persist_message` signature.
    #[must_use]
    pub fn from_role_parts(
        role: Role,
        content: &str,
        parts: &[MessagePart],
        has_injection_flags: bool,
    ) -> Self {
        Self::from_borrowed(role, content, parts, has_injection_flags)
    }
}
