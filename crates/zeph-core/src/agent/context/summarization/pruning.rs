// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_context::summarization::extract_overflow_ref;
use zeph_llm::provider::MessagePart;

use crate::agent::Agent;
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    /// Inline pruning for tool loops: clear tool output bodies from messages
    /// older than the last `keep_recent` messages. Called after each tool iteration
    /// to prevent context growth during long tool loops.
    ///
    /// # Invariant
    ///
    /// This method MUST be called AFTER `maybe_summarize_tool_pair()`. The summarizer
    /// reads `msg.content` to build the LLM prompt; pruning replaces that content with
    /// `"[pruned]"`. Calling prune first would cause the summarizer to produce useless
    /// summaries. After summarization, the processed pair has `deferred_summary` set and
    /// is skipped by `count_unsummarized_pairs`. The pruning loop may still clear their
    /// bodies for token savings, but the content has already been captured in the summary.
    pub(crate) fn prune_stale_tool_outputs(&mut self, keep_recent: usize) -> usize {
        if self.msg.messages.len() <= keep_recent + 1 {
            return 0;
        }
        let boundary = self.msg.messages.len().saturating_sub(keep_recent);
        let mut freed = 0usize;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .cast_signed();
        // Skip system prompt (index 0), prune from 1..boundary.
        // Also skip focus-pinned Knowledge block messages (#1850 S1 fix).
        for msg in &mut self.msg.messages[1..boundary] {
            if msg.metadata.focus_pinned {
                continue;
            }
            let mut modified = false;
            for part in &mut msg.parts {
                match part {
                    MessagePart::ToolOutput {
                        body, compacted_at, ..
                    } if compacted_at.is_none() && !body.is_empty() => {
                        freed += self.runtime.metrics.token_counter.count_tokens(body);
                        let ref_notice = extract_overflow_ref(body)
                            .map(|p| {
                                format!("[tool output pruned; use read_overflow {p} to retrieve]")
                            })
                            .unwrap_or_default();
                        freed -= self.runtime.metrics.token_counter.count_tokens(&ref_notice);
                        *compacted_at = Some(now);
                        *body = ref_notice;
                        modified = true;
                    }
                    MessagePart::ToolResult { content, .. } => {
                        let tokens = self.runtime.metrics.token_counter.count_tokens(content);
                        if tokens > 20 {
                            freed += tokens;
                            let ref_notice = extract_overflow_ref(content).map_or_else(
                                || String::from("[pruned]"),
                                |p| {
                                    format!(
                                        "[tool output pruned; use read_overflow {p} to retrieve]"
                                    )
                                },
                            );
                            freed -= self.runtime.metrics.token_counter.count_tokens(&ref_notice);
                            *content = ref_notice;
                            modified = true;
                        }
                    }
                    _ => {}
                }
            }
            if modified {
                msg.rebuild_content();
            }
        }
        if freed > 0 {
            self.update_metrics(|m| m.tool_output_prunes += 1);
            tracing::debug!(
                freed,
                boundary,
                keep_recent,
                "inline pruned stale tool outputs"
            );
        }
        freed
    }
}

// ── Test-only pruning helpers ─────────────────────────────────────────────────
//
// Production pruning goes through ContextService (via ContextSummarizationView) which
// calls the free functions in `zeph-agent-context::summarization::pruning` directly.
// These Agent<C> wrappers exist solely for integration tests in context/tests/ that
// test pruning invariants on the Agent<C> surface before the test suite is migrated
// to use the service path directly.
//
// TODO(review): migrate context/tests/ pruning tests to use ContextService path, then
// delete this cfg(test) impl block entirely.
#[cfg(test)]
impl<C: Channel> Agent<C> {
    /// Prune tool output bodies.
    ///
    /// Dispatches to scored pruning when `context-compression` is enabled and the configured
    /// pruning strategy is not `Reactive`. Falls back to oldest-first when the feature is
    /// disabled or the strategy is `Reactive`.
    ///
    /// Returns the number of tokens freed.
    pub(in crate::agent) fn prune_tool_outputs(&mut self, min_to_free: usize) -> usize {
        {
            use crate::config::PruningStrategy;
            match &self.context_manager.compression.pruning_strategy {
                PruningStrategy::TaskAware => {
                    return self.prune_tool_outputs_scored(min_to_free);
                }
                PruningStrategy::Mig => {
                    return self.prune_tool_outputs_mig(min_to_free);
                }
                PruningStrategy::Subgoal => {
                    return self.prune_tool_outputs_subgoal(min_to_free);
                }
                PruningStrategy::SubgoalMig => {
                    return self.prune_tool_outputs_subgoal_mig(min_to_free);
                }
                PruningStrategy::Reactive => {} // fall through to oldest-first
            }
        }
        self.prune_tool_outputs_oldest_first(min_to_free)
    }

