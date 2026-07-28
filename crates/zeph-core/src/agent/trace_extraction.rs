// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Post-session trace extraction hook (`AutoSkill A1`, spec 056).
//!
//! Fires after the agent's main loop exits. When enabled, collects user-role messages from
//! the conversation history, builds a [`TraceExtractor`], and runs it as a background task.
//! Idempotency is enforced via the `skill_trace_sessions` `SQLite` table.

use std::path::PathBuf;
use std::sync::Arc;

use zeph_llm::any::AnyProvider;
use zeph_llm::provider::Role;
use zeph_memory::semantic::SemanticMemory;
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
        if self.runtime.config.bare {
            return;
        }
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
        let memory = self.services.memory.persistence.memory.clone();

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
                    memory,
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
    memory: Option<Arc<SemanticMemory>>,
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
            log_and_persist(&session_id, &result, db_pool, memory).await;
        }
        Err(e) => {
            tracing::warn!(session_id = %session_id, error = %e, "trace_extraction: failed (session NOT marked as processed)");
        }
    }
}

/// Log the extraction summary, persist the idempotency row, and record each freshly
/// quarantined skill's `skill_trust` row at [`zeph_common::SkillTrustLevel::Quarantined`].
///
/// The trust write happens here — not in `zeph-skills` — because writing the draft `SKILL.md`
/// to `_quarantine/` is not itself a security boundary (see `SkillGenerator::write_quarantined`
/// doc comment): without this DB row, the next hot-reload's `update_trust_for_reloaded_skills`
/// finds no existing row for the new skill and falls back to `trust_cfg.default_level`, which
/// can silently trust an unreviewed draft (#6702).
async fn log_and_persist(
    session_id: &str,
    result: &TraceExtractionResult,
    db_pool: Option<zeph_db::DbPool>,
    memory: Option<Arc<SemanticMemory>>,
) {
    tracing::info!(
        session_id = %session_id,
        proposed = result.candidates_proposed,
        saved = result.candidates_saved,
        merged = result.candidates_merged,
        "trace_extraction: session complete"
    );
    if let Some(pool) = db_pool {
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

    let Some(memory) = memory else { return };
    for (skill_name, skill_md_path) in &result.saved_skill_paths {
        persist_quarantined_trust_row(session_id, &memory, skill_name, skill_md_path).await;
    }
}

/// Hash a single quarantined skill and (re)write its `skill_trust` row, unless doing so would
/// clobber the row of an unrelated skill that happens to share the name.
async fn persist_quarantined_trust_row(
    session_id: &str,
    memory: &SemanticMemory,
    skill_name: &str,
    skill_md_path: &std::path::Path,
) {
    let md_path = skill_md_path.to_path_buf();
    let Ok(Some((skill_dir, hash))) = tokio::task::spawn_blocking(move || {
        let skill_dir = md_path.parent().map(std::path::Path::to_path_buf)?;
        let hash = zeph_skills::compute_skill_hash(&skill_dir).ok()?;
        Some((skill_dir, hash))
    })
    .await
    else {
        tracing::warn!(
            session_id = %session_id,
            skill = %skill_name,
            "trace_extraction: failed to hash quarantined skill, trust row not written"
        );
        return;
    };
    // `source_path` stores the skill *directory*, matching the convention used by
    // `update_trust_for_reloaded_skills` (skill_reload.rs) — the two write sites must agree so
    // the column doesn't flip-flop between the eager write here and the next hot-reload
    // (#6702 M3).
    let source_path = skill_dir.to_str();
    if has_conflicting_trust_row(memory, skill_name, source_path).await {
        tracing::warn!(
            session_id = %session_id,
            skill = %skill_name,
            quarantine_source_path = ?source_path,
            "trace_extraction: name collision with an existing skill outside the quarantine \
             dir, skipping trust upsert to avoid clobbering it"
        );
        return;
    }
    // A failed write here means the exact #6702 bug recurs on the next reload with nothing
    // left to retry it (the draft is already on disk, quarantine-only). Retry once
    // synchronously before giving up — this is a local SQLite write, so a bounded retry is
    // cheap and covers transient lock contention (#6702 M1).
    let mut attempt = memory
        .sqlite()
        .upsert_skill_trust(
            skill_name,
            zeph_common::SkillTrustLevel::Quarantined,
            zeph_memory::store::SourceKind::Hub,
            None,
            source_path,
            &hash,
        )
        .await;
    if attempt.is_err() {
        attempt = memory
            .sqlite()
            .upsert_skill_trust(
                skill_name,
                zeph_common::SkillTrustLevel::Quarantined,
                zeph_memory::store::SourceKind::Hub,
                None,
                source_path,
                &hash,
            )
            .await;
    }
    if let Err(e) = attempt {
        tracing::error!(
            session_id = %session_id,
            skill = %skill_name,
            error = %e,
            "trace_extraction: failed to persist quarantined trust row after retry"
        );
    }
}

/// Whether `skill_name` already has a trust row owned by a *different* on-disk skill.
///
/// The merge prompt instructs the LLM to preserve the existing skill's name
/// (`merge_prompts.rs`), so `skill_name` here routinely collides with a Bundled or Trusted
/// skill living outside the quarantine dir. Blindly upserting would clobber that unrelated
/// skill's trust row, and the next hot-reload would demote it via `hash_mismatch_level`
/// (#6702 S1'). A row with no recorded `source_path` (legacy/manual rows) or one that already
/// points at `quarantine_source_path` (re-extracting the same evolving draft in place) is safe
/// to overwrite; only a row whose `source_path` names a different directory is a genuine
/// collision.
async fn has_conflicting_trust_row(
    memory: &SemanticMemory,
    skill_name: &str,
    quarantine_source_path: Option<&str>,
) -> bool {
    let Some(row) = memory
        .sqlite()
        .load_skill_trust(skill_name)
        .await
        .ok()
        .flatten()
    else {
        return false;
    };
    match row.source_path.as_deref() {
        None => false,
        existing => existing != quarantine_source_path,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zeph_memory::semantic::SemanticMemory;

    use super::*;

    async fn test_memory() -> Arc<SemanticMemory> {
        let provider = zeph_llm::any::AnyProvider::Mock(zeph_llm::mock::MockProvider::default());
        Arc::new(
            SemanticMemory::new(
                ":memory:",
                "http://127.0.0.1:1",
                None,
                provider,
                "test-model",
            )
            .await
            .unwrap(),
        )
    }

    /// Regression test for #6702: `log_and_persist` must eagerly write a `Quarantined`
    /// `skill_trust` row for every skill saved during extraction, so the very first
    /// hot-reload never has to fall back to `trust_cfg.default_level`.
    #[tokio::test]
    async fn log_and_persist_writes_quarantined_trust_row_for_saved_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("my-new-skill");
        tokio::fs::create_dir_all(&skill_dir).await.unwrap();
        let skill_md_path = skill_dir.join("SKILL.md");
        tokio::fs::write(
            &skill_md_path,
            "---\nname: my-new-skill\ndescription: A test skill.\nversion: 0\nsource: trace_extraction\n---\n\n## How to use\nDo the thing.\n",
        )
        .await
        .unwrap();

        let memory = test_memory().await;
        let result = TraceExtractionResult {
            saved_skill_paths: vec![("my-new-skill".to_string(), skill_md_path)],
            ..Default::default()
        };

        log_and_persist("test-session", &result, None, Some(memory.clone())).await;

        let row = memory
            .sqlite()
            .load_skill_trust("my-new-skill")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.trust_level, zeph_common::SkillTrustLevel::Quarantined);
        assert_eq!(row.source_kind, zeph_memory::store::SourceKind::Hub);
    }

    /// Spec 057 NEVER clause: a merge result written under a name that already carries a
    /// `Trusted` row (a human-reviewed skill, or a coincidental name reuse) must still be
    /// reset to `Quarantined` — trust must never be inherited from an existing row. Locks in
    /// the S1/S2 fix (`saved_skill_paths` keyed on the written name) end-to-end through
    /// `log_and_persist`. The existing row has no recorded `source_path`, which also exercises
    /// the "unknown provenance" branch of the S1' guard
    /// (`log_and_persist_skips_upsert_on_source_path_collision` below covers the "known,
    /// different directory" branch).
    #[tokio::test]
    async fn log_and_persist_resets_already_trusted_skill_to_quarantined() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("existing-skill-v2");
        tokio::fs::create_dir_all(&skill_dir).await.unwrap();
        let skill_md_path = skill_dir.join("SKILL.md");
        tokio::fs::write(
            &skill_md_path,
            "---\nname: existing-skill-v2\ndescription: A merged skill.\nversion: 1\nsource: trace_extraction\n---\n\n## How to use\nDo the merged thing.\n",
        )
        .await
        .unwrap();

        let memory = test_memory().await;
        memory
            .sqlite()
            .upsert_skill_trust(
                "existing-skill-v2",
                zeph_common::SkillTrustLevel::Trusted,
                zeph_memory::store::SourceKind::Local,
                None,
                None,
                "stale-hash",
            )
            .await
            .unwrap();

        let result = TraceExtractionResult {
            saved_skill_paths: vec![("existing-skill-v2".to_string(), skill_md_path)],
            ..Default::default()
        };

        log_and_persist("test-session", &result, None, Some(memory.clone())).await;

        let row = memory
            .sqlite()
            .load_skill_trust("existing-skill-v2")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.trust_level,
            zeph_common::SkillTrustLevel::Quarantined,
            "trust must never be inherited from an existing row (spec 057)"
        );
    }

    /// #6702 S1': the merge prompt instructs the LLM to preserve the existing skill's name, so
    /// `saved_skill_paths` routinely carries a name that already belongs to an unrelated skill
    /// living outside the quarantine dir (e.g. a Bundled skill). `log_and_persist` must detect
    /// the `source_path` mismatch and skip the upsert entirely, leaving that skill's real trust
    /// row untouched instead of clobbering it with quarantine metadata for an unrelated draft.
    #[tokio::test]
    async fn log_and_persist_skips_upsert_on_source_path_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let bundled_dir = tmp.path().join("bundled").join("existing-skill");
        let quarantine_dir = tmp.path().join("_quarantine").join("existing-skill");
        tokio::fs::create_dir_all(&quarantine_dir).await.unwrap();
        let skill_md_path = quarantine_dir.join("SKILL.md");
        tokio::fs::write(
            &skill_md_path,
            "---\nname: existing-skill\ndescription: A merged draft.\nversion: 1\nsource: trace_extraction\n---\n\n## How to use\nDo the merged thing.\n",
        )
        .await
        .unwrap();

        let memory = test_memory().await;
        memory
            .sqlite()
            .upsert_skill_trust(
                "existing-skill",
                zeph_common::SkillTrustLevel::Trusted,
                zeph_memory::store::SourceKind::Bundled,
                None,
                bundled_dir.to_str(),
                "bundled-hash",
            )
            .await
            .unwrap();

        let result = TraceExtractionResult {
            saved_skill_paths: vec![("existing-skill".to_string(), skill_md_path)],
            ..Default::default()
        };

        log_and_persist("test-session", &result, None, Some(memory.clone())).await;

        let row = memory
            .sqlite()
            .load_skill_trust("existing-skill")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.trust_level,
            zeph_common::SkillTrustLevel::Trusted,
            "a name collision with a skill outside the quarantine dir must not clobber its trust row"
        );
        assert_eq!(row.source_kind, zeph_memory::store::SourceKind::Bundled);
        assert_eq!(row.source_path.as_deref(), bundled_dir.to_str());
        assert_eq!(row.blake3_hash, "bundled-hash");
    }

    // Opens a migrated in-memory SQLite pool with the `skill_trace_sessions` table, mirroring
    // `shadow_sentinel.rs`'s `test_pool` helper.
    async fn test_pool() -> zeph_db::DbPool {
        zeph_db::DbConfig {
            url: ":memory:".to_owned(),
            ..Default::default()
        }
        .connect()
        .await
        .expect("connect + migrate in-memory sqlite pool")
    }

    /// When `memory` is `None` (no `SemanticMemory` configured), `log_and_persist` must still
    /// persist the `skill_trace_sessions` idempotency row — the trust-write loop and the
    /// session-row insert are structurally independent, so the absence of one must not silently
    /// drop the other.
    #[tokio::test]
    async fn log_and_persist_without_memory_still_persists_session_row() {
        let pool = test_pool().await;
        let result = TraceExtractionResult {
            candidates_proposed: 1,
            candidates_saved: 1,
            saved_skill_paths: vec![("orphan-skill".to_string(), PathBuf::from("/nonexistent"))],
            ..Default::default()
        };

        log_and_persist("test-session", &result, Some(pool.clone()), None).await;

        let count: i64 =
            zeph_db::query_scalar("SELECT COUNT(*) FROM skill_trace_sessions WHERE session_id = ?")
                .bind("test-session")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            count, 1,
            "session row must be persisted even without memory"
        );
    }
}
