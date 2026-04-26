// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{LlmProvider, Message, Role};
use zeph_memory::AnchoredSummary;

use crate::agent::Agent;
use crate::agent::context::{CompactionOutcome, cap_summary, chunk_messages};
use crate::agent::context_manager::CompactionTier;
use crate::channel::Channel;
use crate::context::ContextBudget;

impl<C: Channel> Agent<C> {
    /// Tiered compaction: Soft tier prunes tool outputs + applies deferred summaries (no LLM),
    /// Hard tier falls back to full LLM summarization.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub(in crate::agent) async fn maybe_compact(
        &mut self,
    ) -> Result<(), crate::agent::error::AgentError> {
        // Increment the turn counter unconditionally so every user-message turn is counted
        // regardless of early-return guards (exhaustion, server compaction, cooldown).
        if let Some(ref mut count) = self.context_manager.turns_since_last_hard_compaction {
            *count += 1;
        }

        // Guard 3 — Exhaustion: stop compaction permanently when it cannot reduce context.
        // One-shot warning: flip warned → true on first visit, no-op on subsequent calls.
        if let crate::agent::context_manager::CompactionState::Exhausted { ref mut warned } =
            self.context_manager.compaction
            && !*warned
        {
            *warned = true;
            tracing::warn!("compaction exhausted: context budget too tight for this session");
            let _ = self
                .channel
                .send(
                    "Warning: context budget is too tight — compaction cannot free enough \
                     space. Consider increasing [memory] context_budget_tokens or starting \
                     a new session.",
                )
                .await;
        }
        if self.context_manager.compaction.is_exhausted() {
            return Ok(());
        }

        // S1: skip client-side compaction when server compaction is active — unless context
        // has grown past 95% of the budget without a server compaction event (safety fallback).
        if self.apply_server_compaction_skip_guard() {
            return Ok(());
        }

        // Skip if hard compaction already ran this turn (CRIT-03).
        if self.context_manager.compaction.is_compacted_this_turn() {
            return Ok(());
        }

        // Guard 1 — Cooldown: skip Hard-tier LLM compaction for N turns after the last successful
        // compaction. Soft compaction (pruning only) is still allowed during cooldown.
        let in_cooldown = self.decrement_cooldown_counter();

        match self.compaction_tier() {
            CompactionTier::None => Ok(()),
            CompactionTier::Soft => self.do_soft_compaction().await,
            CompactionTier::Hard => self.do_hard_compaction(in_cooldown).await,
        }
    }

    /// Returns `true` if caller should `return Ok(())` due to active server compaction.
    ///
    /// Skips client-side compaction when server compaction is active, unless context has grown
    /// past 95% of the budget without a server compaction event (safety fallback).
    fn apply_server_compaction_skip_guard(&self) -> bool {
        if !self.providers.server_compaction_active {
            return false;
        }
        let budget = self
            .context_manager
            .budget
            .as_ref()
            .map_or(0, ContextBudget::max_tokens);
        if budget > 0 {
            let total_tokens: usize = self
                .msg
                .messages
                .iter()
                .map(|m| self.metrics.token_counter.count_message_tokens(m))
                .sum();
            let fallback_threshold = budget * 95 / 100;
            if total_tokens < fallback_threshold {
                return true;
            }
            tracing::warn!(
                total_tokens,
                fallback_threshold,
                "server compaction active but context at 95%+ — falling back to client-side"
            );
            false
        } else {
            true
        }
    }

    /// Decrements the cooldown counter when in Cooling state.
    ///
    /// Returns `true` if compaction is currently in cooldown (caller should skip hard
    /// LLM summarization). Transitions state to `Ready` when the counter reaches zero.
    fn decrement_cooldown_counter(&mut self) -> bool {
        let in_cooldown = self.context_manager.compaction.cooldown_remaining() > 0;
        if in_cooldown
            && let crate::agent::context_manager::CompactionState::Cooling {
                ref mut turns_remaining,
            } = self.context_manager.compaction
        {
            *turns_remaining -= 1;
            if *turns_remaining == 0 {
                self.context_manager.compaction =
                    crate::agent::context_manager::CompactionState::Ready;
            }
        }
        in_cooldown
    }

    /// Execute the Soft compaction tier: apply deferred summaries and prune tool outputs.
    ///
    /// Never triggers an LLM call. Does not set `compacted_this_turn`, so Hard tier may still
    /// fire in the same turn if context remains above the hard threshold.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    async fn do_soft_compaction(&mut self) -> Result<(), crate::agent::error::AgentError> {
        let _ = self.channel.send_status("soft compacting context...").await;

        // Step 0 (context-compression): apply any completed background goal extraction
        // and schedule a new one if the user message has changed (#1909, #2022).
        {
            use crate::config::PruningStrategy;
            match &self.context_manager.compression.pruning_strategy {
                PruningStrategy::Subgoal | PruningStrategy::SubgoalMig => {
                    self.maybe_refresh_subgoal();
                }
                _ => self.maybe_refresh_task_goal(),
            }
        }

        // Step 1: apply deferred tool summaries (free tokens without LLM).
        let applied = self.apply_deferred_summaries();
        let _ = self.apply_deferred_summaries();

        // Step 1b (S5 fix): rebuild subgoal index map if deferred summaries were applied.
        // Deferred summaries insert messages (shifting indices), invalidating msg_to_subgoal.
        if applied > 0
            && self
                .context_manager
                .compression
                .pruning_strategy
                .is_subgoal()
        {
            self.compression.subgoal_registry.rebuild_after_compaction(
                &self.msg.messages,
                0, // 0 = no drain, just repair shifted indices
            );
        }

        // Step 2: prune tool outputs down to soft threshold.
        let budget = self
            .context_manager
            .budget
            .as_ref()
            .map_or(0, ContextBudget::max_tokens);
        let soft_threshold =
            (budget as f32 * self.context_manager.soft_compaction_threshold) as usize;
        let cached = usize::try_from(self.providers.cached_prompt_tokens).unwrap_or(usize::MAX);
        let min_to_free = cached.saturating_sub(soft_threshold);
        if min_to_free > 0 {
            self.prune_tool_outputs(min_to_free);
        }

        let _ = self.channel.send_status("").await;
        tracing::info!(
            cached_tokens = self.providers.cached_prompt_tokens,
            soft_threshold,
            "soft compaction complete"
        );
        // Soft compaction does NOT set compacted_this_turn, allowing Hard to fire
        // in the same turn if context is still above the hard threshold.
        Ok(())
    }

    /// Execute the Hard compaction tier: prune tool outputs and fall back to LLM summarization.
    ///
    /// Respects the cooldown guard: when `in_cooldown` is `true`, skips LLM summarization
    /// and returns immediately after tracking the compaction event.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    async fn do_hard_compaction(
        &mut self,
        in_cooldown: bool,
    ) -> Result<(), crate::agent::error::AgentError> {
        // Track hard compaction event: finalize the previous segment's turn count
        // and start a new one. Counted regardless of cooldown — captures pressure,
        // not just action. When compaction_hard_count == 0, compaction_turns_after_hard
        // is expected to be empty.
        if let Some(turns) = self.context_manager.turns_since_last_hard_compaction {
            self.update_metrics(|m| {
                m.compaction_turns_after_hard.push(turns);
            });
        }
        self.context_manager.turns_since_last_hard_compaction = Some(0);
        self.update_metrics(|m| {
            m.compaction_hard_count += 1;
        });

        // Cooldown guard: skip LLM summarization while cooling down.
        if in_cooldown {
            tracing::debug!(
                turns_remaining = self.context_manager.compaction.cooldown_remaining(),
                "hard compaction skipped: cooldown active"
            );
            return Ok(());
        }

        let budget = self
            .context_manager
            .budget
            .as_ref()
            .map_or(0, ContextBudget::max_tokens);
        let hard_threshold =
            (budget as f32 * self.context_manager.hard_compaction_threshold) as usize;
        let cached = usize::try_from(self.providers.cached_prompt_tokens).unwrap_or(usize::MAX);
        let min_to_free = cached.saturating_sub(hard_threshold);

        let _ = self.channel.send_status("compacting context...").await;

        // Step 1: apply deferred summaries first (free tokens without LLM).
        self.apply_deferred_summaries();

        // Step 2: prune tool outputs.
        if self.try_pruning_only_hard_compaction(min_to_free).await? {
            return Ok(());
        }

        // Step 3: Guard 2 — Counterproductive: check if there are enough messages
        // to make LLM summarization worthwhile.
        let preserve_tail = self.context_manager.compaction_preserve_tail;
        let compactable = self.msg.messages.len().saturating_sub(preserve_tail + 1);
        if compactable <= 1 {
            tracing::warn!(
                compactable,
                "hard compaction: too few messages to compact, marking exhausted"
            );
            // Only reachable from Ready state (Cooling is guarded by in_cooldown above).
            self.context_manager.compaction =
                crate::agent::context_manager::CompactionState::Exhausted { warned: false };
            let _ = self.channel.send_status("").await;
            return Ok(());
        }

        // Step 4: fall back to full LLM summarization.
        tracing::info!(
            min_to_free,
            "hard compaction: pruning insufficient, falling back to LLM summarization"
        );
        let tokens_before = self.providers.cached_prompt_tokens;
        let outcome = self.compact_context().await?;

        if self
            .record_hard_compaction_outcome(outcome, tokens_before)
            .await?
        {
            return Ok(());
        }

        self.emit_compaction_status_signal(tokens_before).await;
        Ok(())
    }

    /// Attempt to satisfy the hard compaction budget with pruning alone (no LLM).
    ///
    /// Returns `Ok(true)` if pruning was sufficient and the caller should return early.
    async fn try_pruning_only_hard_compaction(
        &mut self,
        min_to_free: usize,
    ) -> Result<bool, crate::agent::error::AgentError> {
        let freed = self.prune_tool_outputs(min_to_free);
        if freed >= min_to_free {
            tracing::info!(freed, "hard compaction: pruning sufficient");
            self.context_manager.compaction =
                crate::agent::context_manager::CompactionState::CompactedThisTurn {
                    cooldown: self.context_manager.compaction_cooldown_turns,
                };
            self.flush_deferred_summaries().await;
            let _ = self.channel.send_status("").await;
            return Ok(true);
        }
        Ok(false)
    }

    /// Record the outcome of the LLM-based hard compaction pass and update state.
    ///
    /// Returns `Ok(true)` if the caller should return early (exhausted state reached or
    /// probe rejected). Returns `Ok(false)` to proceed to the status signal emission.
    async fn record_hard_compaction_outcome(
        &mut self,
        outcome: CompactionOutcome,
        tokens_before: u64,
    ) -> Result<bool, crate::agent::error::AgentError> {
        match outcome {
            CompactionOutcome::ProbeRejected => {
                // Probe rejected the summary. This is NOT exhaustion — the compactor
                // can still summarize, but the summary was too lossy.
                // Set cooldown to prevent immediate retry, but do NOT mark Exhausted.
                tracing::info!("compaction probe rejected summary — setting cooldown");
                self.context_manager.compaction =
                    crate::agent::context_manager::CompactionState::CompactedThisTurn {
                        cooldown: self.context_manager.compaction_cooldown_turns,
                    };
                let _ = self
                    .channel
                    .send("Context compaction skipped this turn — will retry.")
                    .await;
                return Ok(true);
            }
            CompactionOutcome::CompactedWithPersistError => {
                tracing::warn!(
                    "compaction succeeded but persist failed — in-memory state is \
                     correct, storage may be inconsistent"
                );
                // Fall through to the same handling as Compacted.
                let freed_tokens =
                    tokens_before.saturating_sub(self.providers.cached_prompt_tokens);
                if freed_tokens == 0 {
                    self.context_manager.compaction =
                        crate::agent::context_manager::CompactionState::Exhausted { warned: false };
                    let _ = self.channel.send_status("").await;
                    return Ok(true);
                }
                if matches!(self.compaction_tier(), CompactionTier::Hard) {
                    self.context_manager.compaction =
                        crate::agent::context_manager::CompactionState::Exhausted { warned: false };
                    let _ = self.channel.send_status("").await;
                    return Ok(true);
                }
                self.context_manager.compaction =
                    crate::agent::context_manager::CompactionState::CompactedThisTurn {
                        cooldown: self.context_manager.compaction_cooldown_turns,
                    };
            }
            CompactionOutcome::Compacted => {
                // Guard 2 — Counterproductive: net freed tokens is zero (summary ate all
                // freed space — no net reduction).
                let freed_tokens =
                    tokens_before.saturating_sub(self.providers.cached_prompt_tokens);
                if freed_tokens == 0 {
                    tracing::warn!(
                        "hard compaction: summary consumed all freed tokens — no net \
                         reduction, marking exhausted"
                    );
                    // Only reachable from Ready state (Cooling is guarded by in_cooldown).
                    self.context_manager.compaction =
                        crate::agent::context_manager::CompactionState::Exhausted { warned: false };
                    let _ = self.channel.send_status("").await;
                    return Ok(true);
                }
                // Guard 3 — Still above threshold: compaction freed some tokens but context
                // remains above the hard threshold; further LLM attempts are unlikely to help.
                if matches!(self.compaction_tier(), CompactionTier::Hard) {
                    tracing::warn!(
                        freed_tokens,
                        "hard compaction: context still above hard threshold after \
                         compaction, marking exhausted"
                    );
                    // Only reachable from Ready state (Cooling is guarded by in_cooldown).
                    self.context_manager.compaction =
                        crate::agent::context_manager::CompactionState::Exhausted { warned: false };
                    let _ = self.channel.send_status("").await;
                    return Ok(true);
                }
                self.context_manager.compaction =
                    crate::agent::context_manager::CompactionState::CompactedThisTurn {
                        cooldown: self.context_manager.compaction_cooldown_turns,
                    };
            }
            CompactionOutcome::NoChange => {
                // compact_context() decided there was nothing to compact.
                // The compactable <= 1 guard above should have caught this, but handle
                // it gracefully if the messages changed during the async call.
            }
        }
        Ok(false)
    }

    /// Emit a UX status signal when tokens were actually freed by compaction (#3314).
    async fn emit_compaction_status_signal(&mut self, tokens_before: u64) {
        let tokens_after = self.providers.cached_prompt_tokens;
        if tokens_after < tokens_before {
            let now_ms = u64::try_from(
                std::time::SystemTime::UNIX_EPOCH
                    .elapsed()
                    .unwrap_or_default()
                    .as_millis(),
            )
            .unwrap_or(u64::MAX);
            tracing::info!(
                tokens_before,
                tokens_after,
                saved = tokens_before.saturating_sub(tokens_after),
                "context compaction complete"
            );
            let _ = self
                .channel
                .send_status(&format!(
                    "Compacting: {tokens_before}→{tokens_after} tokens"
                ))
                .await;
            self.update_metrics(|m| {
                m.compaction_last_before = tokens_before;
                m.compaction_last_after = tokens_after;
                m.compaction_last_at_ms = now_ms;
            });
        }
    }

    /// Soft-only compaction for mid-iteration use inside tool execution loops.
    ///
    /// Applies deferred tool summaries and prunes tool outputs down to the soft threshold.
    /// Never triggers Hard tier (no LLM call), never increments
    /// `turns_since_last_hard_compaction`, and never decrements the cooldown counter.
    /// Returns immediately when `compacted_this_turn` is set (hard compaction ran earlier
    /// in this turn) or when context usage is below the soft threshold.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub(in crate::agent) fn maybe_soft_compact_mid_iteration(&mut self) {
        if self.context_manager.compaction.is_compacted_this_turn() {
            return;
        }
        if !matches!(
            self.compaction_tier(),
            CompactionTier::Soft | CompactionTier::Hard
        ) {
            return;
        }
        let budget = self
            .context_manager
            .budget
            .as_ref()
            .map_or(0, ContextBudget::max_tokens);
        let soft_threshold =
            (budget as f32 * self.context_manager.soft_compaction_threshold) as usize;
        let cached = usize::try_from(self.providers.cached_prompt_tokens).unwrap_or(usize::MAX);
        // Step 1: apply deferred summaries.
        self.apply_deferred_summaries();
        // Step 2: prune tool outputs down to soft threshold.
        let min_to_free = cached.saturating_sub(soft_threshold);
        if min_to_free > 0 {
            self.prune_tool_outputs(min_to_free);
        }
        tracing::debug!(
            cached_tokens = self.providers.cached_prompt_tokens,
            soft_threshold,
            "mid-iteration soft compaction complete"
        );
    }

    /// Run the Focus strategy auto-consolidation pass.
    ///
    /// Acquires the compression guard, clones the relevant message slice, calls
    /// `run_focus_auto_consolidation`, and appends any resulting summary as an
    /// `AutoConsolidated` knowledge block. Always releases the guard on exit.
    /// This is a helper extracted from `maybe_proactive_compress` to stay within the
    /// 100-line function limit.
    async fn run_focus_auto_consolidation_pass(
        &mut self,
    ) -> Result<(), crate::agent::error::AgentError> {
        use crate::agent::compaction_strategy::run_focus_auto_consolidation;

        if !self.focus.try_acquire_compression() {
            tracing::debug!("focus auto-consolidation skipped — compression already in progress");
            return Ok(());
        }
        // RAII guard: releases the compression lock on drop, even on cancellation or panic.
        // Clones the Arc so FocusState can still be mutably borrowed later in this scope.
        let _compression_guard =
            crate::agent::focus::CompressionGuard(self.focus.compressing.clone());

        let _ = self.channel.send_status("consolidating knowledge...").await;

        let focus_scorer_provider_name = self
            .context_manager
            .compression
            .focus_scorer_provider
            .as_str()
            .to_owned();

        // S4: if a provider name is configured but not resolvable, skip rather than silently
        // burning premium tokens on the primary provider.
        if !focus_scorer_provider_name.is_empty()
            && !self.providers.provider_pool.iter().any(|e| {
                e.effective_name()
                    .eq_ignore_ascii_case(&focus_scorer_provider_name)
            })
        {
            tracing::error!(
                provider = %focus_scorer_provider_name,
                "focus_scorer_provider not found in [[llm.providers]], skipping auto-consolidation"
            );
            return Ok(());
        }

        let focus_provider = self.resolve_background_provider(&focus_scorer_provider_name);

        // Clone a bounded message slice before the await point (Await Discipline §6).
        let preserve_tail = self.context_manager.compaction_preserve_tail;
        let slice_end = self.msg.messages.len().saturating_sub(preserve_tail);
        let messages: Vec<_> = self.msg.messages[..slice_end].to_vec();
        let max_chars = self.focus.config.max_knowledge_tokens.saturating_mul(4);
        let min_window = self.focus.config.auto_consolidate_min_window;

        tracing::debug!(
            min_window,
            max_chars,
            cached_tokens = self.providers.cached_prompt_tokens,
            "focus auto-consolidation starting"
        );

        let outcome =
            run_focus_auto_consolidation(&messages, min_window, focus_provider, max_chars).await;

        match outcome {
            Ok(Some(summary)) => {
                tracing::info!(
                    chars = summary.len(),
                    "focus auto-consolidation produced summary"
                );
                self.focus.append_auto_knowledge(summary);
                self.update_metrics(|m| m.compression_events += 1);
            }
            Ok(None) => tracing::debug!("focus auto-consolidation: no qualifying window found"),
            Err(e) => tracing::warn!("focus auto-consolidation failed: {e}"),
        }

        let _ = self.channel.send_status("").await;
        Ok(())
    }

    /// Proactive context compression: fires before reactive compaction when context exceeds
    /// the configured `threshold_tokens`. Mutually exclusive with reactive compaction per turn
    /// (guarded by `compacted_this_turn`).
    pub(in crate::agent) async fn maybe_proactive_compress(
        &mut self,
    ) -> Result<(), crate::agent::error::AgentError> {
        // S1: skip proactive compression when server compaction is active — unless context
        // has grown past 95% of the budget without a server compaction event (safety fallback).
        if self.providers.server_compaction_active {
            let budget = self
                .context_manager
                .budget
                .as_ref()
                .map_or(0, ContextBudget::max_tokens);
            if budget > 0 {
                let fallback_threshold = (budget * 95 / 100) as u64;
                if self.providers.cached_prompt_tokens <= fallback_threshold {
                    return Ok(());
                }
                tracing::warn!(
                    cached_prompt_tokens = self.providers.cached_prompt_tokens,
                    fallback_threshold,
                    "server compaction active but context at 95%+ — falling back to client-side proactive"
                );
            } else {
                return Ok(());
            }
        }
        let Some((_threshold, max_summary_tokens)) = self
            .context_manager
            .should_proactively_compress(self.providers.cached_prompt_tokens)
        else {
            return Ok(());
        };

        // Branch on compression strategy: Focus augments with auto-consolidation;
        // all other proactive-eligible strategies use the LLM compaction path.
        if matches!(
            self.context_manager.compression.strategy,
            crate::config::CompressionStrategy::Focus
        ) {
            self.run_focus_auto_consolidation_pass().await?;
            // Focus augments context — do NOT set compacted_this_turn so reactive may still fire.
            return Ok(());
        }

        let tokens_before = self.providers.cached_prompt_tokens;
        let _ = self.channel.send_status("compressing context...").await;
        tracing::info!(
            max_summary_tokens,
            cached_tokens = tokens_before,
            "proactive compression triggered"
        );

        let result = self
            .compact_context_with_budget(Some(max_summary_tokens))
            .await;

        if result.is_ok() {
            // Proactive compression does not impose a post-compaction cooldown.
            self.context_manager.compaction =
                crate::agent::context_manager::CompactionState::CompactedThisTurn { cooldown: 0 };
            let tokens_saved = tokens_before.saturating_sub(self.providers.cached_prompt_tokens);
            self.update_metrics(|m| {
                m.compression_events += 1;
                m.compression_tokens_saved += tokens_saved;
            });
        }

        let _ = self.channel.send_status("").await;
        result
    }

    /// Run LLM compaction with an optional chunk budget hint for the summary.
    ///
    /// When `max_summary_tokens` is `Some(n)`, the chunk budget used by `chunk_messages`
    /// is capped at `n`, limiting how much context is summarized per LLM call.
    pub(super) async fn compact_context_with_budget(
        &mut self,
        max_summary_tokens: Option<usize>,
    ) -> Result<(), crate::agent::error::AgentError> {
        // Force-apply any pending deferred summaries before draining to avoid losing them (CRIT-01).
        let _ = self.apply_deferred_summaries();

        let preserve_tail = self.context_manager.compaction_preserve_tail;

        if self.msg.messages.len() <= preserve_tail + 1 {
            return Ok(());
        }

        let compact_end = {
            let raw = self.msg.messages.len() - preserve_tail;
            Self::adjust_compact_end_for_tool_pairs(&self.msg.messages, raw)
        };

        if compact_end <= 1 {
            return Ok(());
        }

        let to_compact = &self.msg.messages[1..compact_end];
        if to_compact.is_empty() {
            return Ok(());
        }

        // Memex: archive tool output bodies before compaction (#2432).
        let archived_refs: Vec<String> = self.archive_tool_outputs_for_compaction(to_compact).await;

        let summary = self
            .summarize_messages_with_budget(to_compact, max_summary_tokens)
            .await?;

        let compacted_count = to_compact.len();
        let archive_postfix = if archived_refs.is_empty() {
            String::new()
        } else {
            let refs = archived_refs.join("\n");
            format!("\n\n[archived tool outputs — retrievable via read_overflow]\n{refs}")
        };
        let summary_content = format!(
            "[conversation summary — {compacted_count} messages compacted]\n{summary}{archive_postfix}"
        );
        self.msg.messages.drain(1..compact_end);
        self.msg.messages.insert(
            1,
            Message {
                role: Role::System,
                content: summary_content.clone(),
                parts: vec![],
                metadata: zeph_llm::provider::MessageMetadata::agent_only(),
            },
        );

        tracing::info!(
            compacted_count,
            summary_tokens = self.metrics.token_counter.count_tokens(&summary),
            "compacted context (with budget)"
        );

        self.recompute_prompt_tokens();
        self.update_metrics(|m| {
            m.context_compactions += 1;
        });

        if let (Some(memory), Some(cid)) = (
            &self.memory_state.persistence.memory,
            self.memory_state.persistence.conversation_id,
        ) {
            let sqlite = memory.sqlite();
            let ids = sqlite
                .oldest_message_ids(cid, u32::try_from(compacted_count + 1).unwrap_or(u32::MAX))
                .await;
            let mut persist_failed = false;
            match ids {
                Ok(ids) if ids.len() >= 2 => {
                    let start = ids[1];
                    let end = ids[compacted_count.min(ids.len() - 1)];
                    if let Err(e) = sqlite
                        .replace_conversation(cid, start..=end, "system", &summary_content)
                        .await
                    {
                        tracing::warn!("failed to persist compaction in sqlite: {e:#}");
                        persist_failed = true;
                    } else if let Err(e) = memory.store_session_summary(cid, &summary).await {
                        tracing::warn!("failed to store session summary in Qdrant: {e:#}");
                        persist_failed = true;
                    }
                }
                Ok(_) => {
                    if let Err(e) = memory.store_session_summary(cid, &summary).await {
                        tracing::warn!("failed to store session summary: {e:#}");
                        persist_failed = true;
                    }
                }
                Err(e) => {
                    tracing::warn!("failed to get message ids for compaction: {e:#}");
                    if let Err(e) = memory.store_session_summary(cid, &summary).await {
                        tracing::warn!("failed to store session summary: {e:#}");
                    }
                    persist_failed = true;
                }
            }
            if persist_failed {
                let _ = self
                    .channel
                    .send(
                        "Context compaction failed — response quality may be affected in long \
                         sessions.",
                    )
                    .await;
            }
        }

        Ok(())
    }

    /// Summarize messages with an optional chunk-size budget.
    ///
    /// When `chunk_budget` is `Some(n)`, the token budget per chunk is `n` instead of
    /// the default 4096. This indirectly limits how long summaries are by reducing
    /// how much context is fed to each LLM call.
    async fn summarize_messages_with_budget(
        &self,
        messages: &[Message],
        chunk_budget: Option<usize>,
    ) -> Result<String, crate::agent::error::AgentError> {
        // Try direct summarization first
        let chunk_token_budget = chunk_budget.unwrap_or(4096);
        let oversized_threshold = chunk_token_budget / 2;
        let guidelines = self.load_compression_guidelines_if_enabled().await;

        let chunks = chunk_messages(
            messages,
            chunk_token_budget,
            oversized_threshold,
            &self.metrics.token_counter,
        );

        let llm_timeout = std::time::Duration::from_secs(self.runtime.timeouts.llm_seconds);

        let try_llm = |msgs: &[Message]| {
            let prompt = Self::build_chunk_prompt(msgs, &guidelines);
            let provider = self.summary_or_primary_provider().clone();
            async move {
                tokio::time::timeout(
                    llm_timeout,
                    provider.chat(&[Message {
                        role: Role::User,
                        content: prompt,
                        parts: vec![],
                        metadata: zeph_llm::provider::MessageMetadata::default(),
                    }]),
                )
                .await
                .map_err(|_| zeph_llm::LlmError::Timeout)?
            }
        };

        // For single chunk, summarize directly
        if chunks.len() <= 1 {
            // Structured path for single-chunk (IMP-02): mirrors summarize_messages().
            if self.memory_state.compaction.structured_summaries {
                match self.try_summarize_structured(messages, &guidelines).await {
                    Ok(anchored) => {
                        if let Some(ref d) = self.debug_state.debug_dumper {
                            d.dump_anchored_summary(&anchored, false, &self.metrics.token_counter);
                        }
                        return Ok(cap_summary(anchored.to_markdown(), 16_000));
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "structured summarization (budget path) failed, falling back to prose"
                        );
                        if let Some(ref d) = self.debug_state.debug_dumper {
                            let empty = AnchoredSummary {
                                session_intent: String::new(),
                                files_modified: vec![],
                                decisions_made: vec![],
                                open_questions: vec![],
                                next_steps: vec![],
                            };
                            d.dump_anchored_summary(&empty, true, &self.metrics.token_counter);
                        }
                    }
                }
            }

            match try_llm(messages).await {
                Ok(s) => {
                    // SEC-02: cap summary length to avoid LLM output expanding context.
                    // Estimate 4 chars per token; cap at 2× the requested budget or 8000 tokens.
                    let cap_chars = chunk_budget.unwrap_or(8_000).saturating_mul(8);
                    return Ok(cap_summary(s, cap_chars));
                }
                Err(e) if !e.is_context_length_error() => return Err(e.into()),
                Err(_) => {
                    tracing::warn!(
                        "summarization hit context length error, using metadata fallback"
                    );
                }
            }
            return Ok(Self::build_metadata_summary(messages));
        }

        // Multi-chunk: use the existing summarize_messages logic (chunk_budget only applied to
        // chunk splitting above; consolidated summary uses the default path)
        self.summarize_messages(messages, &guidelines).await
    }

    /// Apply a completed background task-goal extraction result to `current_task_goal`.
    fn apply_completed_task_goal(&mut self) {
        use futures::FutureExt as _;
        if let Some(handle) = self.compression.pending_task_goal.take() {
            if let Some(Ok(Some(goal))) = handle.now_or_never() {
                tracing::debug!("extract_task_goal: background result applied");
                self.compression.current_task_goal = Some(goal);
            }
            // Clear spinner on ALL completion paths (success, None result, or task panic).
            if let Some(ref tx) = self.session.status_tx {
                let _ = tx.send(String::new());
            }
        }
    }

    /// Refresh the cached task goal when the last user message has changed (#1850, #1909).
    ///
    /// Two-phase non-blocking design (mirrors `maybe_sidequest_eviction`):
    ///
    /// - Phase 1 (apply): if a background extraction task spawned last compaction has finished,
    ///   apply its result to `current_task_goal`.
    /// - Phase 2 (schedule): if the user message hash has changed and no task is in-flight,
    ///   spawn a new background `tokio::spawn` for goal extraction. The current compaction uses
    ///   whatever goal was cached from the previous extraction — never blocks.
    ///
    /// This eliminates the 5-second latency spike on every Soft tier compaction that made
    /// `task_aware`/`mig` strategies non-functional for cloud LLM providers.
    pub(in crate::agent) fn maybe_refresh_task_goal(&mut self) {
        use crate::config::PruningStrategy;

        // Only needed when a task-aware or MIG strategy is active.
        match &self.context_manager.compression.pruning_strategy {
            PruningStrategy::Reactive | PruningStrategy::Subgoal | PruningStrategy::SubgoalMig => {
                return;
            }
            PruningStrategy::TaskAware | PruningStrategy::Mig => {}
        }

        // Phase 1: apply background result if the task has completed.
        if self
            .compression
            .pending_task_goal
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            self.apply_completed_task_goal();
        }

        // Phase 2: do not spawn a second task while one is already in-flight.
        if self.compression.pending_task_goal.is_some() {
            return;
        }

        let Some(hash) = last_user_content_hash(&self.msg.messages) else {
            return;
        };

        // Cache hit: extraction already scheduled or completed for this user message.
        if self.compression.task_goal_user_msg_hash == Some(hash) {
            return;
        }

        // Cache miss: update hash and spawn background extraction.
        self.compression.task_goal_user_msg_hash = Some(hash);

        let recent = recent_user_assistant_excerpt(&self.msg.messages, 10, false);
        let provider = self.summary_or_primary_provider().clone();

        let handle = spawn_task_goal_extraction(provider, recent);

        // TODO(I3): this JoinHandle is never `.abort()`ed on agent shutdown. The background task
        // will run to completion (or until the 30-second timeout) even after the agent is dropped.
        // Proper cancellation requires surfacing the handle to the shutdown path — tracked as a
        // separate issue (background tasks not cancelled on agent shutdown).
        self.compression.pending_task_goal = Some(handle);
        tracing::debug!("extract_task_goal: background task spawned");
        if let Some(ref tx) = self.session.status_tx {
            let _ = tx.send("Extracting task goal...".into());
        }
    }

    /// Apply a completed background subgoal extraction result to the subgoal registry.
    fn apply_completed_subgoal(&mut self, msg_len: usize) {
        use futures::FutureExt as _;
        if let Some(handle) = self.compression.pending_subgoal.take() {
            if let Some(Ok(Some(result))) = handle.now_or_never() {
                let is_transition = result.completed.is_some();
                if is_transition {
                    self.register_subgoal_transition(&result, msg_len);
                } else {
                    self.register_subgoal_continuation(&result, msg_len);
                }
            }
            // Clear spinner on ALL completion paths (success, None, or panic).
            if let Some(ref tx) = self.session.status_tx {
                let _ = tx.send(String::new());
            }
        }
    }

    /// Register a subgoal transition: complete the current active subgoal and start a new one.
    fn register_subgoal_transition(
        &mut self,
        result: &crate::agent::state::SubgoalExtractionResult,
        msg_len: usize,
    ) {
        if let Some(completed_desc) = &result.completed {
            tracing::debug!(
                completed = completed_desc.as_str(),
                "subgoal transition detected"
            );
        }
        self.compression
            .subgoal_registry
            .complete_active(msg_len.saturating_sub(1));
        let new_id = self
            .compression
            .subgoal_registry
            .push_active(result.current.clone(), msg_len.saturating_sub(1));
        self.compression
            .subgoal_registry
            .extend_active(msg_len.saturating_sub(1));
        tracing::debug!(
            current = result.current.as_str(),
            id = new_id.0,
            "new active subgoal registered"
        );
    }

    /// Register a subgoal continuation: extend or create the first subgoal.
    fn register_subgoal_continuation(
        &mut self,
        result: &crate::agent::state::SubgoalExtractionResult,
        msg_len: usize,
    ) {
        let is_first = self.compression.subgoal_registry.subgoals.is_empty();
        if is_first {
            // First extraction result: create initial subgoal.
            let id = self
                .compression
                .subgoal_registry
                .push_active(result.current.clone(), msg_len.saturating_sub(1));
            // S4 fix: retroactively tag all pre-extraction messages [1..msg_len-1].
            if msg_len > 2 {
                self.compression
                    .subgoal_registry
                    .tag_range(1, msg_len - 2, id);
            }
            self.compression
                .subgoal_registry
                .extend_active(msg_len.saturating_sub(1));
            tracing::debug!(
                current = result.current.as_str(),
                id = id.0,
                retroactive_msgs = msg_len.saturating_sub(2),
                "first subgoal registered with retroactive tagging"
            );
        } else {
            // Extend existing active subgoal.
            self.compression
                .subgoal_registry
                .extend_active(msg_len.saturating_sub(1));
            tracing::debug!(current = result.current.as_str(), "active subgoal extended");
        }
    }

    /// Refresh the subgoal registry when the last user message has changed (#2022).
    ///
    /// Mirrors the two-phase `maybe_refresh_task_goal` pattern exactly:
    ///
    /// - Phase 1 (apply): if the background extraction task from last turn has finished,
    ///   parse the result and update the subgoal registry.
    /// - Phase 2 (schedule): if the user message hash has changed and no task is in-flight,
    ///   spawn a new background extraction. Current compaction uses the cached registry state.
    ///
    /// Transition detection: the LLM's `COMPLETED:` signal drives transitions (S3 fix).
    /// When `COMPLETED: NONE`, the same subgoal continues (`extend_active`).
    /// When `COMPLETED:` is non-NONE, a new subgoal is created (`complete_active` + `push_active`).
    pub(in crate::agent) fn maybe_refresh_subgoal(&mut self) {
        use crate::config::PruningStrategy;

        // Only needed when a subgoal-aware strategy is active.
        match &self.context_manager.compression.pruning_strategy {
            PruningStrategy::Subgoal | PruningStrategy::SubgoalMig => {}
            _ => return,
        }

        let msg_len = self.msg.messages.len();

        // Phase 1: apply background result if the task has completed.
        if self
            .compression
            .pending_subgoal
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            self.apply_completed_subgoal(msg_len);
        }

        // Phase 2: do not spawn a second task while one is in-flight.
        if self.compression.pending_subgoal.is_some() {
            return;
        }

        // Find the last agent-visible user message content and check for hash change.
        let last_user_content = self
            .msg
            .messages
            .iter()
            .rev()
            .find(|m| {
                m.role == zeph_llm::provider::Role::User && m.metadata.visibility.is_agent_visible()
            })
            .map(|m| m.content.as_str())
            .unwrap_or_default();

        if last_user_content.is_empty() {
            return;
        }

        let hash = {
            use std::hash::Hash as _;
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            last_user_content.hash(&mut hasher);
            std::hash::Hasher::finish(&hasher)
        };

        if self.compression.subgoal_user_msg_hash == Some(hash) {
            return;
        }
        self.compression.subgoal_user_msg_hash = Some(hash);

        // Clone the last 6 agent-visible messages (M2 fix: only agent_visible, not invisible
        // [tool summary] placeholders) for the extraction prompt.
        let recent = recent_user_assistant_excerpt(&self.msg.messages, 6, true);
        let provider = self.summary_or_primary_provider().clone();

        let handle = spawn_subgoal_extraction(provider, recent);

        // intentionally untracked: returns a typed JoinHandle<Option<SubgoalExtractionResult>>
        // stored in compression.pending_subgoal and polled non-blocking next turn.
        // BackgroundSupervisor does not support typed result retrieval.
        self.compression.pending_subgoal = Some(handle);
        tracing::debug!("subgoal_extraction: background task spawned");
        if let Some(ref tx) = self.session.status_tx {
            let _ = tx.send("Tracking subgoal...".into());
        }
    }
}

