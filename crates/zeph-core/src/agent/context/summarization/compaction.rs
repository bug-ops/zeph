// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::ops::ControlFlow;

use zeph_llm::provider::{Message, MessageMetadata, Role};

use crate::agent::Agent;
use crate::agent::context::CompactionOutcome;
use crate::channel::Channel;

/// Partitioned message slices produced by [`Agent::partition_messages_for_compaction`].
struct CompactionPartition {
    /// Focus-pinned knowledge block messages that survive compaction.
    pinned_messages: Vec<Message>,
    /// Active-subgoal messages protected from summarization (only populated when a subgoal
    /// strategy is active).
    active_subgoal_messages: Vec<Message>,
    /// Messages to be summarized by the LLM.
    to_compact: Vec<Message>,
}

impl<C: Channel> Agent<C> {
    /// Partition the compaction range into pinned, active-subgoal, and to-compact slices.
    ///
    /// Extracts `messages[1..compact_end]` into three disjoint sets so that focus-pinned
    /// and active-subgoal messages survive compaction without being fed to the LLM.
    fn partition_messages_for_compaction(&self, compact_end: usize) -> CompactionPartition {
        // S1 fix: extract focus-pinned messages before draining so they survive compaction.
        // These are Knowledge block messages created by the Focus Agent (#1850).
        let pinned_messages: Vec<Message> = self.msg.messages[1..compact_end]
            .iter()
            .filter(|m| m.metadata.focus_pinned)
            .cloned()
            .collect();

        // S2 fix (#2022): extract active-subgoal messages before draining so they survive
        // compaction. Mirrors the focus_pinned pattern exactly. Only applies when a subgoal
        // strategy is active and the registry has tagged active messages.
        let active_subgoal_messages: Vec<Message> = if self
            .context_manager
            .compression
            .pruning_strategy
            .is_subgoal()
        {
            use crate::agent::compaction_strategy::SubgoalState;
            self.msg.messages[1..compact_end]
                .iter()
                .enumerate()
                .filter(|(slice_i, m)| {
                    // slice_i is 0-based within [1..compact_end]; actual index = slice_i + 1.
                    let actual_i = slice_i + 1;
                    !m.metadata.focus_pinned
                        && matches!(
                            self.compression.subgoal_registry.subgoal_state(actual_i),
                            Some(SubgoalState::Active)
                        )
                })
                .map(|(_, m)| m.clone())
                .collect()
        } else {
            vec![]
        };

        // Summarize only the non-pinned, non-active-subgoal messages in the compaction range.
        let to_compact: Vec<Message> = {
            let is_subgoal = self
                .context_manager
                .compression
                .pruning_strategy
                .is_subgoal();

            if is_subgoal {
                use crate::agent::compaction_strategy::SubgoalState;
                self.msg.messages[1..compact_end]
                    .iter()
                    .enumerate()
                    .filter(|(slice_i, m)| {
                        let actual_i = slice_i + 1;
                        !m.metadata.focus_pinned
                            && !matches!(
                                self.compression.subgoal_registry.subgoal_state(actual_i),
                                Some(SubgoalState::Active)
                            )
                    })
                    .map(|(_, m)| m.clone())
                    .collect()
            } else {
                self.msg.messages[1..compact_end]
                    .iter()
                    .filter(|m| !m.metadata.focus_pinned)
                    .cloned()
                    .collect()
            }
        };

        CompactionPartition {
            pinned_messages,
            active_subgoal_messages,
            to_compact,
        }
    }

