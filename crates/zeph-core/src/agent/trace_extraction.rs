// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Post-session trace extraction hook (`AutoSkill A1`, spec 056).
//!
//! Fires after the agent's main loop exits. When enabled, collects user-role messages from
//! the conversation history, builds a [`TraceExtractor`], and runs it as a background task.
//! Idempotency is enforced via the `skill_trace_sessions` `SQLite` table.

use std::path::PathBuf;

use zeph_llm::any::AnyProvider;
use zeph_llm::provider::Role;
use zeph_skills::loader::SkillMeta;
use zeph_skills::trace_extractor::{
    TraceExtractionResult, TraceExtractor, UserMessage, session_record,
};

use crate::agent::Channel;

impl<C: Channel> super::Agent<C> {
    /// Spawn a background trace extraction task if configured and not yet processed.
    ///
    /// Reads `[skills.learning]` config via `learning_engine.config`. When
    /// `trace_extraction_enabled = false`, returns immediately without spawning a task.
    pub(super) async fn maybe_extract_skills_from_trace(&mut self) {
        let Some(ref learning_cfg) = self.services.learning_engine.config else {
            return;
        };
        if !learning_cfg.trace_extraction_enabled {
            return;
        }

        let Some(conv) = self.services.memory.persistence.conversation_id else {
            tracing::debug!("trace_extraction: no conversation_id, skipping");
            return;
        };
        let conversation_id = conv.0.to_string();

        if self.session_already_extracted(&conversation_id).await {
            tracing::debug!(session_id = %conversation_id, "trace_extraction: session already extracted, skipping");
            return;
        }

        let user_messages: Vec<UserMessage> = self
            .msg
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .map(|m| UserMessage {
                text: m.to_llm_content().to_string(),
            })
            .collect();

        if user_messages.is_empty() {
            tracing::debug!(session_id = %conversation_id, "trace_extraction: no user messages, skipping");
            return;
        }

        let extract_provider =
            self.resolve_background_provider(learning_cfg.trace_extraction_provider.as_str());
        let embed_provider = self
            .resolve_background_provider(learning_cfg.trace_extraction_embedding_provider.as_str());

        let Some(ref output_dir) = self.services.skill.managed_dir else {
            tracing::debug!("trace_extraction: no managed_dir configured, skipping");
            return;
        };
        let output_dir = output_dir.clone();

        let max_turns = learning_cfg.trace_extraction_max_turns;
        let max_input_bytes = learning_cfg.trace_extraction_max_input_bytes;
        let merge_threshold = learning_cfg.merge_threshold;
        let merge_enabled = learning_cfg.skill_merge_enabled;
        let dedup_threshold = learning_cfg.dedup_threshold;

        let existing_meta: Vec<SkillMeta> = self
            .services
            .skill
            .registry
            .read()
            .all_meta()
            .iter()
            .copied()
            .cloned()
            .collect();

        let status_tx = self.services.session.status_tx.clone();
        let db_pool = self.get_db_pool_for_trace_extraction();

        let _ = self
            .services
            .session
            .status_tx
            .as_ref()
            .map(|tx| tx.send("Extracting skills from session…".into()));

        let blocking_handle = self.runtime.lifecycle.task_supervisor.spawn_oneshot(
            std::sync::Arc::from("agent.learning.trace_extraction"),
            move || {
                run_extraction(
                    extract_provider,
                    embed_provider,
                    output_dir,
                    max_turns,
                    max_input_bytes,
                    merge_threshold,
                    dedup_threshold,
                    merge_enabled,
                    existing_meta,
                    user_messages,
                    conversation_id,
                    db_pool,
                    status_tx,
                )
            },
        );
        self.services.learning_engine.trace_extraction_handle = Some(blocking_handle);
    }

    /// Check whether `session_id` already exists in `skill_trace_sessions`.
    async fn session_already_extracted(&self, session_id: &str) -> bool {
        let Some(pool) = self.get_db_pool_for_trace_extraction() else {
            return false;
        };
        let count: i64 =
            zeph_db::query_scalar("SELECT COUNT(*) FROM skill_trace_sessions WHERE session_id = ?")
                .bind(session_id)
                .fetch_one(&pool)
                .await
                .unwrap_or(0);
        count > 0
    }

    /// Get the DB pool for idempotency writes.
    fn get_db_pool_for_trace_extraction(&self) -> Option<zeph_db::DbPool> {
        self.services
            .memory
            .persistence
            .memory
            .as_ref()
            .map(|m| m.sqlite().pool().clone())
    }
}

/// Background task: build extractor, embed existing skills, run extraction, write idempotency row.
#[allow(clippy::too_many_arguments)]
async fn run_extraction(
    extract_provider: AnyProvider,
    embed_provider: AnyProvider,
    output_dir: PathBuf,
    max_turns: u32,
    max_input_bytes: usize,
    merge_threshold: f32,
    dedup_threshold: f32,
    merge_enabled: bool,
    existing_meta: Vec<SkillMeta>,
    user_messages: Vec<UserMessage>,
    session_id: String,
    db_pool: Option<zeph_db::DbPool>,
    status_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
) {
    let extractor = TraceExtractor::new(
        extract_provider,
        embed_provider,
        output_dir,
        max_turns,
        max_input_bytes,
        merge_threshold,
        dedup_threshold,
        merge_enabled,
        status_tx,
    );
    let existing_embeddings = extractor.embed_existing(&existing_meta).await;
    match extractor
        .extract_from_trace(&user_messages, &existing_embeddings, &session_id)
        .await
    {
        Ok(result) => {
            log_and_persist(&session_id, &result, db_pool).await;
        }
        Err(e) => {
            tracing::warn!(session_id = %session_id, error = %e, "trace_extraction: failed (session NOT marked as processed)");
        }
    }
}

async fn log_and_persist(
    session_id: &str,
    result: &TraceExtractionResult,
    db_pool: Option<zeph_db::DbPool>,
) {
    tracing::info!(
        session_id = %session_id,
        proposed = result.candidates_proposed,
        saved = result.candidates_saved,
        merged = result.candidates_merged,
        "trace_extraction: session complete"
    );
    let Some(pool) = db_pool else { return };
    let (sid, ts, proposed, saved, merged) = session_record(session_id, result);
    let _ = zeph_db::query(
        "INSERT OR IGNORE INTO skill_trace_sessions \
         (session_id, processed_at, candidates_proposed, candidates_saved, candidates_merged) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(sid)
    .bind(ts)
    .bind(proposed)
    .bind(saved)
    .bind(merged)
    .execute(&pool)
    .await
    .map_err(|e| {
        tracing::warn!(error = %e, "trace_extraction: failed to write idempotency row");
    });
}