/// Compute a hash of the last user message in `messages`.
///
/// Returns `None` if there is no user message or the content is empty.
/// Reusable by both `maybe_refresh_task_goal` and `maybe_refresh_subgoal`.
fn last_user_content_hash(messages: &[Message]) -> Option<u64> {
    use std::hash::Hash as _;

    let content = messages
        .iter()
        .rev()
        .find(|m| m.role == zeph_llm::provider::Role::User)
        .map(|m| m.content.as_str())
        .unwrap_or_default();

    if content.is_empty() {
        return None;
    }

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    Some(std::hash::Hasher::finish(&hasher))
}

/// Collect recent user/assistant messages for LLM extraction prompts.
///
/// Takes up to `take` messages from the tail of `messages`, reversed so newest is last.
/// When `agent_visible_only` is `true`, only messages with
/// [`MessageVisibility::is_agent_visible`] are included (used by subgoal extraction to
/// exclude invisible `[tool summary]` placeholders).
fn recent_user_assistant_excerpt(
    messages: &[Message],
    take: usize,
    agent_visible_only: bool,
) -> Vec<(Role, String)> {
    messages
        .iter()
        .filter(|m| {
            let role_ok = matches!(
                m.role,
                zeph_llm::provider::Role::User | zeph_llm::provider::Role::Assistant
            );
            let visible_ok = !agent_visible_only || m.metadata.visibility.is_agent_visible();
            role_ok && visible_ok
        })
        .rev()
        .take(take)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|m| (m.role, m.content.clone()))
        .collect()
}