    /// Validate summary quality via the compaction probe before committing the summary.
    ///
    /// All four probe metric counters (`compaction_probe_failures`, `compaction_probe_soft_failures`,
    /// `compaction_probe_passes`, `compaction_probe_errors`) and `last_probe_*` state are updated
    /// exclusively inside this helper. The caller MUST NOT duplicate any metric increment
    /// (cross-cutting constraint #6).
    ///
    /// Returns:
    /// - `Ok(ControlFlow::Break(CompactionOutcome::ProbeRejected))` — hard fail; caller should
    ///   return the rejected outcome immediately.
    /// - `Ok(ControlFlow::Continue(()))` — probe passed (or was disabled); caller may proceed.
    async fn apply_compaction_probe(
        &mut self,
        to_compact: &[Message],
        summary: &str,
    ) -> Result<ControlFlow<CompactionOutcome, ()>, crate::agent::error::AgentError> {
        if !self.context_manager.compression.probe.enabled {
            return Ok(ControlFlow::Continue(()));
        }

        let probe_config = self.context_manager.compression.probe.clone();
        let probe_provider = self.probe_or_summary_provider().clone();
        let probe_result = match zeph_memory::validate_compaction(
            probe_provider,
            to_compact.to_vec(),
            summary.to_owned(),
            &probe_config,
        )
        .await
        {
            Ok(result) => result,
            Err(e) => {
                tracing::warn!("compaction probe error (non-blocking): {e:#}");
                self.update_metrics(|m| {
                    m.compaction_probe_errors += 1;
                    m.last_probe_verdict = Some(zeph_memory::ProbeVerdict::Error);
                    m.last_probe_score = None;
                    m.last_probe_category_scores = None;
                });
                return Ok(ControlFlow::Continue(()));
            }
        };

        if let Some(ref result) = probe_result {
            if let Some(ref d) = self.debug_state.debug_dumper {
                d.dump_compaction_probe(result);
            }

            let cat_scores = result.category_scores.clone();
            let probe_threshold = result.threshold;
            let probe_hard_fail_threshold = result.hard_fail_threshold;
            match result.verdict {
                zeph_memory::ProbeVerdict::HardFail => {
                    tracing::warn!(
                        score = result.score,
                        threshold = result.hard_fail_threshold,
                        "compaction probe HARD FAIL — keeping original messages"
                    );
                    self.update_metrics(|m| {
                        m.compaction_probe_failures += 1;
                        m.last_probe_verdict = Some(zeph_memory::ProbeVerdict::HardFail);
                        m.last_probe_score = Some(result.score);
                        m.last_probe_category_scores = Some(cat_scores.clone());
                        m.compaction_probe_threshold = probe_threshold;
                        m.compaction_probe_hard_fail_threshold = probe_hard_fail_threshold;
                    });
                    return Ok(ControlFlow::Break(CompactionOutcome::ProbeRejected));
                }
                zeph_memory::ProbeVerdict::SoftFail => {
                    tracing::warn!(
                        score = result.score,
                        threshold = result.threshold,
                        "compaction probe SOFT FAIL — proceeding with warning"
                    );
                    self.update_metrics(|m| {
                        m.compaction_probe_soft_failures += 1;
                        m.last_probe_verdict = Some(zeph_memory::ProbeVerdict::SoftFail);
                        m.last_probe_score = Some(result.score);
                        m.last_probe_category_scores = Some(cat_scores.clone());
                        m.compaction_probe_threshold = probe_threshold;
                        m.compaction_probe_hard_fail_threshold = probe_hard_fail_threshold;
                    });
                }
                zeph_memory::ProbeVerdict::Pass => {
                    tracing::info!(score = result.score, "compaction probe passed");
                    self.update_metrics(|m| {
                        m.compaction_probe_passes += 1;
                        m.last_probe_verdict = Some(zeph_memory::ProbeVerdict::Pass);
                        m.last_probe_score = Some(result.score);
                        m.last_probe_category_scores = Some(cat_scores.clone());
                        m.compaction_probe_threshold = probe_threshold;
                        m.compaction_probe_hard_fail_threshold = probe_hard_fail_threshold;
                    });
                }
                zeph_memory::ProbeVerdict::Error => {
                    // Unreachable: validate_compaction returns Err on errors, not Ok(Error).
                    // If this fires, the error-handling path in validate_compaction changed.
                    debug_assert!(false, "ProbeVerdict::Error reached inside Ok path");
                }
            }
        }

        Ok(ControlFlow::Continue(()))
    }