    /// Oldest-first (Reactive) tool output pruning.
    ///
    /// This is the non-dispatching inner implementation. Called directly by the dispatcher
    /// when strategy is `Reactive` and by scored strategies as their fallback — the latter
    /// avoids the infinite recursion that would occur if they called `prune_tool_outputs`.
    #[allow(clippy::cast_precision_loss)]
    pub(super) fn prune_tool_outputs_oldest_first(&mut self, min_to_free: usize) -> usize {
        let protect = self.context_manager.prune_protect_tokens;
        let mut tail_tokens = 0usize;
        let mut protection_boundary = self.msg.messages.len();
        if protect > 0 {
            for (i, msg) in self.msg.messages.iter().enumerate().rev() {
                tail_tokens += self.runtime.metrics.token_counter.count_message_tokens(msg);
                if tail_tokens >= protect {
                    protection_boundary = i;
                    break;
                }
                if i == 0 {
                    protection_boundary = 0;
                }
            }
        }

        let mut freed = 0usize;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .cast_signed();
        for msg in &mut self.msg.messages[..protection_boundary] {
            if freed >= min_to_free {
                break;
            }
            // S1 fix: never prune pinned Knowledge block messages (#1850).
            if msg.metadata.focus_pinned {
                continue;
            }
            let mut modified = false;
            for part in &mut msg.parts {
                if let &mut MessagePart::ToolOutput {
                    ref mut body,
                    ref mut compacted_at,
                    ..
                } = part
                    && compacted_at.is_none()
                    && !body.is_empty()
                    // Skip already-archived bodies — they are tiny and irretrievable if pruned.
                    && !body.starts_with("[archived:")
                {
                    freed += self.runtime.metrics.token_counter.count_tokens(body);
                    let ref_notice = extract_overflow_ref(body)
                        .map(|p| format!("[tool output pruned; use read_overflow {p} to retrieve]"))
                        .unwrap_or_default();
                    freed -= self.runtime.metrics.token_counter.count_tokens(&ref_notice);
                    *compacted_at = Some(now);
                    *body = ref_notice;
                    modified = true;
                }
            }
            if modified {
                msg.rebuild_content();
            }
        }

        if freed > 0 {
            self.update_metrics(|m| m.tool_output_prunes += 1);
            tracing::info!(freed, protection_boundary, "pruned tool outputs");
        }
        freed
    }

    /// Compute the protection boundary index for `prune_protect_tokens`.
    ///
    /// Messages at or after the returned index must not be evicted. This mirrors the logic in
    /// `prune_tool_outputs_oldest_first` so all pruning paths enforce the same tail protection.
    fn prune_protection_boundary(&self) -> usize {
        let protect = self.context_manager.prune_protect_tokens;
        if protect == 0 {
            return self.msg.messages.len();
        }
        let mut tail_tokens = 0usize;
        let mut boundary = self.msg.messages.len();
        for (i, msg) in self.msg.messages.iter().enumerate().rev() {
            tail_tokens += self.runtime.metrics.token_counter.count_message_tokens(msg);
            if tail_tokens >= protect {
                boundary = i;
                break;
            }
            if i == 0 {
                boundary = 0;
            }
        }
        boundary
    }

