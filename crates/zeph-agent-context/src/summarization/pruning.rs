// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tool output pruning strategies.
//!
//! Dispatches to the configured strategy: `Reactive` (oldest-first), `TaskAware` (scored),
//! `Mig` (relevance − redundancy), `Subgoal` (tier-aware), and `SubgoalMig` (hybrid).
//!
//! All pruning paths skip focus-pinned Knowledge block messages and respect the tail-protection
//! budget (`prune_protect_tokens`).

use zeph_context::summarization::extract_overflow_ref;
use zeph_llm::provider::MessagePart;

use crate::compaction::{
    BlockScore, score_blocks_mig, score_blocks_subgoal, score_blocks_subgoal_mig,
    score_blocks_task_aware,
};
use crate::state::ContextSummarizationView;

/// Prune tool output bodies using the configured strategy.
///
/// Dispatches to scored pruning when `pruning_strategy` is not `Reactive`. Falls back to
/// oldest-first when the strategy is `Reactive` or no task goal is available.
///
/// Returns the number of tokens freed.
pub(crate) fn prune_tool_outputs(
    summ: &mut ContextSummarizationView<'_>,
    min_to_free: usize,
) -> usize {
    use zeph_config::PruningStrategy;
    match &summ.context_manager.compression.pruning_strategy {
        PruningStrategy::TaskAware => prune_tool_outputs_scored(summ, min_to_free),
        PruningStrategy::Mig => prune_tool_outputs_mig(summ, min_to_free),
        PruningStrategy::Subgoal => prune_tool_outputs_subgoal(summ, min_to_free),
        PruningStrategy::SubgoalMig => prune_tool_outputs_subgoal_mig(summ, min_to_free),
        PruningStrategy::Reactive => prune_tool_outputs_oldest_first(summ, min_to_free),
    }
}

/// Oldest-first (Reactive) tool output pruning.
///
/// Returns the number of tokens freed.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn prune_tool_outputs_oldest_first(
    summ: &mut ContextSummarizationView<'_>,
    min_to_free: usize,
) -> usize {
    let protect = summ.context_manager.prune_protect_tokens;
    let mut tail_tokens = 0usize;
    let mut protection_boundary = summ.messages.len();
    if protect > 0 {
        for (i, msg) in summ.messages.iter().enumerate().rev() {
            tail_tokens += summ.token_counter.count_message_tokens(msg);
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
    for msg in &mut summ.messages[..protection_boundary] {
        if freed >= min_to_free {
            break;
        }
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
                && !body.starts_with("[archived:")
            {
                freed += summ.token_counter.count_tokens(body);
                let ref_notice = extract_overflow_ref(body)
                    .map(|p| format!("[tool output pruned; use read_overflow {p} to retrieve]"))
                    .unwrap_or_default();
                freed -= summ.token_counter.count_tokens(&ref_notice);
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
        // TODO(review): metric increment (tool_output_prunes) not tracked without MetricsCallback
        tracing::info!(freed, protection_boundary, "pruned tool outputs");
    }
    freed
}

/// Compute the protection boundary index.
///
/// Messages at or after the returned index must not be evicted by scored pruning paths.
fn prune_protection_boundary(summ: &ContextSummarizationView<'_>) -> usize {
    let protect = summ.context_manager.prune_protect_tokens;
    if protect == 0 {
        return summ.messages.len();
    }
    let mut tail_tokens = 0usize;
    let mut boundary = summ.messages.len();
    for (i, msg) in summ.messages.iter().enumerate().rev() {
        tail_tokens += summ.token_counter.count_message_tokens(msg);
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

/// Task-aware / MIG pruning: evict lowest-relevance blocks first.
fn prune_tool_outputs_scored(summ: &mut ContextSummarizationView<'_>, min_to_free: usize) -> usize {
    let goal = summ.current_task_goal.clone();
    let mut scores = if let Some(ref goal) = goal {
        score_blocks_task_aware(summ.messages, goal, &summ.token_counter)
    } else {
        return prune_tool_outputs_oldest_first(summ, min_to_free);
    };

    scores.sort_unstable_by(|a, b| {
        a.relevance
            .partial_cmp(&b.relevance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    evict_sorted_blocks(summ, &scores, min_to_free, "task_aware")
}

/// MIG-scored pruning: evict blocks with the lowest marginal information gain.
fn prune_tool_outputs_mig(summ: &mut ContextSummarizationView<'_>, min_to_free: usize) -> usize {
    let goal = summ.current_task_goal.as_deref();
    let mut scores = score_blocks_mig(summ.messages, goal, &summ.token_counter);
    scores.sort_unstable_by(|a, b| {
        a.mig
            .partial_cmp(&b.mig)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    evict_sorted_blocks(summ, &scores, min_to_free, "mig")
}

/// Subgoal-aware pruning: evict outdated/completed subgoal tool outputs first.
fn prune_tool_outputs_subgoal(
    summ: &mut ContextSummarizationView<'_>,
    min_to_free: usize,
) -> usize {
    let mut scores =
        score_blocks_subgoal(summ.messages, summ.subgoal_registry, &summ.token_counter);
    scores.sort_unstable_by(|a, b| {
        a.relevance
            .partial_cmp(&b.relevance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    evict_sorted_blocks(summ, &scores, min_to_free, "subgoal")
}

/// Subgoal + MIG hybrid pruning.
fn prune_tool_outputs_subgoal_mig(
    summ: &mut ContextSummarizationView<'_>,
    min_to_free: usize,
) -> usize {
    let mut scores =
        score_blocks_subgoal_mig(summ.messages, summ.subgoal_registry, &summ.token_counter);
    scores.sort_unstable_by(|a, b| {
        a.mig
            .partial_cmp(&b.mig)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    evict_sorted_blocks(summ, &scores, min_to_free, "subgoal_mig")
}

/// Shared eviction loop over a pre-sorted `BlockScore` slice.
///
/// Skips focus-pinned messages and the protected tail. Returns tokens freed.
fn evict_sorted_blocks(
    summ: &mut ContextSummarizationView<'_>,
    sorted_scores: &[BlockScore],
    min_to_free: usize,
    strategy: &str,
) -> usize {
    let protection_boundary = prune_protection_boundary(summ);
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
        if block.msg_index >= protection_boundary {
            continue;
        }
        let msg = &mut summ.messages[block.msg_index];
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
                freed += summ.token_counter.count_tokens(body);
                let ref_notice = extract_overflow_ref(body)
                    .map(|p| format!("[tool output pruned; use read_overflow {p} to retrieve]"))
                    .unwrap_or_default();
                freed -= summ.token_counter.count_tokens(&ref_notice);
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
        summ.messages[idx].rebuild_content();
    }

    if freed > 0 {
        // TODO(review): metric increment (tool_output_prunes) not tracked without MetricsCallback
        tracing::info!(
            freed,
            pruned = pruned_indices.len(),
            strategy,
            "pruned tool outputs"
        );
    }
    freed
}