/// Spawn a background task-goal extraction task.
///
/// Returns the `JoinHandle`; the caller stores it in `compression.pending_task_goal` and
/// polls it non-blocking on the next turn.
///
/// # Note
///
/// This handle is intentionally untracked — `BackgroundSupervisor` does not support typed
/// result retrieval. The handle is stored and polled via `JoinHandle::is_finished` +
/// `FutureExt::now_or_never`.
fn spawn_task_goal_extraction(
    provider: AnyProvider,
    recent: Vec<(Role, String)>,
) -> tokio::task::JoinHandle<Option<String>> {
    tokio::spawn(async move {
        use zeph_llm::provider::{Message, MessageMetadata, Role};

        if recent.is_empty() {
            return None;
        }

        let mut context_text = String::new();
        for (role, content) in &recent {
            let role_str = match role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::System => "system",
            };
            let preview = if content.len() > 300 {
                let end = content.floor_char_boundary(300);
                &content[..end]
            } else {
                content.as_str()
            };
            let _ = std::fmt::write(&mut context_text, format_args!("[{role_str}]: {preview}\n"));
        }

        let prompt = format!(
            "Extract the current task goal from this conversation excerpt in one concise \
             sentence.\nFocus on what the user is trying to accomplish right now.\n\
             Respond with only the goal sentence, no preamble.\n\n\
             <conversation>\n{context_text}</conversation>"
        );

        let msgs = [Message {
            role: Role::User,
            content: prompt,
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        match tokio::time::timeout(std::time::Duration::from_secs(30), provider.chat(&msgs)).await {
            Ok(Ok(goal)) => {
                let trimmed = goal.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    const MAX_GOAL_CHARS: usize = 500;
                    let capped = if trimmed.len() > MAX_GOAL_CHARS {
                        tracing::warn!(
                            len = trimmed.len(),
                            "extract_task_goal: LLM returned oversized goal; truncating to {MAX_GOAL_CHARS} chars"
                        );
                        let end = trimmed.floor_char_boundary(MAX_GOAL_CHARS);
                        &trimmed[..end]
                    } else {
                        trimmed
                    };
                    Some(capped.to_string())
                }
            }
            Ok(Err(e)) => {
                tracing::debug!("extract_task_goal: LLM error: {e:#}");
                None
            }
            Err(_) => {
                tracing::debug!("extract_task_goal: timed out");
                None
            }
        }
    })
}