    /// Drain the compaction range and re-insert the summary plus protected messages.
    ///
    /// Drains `messages[1..compact_end]`, inserts the summary at position 1, then
    /// re-inserts pinned knowledge blocks and active-subgoal messages after the summary.
    /// Rebuilds the subgoal index map when a subgoal strategy is active (S1 fix #2022).
    ///
    /// `summary` is the raw LLM output (without the header prefix) used for token-count
    /// logging. `summary_content` is the fully formatted message text inserted into the
    /// message list.
    fn finalize_compacted_messages(
        &mut self,
        compact_end: usize,
        pinned: Vec<Message>,
        active_subgoal: Vec<Message>,
        summary_content: String,
        compacted_count: usize,
        summary: &str,
    ) {
        // Drain the original range (includes pinned, active-subgoal, and non-pinned messages).
        self.msg.messages.drain(1..compact_end);
        // Insert the compaction summary at position 1.
        self.msg.messages.insert(
            1,
            Message {
                role: Role::System,
                content: summary_content,
                parts: vec![],
                metadata: MessageMetadata::agent_only(),
            },
        );
        // Re-insert pinned messages right after the summary (position 2+).
        // They are placed before the preserved tail so the LLM always sees them.
        let pinned_count = pinned.len();
        for (i, pinned_msg) in pinned.into_iter().enumerate() {
            self.msg.messages.insert(2 + i, pinned_msg);
        }
        // Re-insert active-subgoal messages after pinned messages (#2022 S2 fix).
        // Active-subgoal messages are protected from summarization — they carry the current
        // working context and must not be lost during compaction.
        for (i, active_msg) in active_subgoal.into_iter().enumerate() {
            self.msg.messages.insert(2 + pinned_count + i, active_msg);
        }

        // S1 fix (#2022): rebuild subgoal index map from scratch after drain + reinsert.
        // Arithmetic offset is fragile because the final positions depend on pinned_count
        // and active_subgoal_count. Rebuild is O(subgoals * avg_span) — negligible.
        if self
            .context_manager
            .compression
            .pruning_strategy
            .is_subgoal()
        {
            self.compression
                .subgoal_registry
                .rebuild_after_compaction(&self.msg.messages, compact_end);
        }

        tracing::info!(
            compacted_count,
            summary_tokens = self.metrics.token_counter.count_tokens(summary),
            "compacted context"
        );

        self.recompute_prompt_tokens();
        self.update_metrics(|m| {
            m.context_compactions += 1;
        });
    }