    /// Task-aware / MIG pruning: score tool outputs by relevance to the current task goal,
    /// then evict lowest-scoring blocks until `min_to_free` tokens are freed.
    ///
    /// Requires `context-compression` feature. Falls back to `prune_tool_outputs()` otherwise.
    ///
    /// ## `SideQuest` interaction contract (S3 from critic review)
    ///
    /// When both `TaskAware` pruning and `SideQuest` are enabled, `SideQuest` is expected to be
    /// disabled by the caller (set `sidequest.enabled = false` when `pruning_strategy` != Reactive).
    /// This is the "Option A" documented in the critic review: the two systems do not share state
    /// at the pruning level. `SideQuest` uses the same `focus_pinned` protection to avoid evicting
    /// Knowledge block content.
    pub(in crate::agent) fn prune_tool_outputs_scored(&mut self, min_to_free: usize) -> usize {
        use crate::config::PruningStrategy;
        use zeph_agent_context::score_blocks_task_aware;

        let goal = match &self.context_manager.compression.pruning_strategy {
            PruningStrategy::TaskAware => self.services.compression.current_task_goal.clone(),
            _ => None,
        };

        let scores = if let Some(ref goal) = goal {
            score_blocks_task_aware(
                &self.msg.messages,
                goal,
                &self.runtime.metrics.token_counter,
            )
        } else {
            // No goal available: fall back to oldest-first directly (not through the
            // dispatcher, which would recurse back here — S4 fix).
            return self.prune_tool_outputs_oldest_first(min_to_free);
        };

        if let Some(ref d) = self.runtime.debug.debug_dumper {
            d.dump_pruning_scores(&scores);
        }

        // Sort ascending by score: lowest relevance first (best eviction candidates)
        let mut sorted_scores = scores;
        sorted_scores.sort_unstable_by(|a, b| {
            a.relevance
                .partial_cmp(&b.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let protection_boundary = self.prune_protection_boundary();

        let mut freed = 0usize;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .cast_signed();

        let mut pruned_indices = Vec::new();
        for block in &sorted_scores {
            if freed >= min_to_free {
                break;
            }
            // Respect prune_protect_tokens: skip messages in the protected tail.
            if block.msg_index >= protection_boundary {
                continue;
            }
            let msg = &mut self.msg.messages[block.msg_index];
            if msg.metadata.focus_pinned {
                continue;
            }
            let mut modified = false;
            for part in &mut msg.parts {
                if let MessagePart::ToolOutput {
                    body, compacted_at, ..
                } = part
                    && compacted_at.is_none()
                    && !body.is_empty()
                {
                    freed += self.runtime.metrics.token_counter.count_tokens(body);
                    let ref_notice = extract_overflow_ref(body)
                        .map(|p| format!("[tool output pruned; use read_overflow {p} to retrieve]"))
                        .unwrap_or_default();
                    freed -= self.runtime.metrics.token_counter.count_tokens(&ref_notice);
                    *compacted_at = Some(now);
                    *body = ref_notice;
                    modified = true;
                }
            }
            if modified {
                pruned_indices.push(block.msg_index);
            }
        }

        for &idx in &pruned_indices {
            self.msg.messages[idx].rebuild_content();
        }

        if freed > 0 {
            tracing::info!(
                freed,
                pruned = pruned_indices.len(),
                strategy = "task_aware",
                "task-aware pruned tool outputs"
            );
            self.update_metrics(|m| m.tool_output_prunes += 1);
        }
        freed
    }

    /// MIG-scored pruning. Uses relevance − redundancy scoring to identify the best eviction
    /// candidates. Requires `context-compression` feature.
    pub(in crate::agent) fn prune_tool_outputs_mig(&mut self, min_to_free: usize) -> usize {
        use zeph_agent_context::score_blocks_mig;

        let goal = self.services.compression.current_task_goal.as_deref();
        let mut scores = score_blocks_mig(
            &self.msg.messages,
            goal,
            &self.runtime.metrics.token_counter,
        );

        if let Some(ref d) = self.runtime.debug.debug_dumper {
            d.dump_pruning_scores(&scores);
        }

        // Sort ascending by MIG: most negative MIG = highest eviction priority
        scores.sort_unstable_by(|a, b| {
            a.mig
                .partial_cmp(&b.mig)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let protection_boundary = self.prune_protection_boundary();

        let mut freed = 0usize;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .cast_signed();

        let mut pruned_indices = Vec::new();
        for block in &scores {
            if freed >= min_to_free {
                break;
            }
            // Respect prune_protect_tokens: skip messages in the protected tail.
            if block.msg_index >= protection_boundary {
                continue;
            }
            let msg = &mut self.msg.messages[block.msg_index];
            if msg.metadata.focus_pinned {
                continue;
            }
            let mut modified = false;
            for part in &mut msg.parts {
                if let MessagePart::ToolOutput {
                    body, compacted_at, ..
                } = part
                    && compacted_at.is_none()
                    && !body.is_empty()
                {
                    freed += self.runtime.metrics.token_counter.count_tokens(body);
                    let ref_notice = extract_overflow_ref(body)
                        .map(|p| format!("[tool output pruned; use read_overflow {p} to retrieve]"))
                        .unwrap_or_default();
                    freed -= self.runtime.metrics.token_counter.count_tokens(&ref_notice);
                    *compacted_at = Some(now);
                    *body = ref_notice;
                    modified = true;
                }
            }
            if modified {
                pruned_indices.push(block.msg_index);
            }
        }

        for &idx in &pruned_indices {
            self.msg.messages[idx].rebuild_content();
        }

        if freed > 0 {
            tracing::info!(
                freed,
                pruned = pruned_indices.len(),
                strategy = "mig",
                "MIG-pruned tool outputs"
            );
            self.update_metrics(|m| m.tool_output_prunes += 1);
        }
        freed
    }

    /// Subgoal-aware pruning: score tool outputs by subgoal tier membership and evict
    /// lowest-scoring blocks (outdated first, then completed, never active).
    ///
    /// Active-subgoal tool outputs receive relevance 1.0 and are effectively protected
    /// from eviction as long as lower-tier outputs can satisfy `min_to_free`.
    pub(in crate::agent) fn prune_tool_outputs_subgoal(&mut self, min_to_free: usize) -> usize {
        use zeph_agent_context::score_blocks_subgoal;

        if let Some(ref d) = self.runtime.debug.debug_dumper {
            d.dump_subgoal_registry(&self.services.compression.subgoal_registry);
        }

        let scores = score_blocks_subgoal(
            &self.msg.messages,
            &self.services.compression.subgoal_registry,
            &self.runtime.metrics.token_counter,
        );

        if let Some(ref d) = self.runtime.debug.debug_dumper {
            d.dump_pruning_scores(&scores);
        }

        // Sort ascending: lowest relevance = highest eviction priority.
        let mut sorted = scores;
        sorted.sort_unstable_by(|a, b| {
            a.relevance
                .partial_cmp(&b.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        self.evict_sorted_blocks(&sorted, min_to_free, "subgoal")
    }

    /// Subgoal + MIG hybrid pruning: combines subgoal tier relevance with pairwise
    /// redundancy scoring (MIG = relevance − redundancy).
    pub(in crate::agent) fn prune_tool_outputs_subgoal_mig(&mut self, min_to_free: usize) -> usize {
        use zeph_agent_context::score_blocks_subgoal_mig;

        if let Some(ref d) = self.runtime.debug.debug_dumper {
            d.dump_subgoal_registry(&self.services.compression.subgoal_registry);
        }

        let mut scores = score_blocks_subgoal_mig(
            &self.msg.messages,
            &self.services.compression.subgoal_registry,
            &self.runtime.metrics.token_counter,
        );

        if let Some(ref d) = self.runtime.debug.debug_dumper {
            d.dump_pruning_scores(&scores);
        }

        // Sort ascending by MIG: most negative MIG = highest eviction priority.
        scores.sort_unstable_by(|a, b| {
            a.mig
                .partial_cmp(&b.mig)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        self.evict_sorted_blocks(&scores, min_to_free, "subgoal_mig")
    }

    /// Shared eviction loop: given a sorted `BlockScore` slice (ascending priority = most evictable
    /// first), evict tool outputs until `min_to_free` tokens are freed or all candidates are
    /// exhausted. Returns tokens freed.
    ///
    /// Extracted to eliminate duplicate code between `prune_tool_outputs_scored`,
    /// `prune_tool_outputs_mig`, and the new subgoal pruning variants.
    fn evict_sorted_blocks(
        &mut self,
        sorted_scores: &[zeph_agent_context::BlockScore],
        min_to_free: usize,
        strategy: &str,
    ) -> usize {
        let mut freed = 0usize;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .cast_signed();

        let mut pruned_indices = Vec::new();
        for block in sorted_scores {
            if freed >= min_to_free {
                break;
            }
            let msg = &mut self.msg.messages[block.msg_index];
            if msg.metadata.focus_pinned {
                continue;
            }
            let mut modified = false;
            for part in &mut msg.parts {
                if let MessagePart::ToolOutput {
                    body, compacted_at, ..
                } = part
                    && compacted_at.is_none()
                    && !body.is_empty()
                {
                    freed += self.runtime.metrics.token_counter.count_tokens(body);
                    let ref_notice = extract_overflow_ref(body)
                        .map(|p| format!("[tool output pruned; use read_overflow {p} to retrieve]"))
                        .unwrap_or_default();
                    freed -= self.runtime.metrics.token_counter.count_tokens(&ref_notice);
                    *compacted_at = Some(now);
                    *body = ref_notice;
                    modified = true;
                }
            }
            if modified {
                pruned_indices.push(block.msg_index);
            }
        }

        for &idx in &pruned_indices {
            self.msg.messages[idx].rebuild_content();
        }

        if freed > 0 {
            tracing::info!(
                freed,
                pruned = pruned_indices.len(),
                strategy,
                "pruned tool outputs"
            );
            self.update_metrics(|m| m.tool_output_prunes += 1);
        }
        freed
    }
}