/// Spawn a background subgoal extraction task.
///
/// Returns the `JoinHandle`; the caller stores it in `compression.pending_subgoal` and
/// polls it non-blocking on the next turn.
///
/// # Note
///
/// This handle is intentionally untracked — `BackgroundSupervisor` does not support typed
/// result retrieval. The handle is stored and polled via `JoinHandle::is_finished` +
/// `FutureExt::now_or_never`.
fn spawn_subgoal_extraction(
    provider: AnyProvider,
    recent: Vec<(Role, String)>,
) -> tokio::task::JoinHandle<Option<crate::agent::state::SubgoalExtractionResult>> {
    tokio::spawn(async move {
        use zeph_llm::provider::{Message, MessageMetadata, Role};

        if recent.is_empty() {
            return None;
        }

        let mut context_text = String::new();
        for (role, content) in &recent {
            let role_str = match role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::System => "system",
            };
            let preview = if content.len() > 300 {
                let end = content.floor_char_boundary(300);
                &content[..end]
            } else {
                content.as_str()
            };
            let _ = std::fmt::write(&mut context_text, format_args!("[{role_str}]: {preview}\n"));
        }

        let prompt = format!(
            "Given this conversation excerpt, identify the agent's CURRENT subgoal in one \
             sentence. A subgoal is the immediate objective the agent is working toward right \
             now, not the overall task.\n\n\
             If the agent just completed a subgoal (answered a question, finished a subtask), \
             also state the COMPLETED subgoal.\n\n\
             Respond in this exact format:\n\
             CURRENT: <one sentence describing current subgoal>\n\
             COMPLETED: <one sentence describing just-completed subgoal, or NONE>\n\n\
             <conversation>\n{context_text}</conversation>"
        );

        let msgs = [Message {
            role: Role::User,
            content: prompt,
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let response =
            match tokio::time::timeout(std::time::Duration::from_secs(30), provider.chat(&msgs))
                .await
            {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    tracing::debug!("subgoal_extraction: LLM error: {e:#}");
                    return None;
                }
                Err(_) => {
                    tracing::debug!("subgoal_extraction: timed out");
                    return None;
                }
            };

        Some(parse_subgoal_extraction_response(&response))
    })
}