    pub(in crate::agent) async fn compact_context(
        &mut self,
    ) -> Result<CompactionOutcome, crate::agent::error::AgentError> {
        // Force-apply any pending deferred summaries before draining to avoid losing them (CRIT-01).
        let _ = self.apply_deferred_summaries();

        let preserve_tail = self.context_manager.compaction_preserve_tail;

        if self.msg.messages.len() <= preserve_tail + 1 {
            return Ok(CompactionOutcome::NoChange);
        }

        let compact_end = {
            let raw = self.msg.messages.len() - preserve_tail;
            Self::adjust_compact_end_for_tool_pairs(&self.msg.messages, raw)
        };

        if compact_end <= 1 {
            return Ok(CompactionOutcome::NoChange);
        }

        let CompactionPartition {
            pinned_messages,
            active_subgoal_messages,
            to_compact,
        } = self.partition_messages_for_compaction(compact_end);

        if to_compact.is_empty() {
            return Ok(CompactionOutcome::NoChange);
        }

        // Load compression guidelines if configured.
        // Extract all params from &self before .await so no &self is held across await.
        let guidelines = {
            let enabled = self
                .memory_state
                .compaction
                .compression_guidelines_config
                .enabled;
            let memory = self.memory_state.persistence.memory.clone();
            let conv_id = self.memory_state.persistence.conversation_id;
            Self::load_compression_guidelines(enabled, memory, conv_id).await
        };

        // Memex: archive tool output bodies before compaction (#2432).
        //
        // Archives are saved BEFORE summarization, but references are injected AFTER
        // summarization as a postfix (fix C1: LLM would destroy [archived:UUID] markers).
        // The LLM summarizes the original messages without placeholders.
        //
        // Invariant: save_archive() is called before the placeholder is created;
        // the placeholder is only inserted into the postfix, not into `to_compact`.
        // Extract all params from &self before .await so no &self is held across await.
        let archived_refs: Vec<String> = {
            let archive_enabled = self.context_manager.compression.archive_tool_outputs;
            let memory = self.memory_state.persistence.memory.clone();
            let cid = self.memory_state.persistence.conversation_id;
            Self::archive_tool_outputs(archive_enabled, memory, cid, to_compact.clone()).await
        };

        // Extract deps before .await so &self is not held across the await boundary.
        let summary = {
            let deps = self.build_summarization_deps();
            let structured = self.memory_state.compaction.structured_summaries;
            Self::summarize_messages_with_deps(
                deps,
                structured,
                to_compact.clone(),
                guidelines.clone(),
            )
            .await?
        };

        // Compaction probe: validate summary quality before committing it.
        // All metric updates live exclusively inside apply_compaction_probe.
        if let ControlFlow::Break(outcome) =
            self.apply_compaction_probe(&to_compact, &summary).await?
        {
            return Ok(outcome);
        }

        let compacted_count = to_compact.len();
        let tokens_before = self.providers.cached_prompt_tokens;
        // Inject archived references as a postfix AFTER the LLM summary (fix C1).
        // The LLM summary is unaware of archives; the postfix ensures references survive.
        let archive_postfix = if archived_refs.is_empty() {
            String::new()
        } else {
            let refs = archived_refs.join("\n");
            format!("\n\n[archived tool outputs — retrievable via read_overflow]\n{refs}")
        };
        let summary_content = format!(
            "[conversation summary — {compacted_count} messages compacted]\n{summary}{archive_postfix}"
        );

        self.finalize_compacted_messages(
            compact_end,
            pinned_messages,
            active_subgoal_messages,
            summary_content.clone(),
            compacted_count,
            &summary,
        );

        self.emit_compaction_status_signal(tokens_before).await;

        // Extract memory params before .await so no &self is held across the persist boundary.
        let (persist_failed, qdrant_fut) = {
            let memory = self.memory_state.persistence.memory.clone();
            let cid = self.memory_state.persistence.conversation_id;
            Self::persist_compaction_result(
                memory,
                cid,
                compacted_count,
                summary_content.clone(),
                summary.clone(),
            )
            .await
        };
        // Dispatch Qdrant session-summary write through the supervisor so the JoinHandle
        // is tracked, bounded, and abortable at turn boundaries (Await Discipline rule 2).
        if let Some(fut) = qdrant_fut {
            self.lifecycle
                .supervisor
                .spawn_summarization("persist-session-summary", fut);
        }
        Ok(if persist_failed {
            CompactionOutcome::CompactedWithPersistError
        } else {
            CompactionOutcome::Compacted
        })
    }

    /// Compact context and return a user-visible status string.
    ///
    /// This wrapper exists to give `agent_access_impl` a single `async fn(&mut self)` call
    /// point that the HRTB checker can verify as `Send`. The inner `compact_context()` method
    /// uses only owned data and cloned `Arc`s across every `.await` point.
    pub(in crate::agent) async fn compact_context_command(
        &mut self,
    ) -> Result<String, zeph_commands::CommandError> {
        if self.msg.messages.len() <= self.context_manager.compaction_preserve_tail + 1 {
            return Ok("Nothing to compact.".to_owned());
        }
        match self
            .compact_context()
            .await
            .map_err(|e| zeph_commands::CommandError::new(e.to_string()))?
        {
            CompactionOutcome::Compacted | CompactionOutcome::NoChange => {
                Ok("Context compacted successfully.".to_owned())
            }
            CompactionOutcome::CompactedWithPersistError => {
                Ok("Context compacted, but persistence to storage failed (see logs).".to_owned())
            }
            CompactionOutcome::ProbeRejected => {
                Ok("Compaction rejected: summary quality below threshold. \
                 Original context preserved."
                    .to_owned())
            }
        }
    }

