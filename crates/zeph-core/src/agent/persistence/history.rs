// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Conversation history loading.
//!
//! [`Agent::load_history`] delegates the bulk of the work to
//! [`PersistenceService::load_history`] and applies the post-load mutations that touch
//! agent-internal singletons (session counts, semantic fact count, token recompute).

use super::super::Agent;
use crate::channel::Channel;
use zeph_agent_persistence::{LoadHistoryParams, MemoryPersistenceView, PersistenceService};

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
    pub async fn load_history(&mut self) -> Result<(), super::super::error::AgentError> {
        // Idempotency guard (spec-068, #5343): a session already hydrated from the durable JSONL
        // event log via `AgentBuilder::with_preloaded_messages` (see `spawn_acp_agent`,
        // `src/acp.rs`) must not also load from `SQLite` — `PersistenceService::load_history`
        // appends rather than replaces, so calling both would duplicate every message. Gated on
        // the explicit `history_preloaded` flag, not `messages.is_empty()`: `Agent::new` always
        // seeds `messages` with the system-prompt message, so emptiness never distinguishes
        // "already hydrated" from "not yet loaded."
        if self.msg.history_preloaded {
            return Ok(());
        }

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

        let svc = PersistenceService::new();
        let outcome = svc
            .load_history(LoadHistoryParams {
                messages: &mut self.msg.messages,
                last_persisted_message_id: &mut self.msg.last_persisted_message_id,
                deferred_hide_ids: &mut self.msg.deferred_db_hide_ids,
                memory_view: &memory_view,
            })
            .await
            .map_err(|e| {
                super::super::error::AgentError::Memory(zeph_memory::MemoryError::Other(
                    e.to_string(),
                ))
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

            // Resume banner for the `[session] enabled = false` `SQLite`-fallback path
            // (spec-068 §13.4, S1): the durable-log hydration path (`with_preloaded_messages`)
            // already short-circuits this whole function via the `history_preloaded` guard
            // above, so this can only run when that path was skipped — either the event-log
            // feature is disabled, or a legacy pre-#5343 conversation had no session row yet.
            // Spec §13.4 explicitly names this `PersistenceService::load_history` fallback as
            // an `is_resume` input; without this, resuming with `[session] enabled = false`
            // silently showed no banner despite genuinely resuming prior history.
            if self.runtime.config.resume_config.show_banner
                && !self.channel.requires_input_sanitization()
            {
                let resume_info = crate::session_resume::SessionResumeInfo::from_messages(
                    &self.msg.messages,
                    None,
                );
                if let Some(banner) = resume_info.banner_text() {
                    let _ = self.channel.send_resume_banner(&banner).await;
                }
            }
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

        // `PersistenceService::load_history` mutates `messages` through a borrowed
        // `&mut Vec<Message>` (part of `LoadHistoryParams`), so the non-system counter
        // (#6427) can't be updated inline at the mutation site — recompute it here,
        // mirroring the `recompute_prompt_tokens` call this already needed.
        self.msg.recompute_non_system_count();
        self.recompute_prompt_tokens();
        Ok(())
    }
}