/// Parse the structured LLM response for subgoal extraction.
///
/// Expected format:
/// ```text
/// CURRENT: <description>
/// COMPLETED: <description or NONE>
/// ```
///
/// Falls back to treating the entire response as the current subgoal on malformed input.
pub(super) fn parse_subgoal_extraction_response(
    response: &str,
) -> crate::agent::state::SubgoalExtractionResult {
    use crate::agent::state::SubgoalExtractionResult;

    let trimmed = response.trim();

    // Try to extract CURRENT: and COMPLETED: prefixes.
    if let Some(current_pos) = trimmed.find("CURRENT:") {
        let after_current = &trimmed[current_pos + "CURRENT:".len()..];
        let (current_line_raw, remainder_raw) = after_current
            .split_once('\n')
            .map_or((after_current, ""), |(l, r)| (l, r));
        let current_line = current_line_raw.trim();
        let remainder = remainder_raw.trim();

        if current_line.is_empty() {
            // Malformed: treat entire response as current subgoal.
            return SubgoalExtractionResult {
                current: trimmed.to_string(),
                completed: None,
            };
        }

        let current = current_line.to_string();

        let completed = if let Some(comp_pos) = remainder.find("COMPLETED:") {
            let comp_text = remainder[comp_pos + "COMPLETED:".len()..].trim();
            let comp_line = comp_text
                .split('\n')
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if comp_line.is_empty() || comp_line.eq_ignore_ascii_case("none") {
                None
            } else {
                Some(comp_line)
            }
        } else {
            None
        };

        return SubgoalExtractionResult { current, completed };
    }

    // Malformed response: treat entire response as current subgoal.
    SubgoalExtractionResult {
        current: trimmed.to_string(),
        completed: None,
    }
}