    /// Persist a completed compaction to `SQLite`.
    ///
    /// Returns `true` when `SQLite` persistence failed (caller should set `CompactedWithPersistError`).
    /// Also returns an optional future for the Qdrant session-summary write; the caller is
    /// responsible for dispatching it through [`BackgroundSupervisor::spawn_summarization`]
    /// so the `JoinHandle` is tracked and bounded per Await Discipline rule 2.
    ///
    /// All parameters are taken by value so the caller does not hold `&self` across any `.await`,
    /// keeping the enclosing future `Send`. `DbStore: Clone` — cloning is synchronous so no
    /// `&SemanticMemory` survives the clone call into any `.await` point.
    async fn persist_compaction_result(
        memory: Option<std::sync::Arc<zeph_memory::semantic::SemanticMemory>>,
        cid: Option<zeph_memory::ConversationId>,
        compacted_count: usize,
        summary_content: String,
        summary: String,
    ) -> (
        bool,
        Option<std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'static>>>,
    ) {
        let (Some(memory), Some(cid)) = (memory, cid) else {
            return (false, None);
        };
        // Persist compaction: mark originals as user_only, insert summary as agent_only.
        // Assumption: the system prompt is always the first (oldest) row for this conversation
        // in SQLite — i.e., ids[0] corresponds to self.msg.messages[0] (the system prompt).
        // This holds for normal sessions but may not hold after cross-session restore if a
        // non-system message was persisted first. MVP assumption; document if changed.
        // oldest_message_ids returns ascending order; ids[1..=compacted_count] are the messages
        // that were drained from self.msg.messages[1..compact_end].
        //
        // Clone DbStore before any .await so no &SemanticMemory is held across await points.
        // SemanticMemory contains AnyProvider which is !Sync, so &SemanticMemory is !Send.
        // DbStore: Clone — cloning is synchronous, no borrow of memory survives .await.
        let sqlite = memory.sqlite().clone();
        let ids = sqlite
            .oldest_message_ids(cid, u32::try_from(compacted_count + 1).unwrap_or(u32::MAX))
            .await;
        let sqlite_failed = match ids {
            Ok(ids) if ids.len() >= 2 => {
                let start = ids[1];
                let end = ids[compacted_count.min(ids.len() - 1)];
                if let Err(e) = sqlite
                    .replace_conversation(cid, start..=end, "system", &summary_content)
                    .await
                {
                    tracing::warn!("failed to persist compaction in sqlite: {e:#}");
                    true
                } else {
                    false
                }
            }
            Ok(_) => false,
            Err(e) => {
                tracing::warn!("failed to get message ids for compaction: {e:#}");
                true
            }
        };

        // Return the Qdrant write as a future so the caller can dispatch it through
        // BackgroundSupervisor::spawn_summarization — tracked, bounded, abort-on-turn-boundary.
        // store_session_summary takes &self — the Arc clone here ensures 'static lifetime
        // without holding &SemanticMemory across the await inside the future.
        let qdrant_fut: std::pin::Pin<
            Box<dyn std::future::Future<Output = bool> + Send + 'static>,
        > = Box::pin(async move {
            if let Err(e) = memory.store_session_summary(cid, &summary).await {
                tracing::warn!("failed to store session summary: {e:#}");
            }
            false
        });

        (sqlite_failed, Some(qdrant_fut))
    }
}
