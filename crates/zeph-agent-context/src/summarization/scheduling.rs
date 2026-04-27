// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Compaction scheduling: tiered compaction dispatch, proactive compression, and
//! background goal/subgoal extraction.
//!
//! The three-tier model:
//! - **Soft** — apply deferred summaries + prune tool outputs (no LLM).
//! - **Hard** — soft tier + LLM full summarization.
//! - **Proactive** — proactively compress before the hard threshold is reached.

use std::hash::Hash as _;

use zeph_common::task_supervisor::BlockingHandle;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{LlmProvider as _, Message, MessageMetadata, Role};

use crate::compaction::SubgoalExtractionResult;
use crate::state::ContextSummarizationView;
use zeph_context::budget::ContextBudget;
use zeph_context::manager::CompactionTier;

use super::deferred::apply_deferred_summaries;
use super::pruning::prune_tool_outputs;

/// Soft-only compaction for mid-iteration use inside tool execution loops.
///
/// Applies deferred tool summaries and prunes tool outputs down to the soft threshold.
/// Never triggers Hard tier (no LLM call), never increments `turns_since_last_hard_compaction`,
/// and never decrements the cooldown counter. Returns immediately when `compacted_this_turn`
/// is set or when context usage is below the soft threshold.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub(crate) fn maybe_soft_compact_mid_iteration(summ: &mut ContextSummarizationView<'_>) {
    if summ.context_manager.compaction.is_compacted_this_turn() {
        return;
    }
    if !matches!(
        summ.context_manager
            .compaction_tier(*summ.cached_prompt_tokens),
        CompactionTier::Soft | CompactionTier::Hard
    ) {
        return;
    }
    let budget = summ
        .context_manager
        .budget
        .as_ref()
        .map_or(0, ContextBudget::max_tokens);
    let soft_threshold = (budget as f32 * summ.context_manager.soft_compaction_threshold) as usize;
    let cached = usize::try_from(*summ.cached_prompt_tokens).unwrap_or(usize::MAX);

    apply_deferred_summaries(summ);
    let min_to_free = cached.saturating_sub(soft_threshold);
    if min_to_free > 0 {
        prune_tool_outputs(summ, min_to_free);
    }
    tracing::debug!(
        cached_tokens = *summ.cached_prompt_tokens,
        soft_threshold,
        "mid-iteration soft compaction complete"
    );
}

/// Refresh the cached task goal when the last user message has changed.
///
/// Two-phase non-blocking design:
/// - **Phase 1 (apply)**: if the background extraction task from last compaction has finished,
///   apply its result to `current_task_goal`.
/// - **Phase 2 (schedule)**: if the user message hash has changed and no task is in-flight,
///   spawn a new background extraction. Current compaction uses the cached goal.
///
/// Only runs when a `TaskAware` or `Mig` pruning strategy is active.
pub(crate) fn maybe_refresh_task_goal(summ: &mut ContextSummarizationView<'_>) {
    match &summ.context_manager.compression.pruning_strategy {
        zeph_config::PruningStrategy::Reactive
        | zeph_config::PruningStrategy::Subgoal
        | zeph_config::PruningStrategy::SubgoalMig => return,
        zeph_config::PruningStrategy::TaskAware | zeph_config::PruningStrategy::Mig => {}
    }

    // Phase 1: apply completed background result.
    if summ.pending_task_goal.is_some() {
        apply_completed_task_goal(summ);
    }

    // Phase 2: no task in flight — may schedule a new one.
    if summ.pending_task_goal.is_some() {
        return;
    }

    let Some(hash) = last_user_content_hash(summ.messages) else {
        return;
    };

    if *summ.task_goal_user_msg_hash == Some(hash) {
        return;
    }

    *summ.task_goal_user_msg_hash = Some(hash);
    let recent = recent_user_assistant_excerpt(summ.messages, 10, false);
    let provider = summ.summarization_deps.provider.clone();
    let handle = spawn_task_goal_extraction(provider, recent, &summ.task_supervisor);
    *summ.pending_task_goal = Some(handle);
    tracing::debug!("extract_task_goal: background task spawned");
    if let Some(ref tx) = summ.status_tx {
        let _ = tx.send("Extracting task goal...".into());
    }
}

/// Refresh the subgoal registry when the last user message has changed.
///
/// Mirrors the two-phase `maybe_refresh_task_goal` pattern.
/// Only runs when a `Subgoal` or `SubgoalMig` pruning strategy is active.
pub(crate) fn maybe_refresh_subgoal(summ: &mut ContextSummarizationView<'_>) {
    match &summ.context_manager.compression.pruning_strategy {
        zeph_config::PruningStrategy::Subgoal | zeph_config::PruningStrategy::SubgoalMig => {}
        _ => return,
    }

    let msg_len = summ.messages.len();

    // Phase 1: apply completed background result.
    if summ.pending_subgoal.is_some() {
        apply_completed_subgoal(summ, msg_len);
    }

    // Phase 2: no task in flight.
    if summ.pending_subgoal.is_some() {
        return;
    }

    let last_user_content = summ
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User && m.metadata.visibility.is_agent_visible())
        .map(|m| m.content.as_str())
        .unwrap_or_default();

    if last_user_content.is_empty() {
        return;
    }

    let hash = {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        last_user_content.hash(&mut hasher);
        std::hash::Hasher::finish(&hasher)
    };

    if *summ.subgoal_user_msg_hash == Some(hash) {
        return;
    }
    *summ.subgoal_user_msg_hash = Some(hash);

    let recent = recent_user_assistant_excerpt(summ.messages, 6, true);
    let provider = summ.summarization_deps.provider.clone();
    let handle = spawn_subgoal_extraction(provider, recent, &summ.task_supervisor);
    *summ.pending_subgoal = Some(handle);
    tracing::debug!("subgoal_extraction: background task spawned");
    if let Some(ref tx) = summ.status_tx {
        let _ = tx.send("Tracking subgoal...".into());
    }
}

// ── Private helpers ──────────────────────────────────────────────────────────

/// Apply a completed background task-goal extraction result to `current_task_goal`.
///
/// Re-stores the handle if the task is not yet complete so the caller can check again
/// next turn.
fn apply_completed_task_goal(summ: &mut ContextSummarizationView<'_>) {
    if let Some(handle) = summ.pending_task_goal.take() {
        match handle.try_join() {
            Ok(Ok(Some(goal))) => {
                tracing::debug!("extract_task_goal: background result applied");
                *summ.current_task_goal = Some(goal);
            }
            Ok(Ok(None)) => {}
            Ok(Err(e)) => tracing::debug!("extract_task_goal: task error: {e}"),
            Err(handle) => {
                *summ.pending_task_goal = Some(handle);
                return;
            }
        }
        // Clear spinner on all completion paths.
        if let Some(ref tx) = summ.status_tx {
            let _ = tx.send(String::new());
        }
    }
}

/// Apply a completed background subgoal extraction result to the registry.
fn apply_completed_subgoal(summ: &mut ContextSummarizationView<'_>, msg_len: usize) {
    if let Some(handle) = summ.pending_subgoal.take() {
        match handle.try_join() {
            Ok(Ok(Some(result))) => {
                let is_transition = result.completed.is_some();
                if is_transition {
                    register_subgoal_transition(summ, &result, msg_len);
                } else {
                    register_subgoal_continuation(summ, &result, msg_len);
                }
            }
            Ok(Ok(None)) => {}
            Ok(Err(e)) => tracing::debug!("subgoal_extraction: task error: {e}"),
            Err(handle) => {
                *summ.pending_subgoal = Some(handle);
                return;
            }
        }
        if let Some(ref tx) = summ.status_tx {
            let _ = tx.send(String::new());
        }
    }
}

fn register_subgoal_transition(
    summ: &mut ContextSummarizationView<'_>,
    result: &SubgoalExtractionResult,
    msg_len: usize,
) {
    if let Some(completed_desc) = &result.completed {
        tracing::debug!(
            completed = completed_desc.as_str(),
            "subgoal transition detected"
        );
    }
    summ.subgoal_registry
        .complete_active(msg_len.saturating_sub(1));
    let new_id = summ
        .subgoal_registry
        .push_active(result.current.clone(), msg_len.saturating_sub(1));
    summ.subgoal_registry
        .extend_active(msg_len.saturating_sub(1));
    tracing::debug!(
        current = result.current.as_str(),
        id = new_id.0,
        "new active subgoal registered"
    );
}

fn register_subgoal_continuation(
    summ: &mut ContextSummarizationView<'_>,
    result: &SubgoalExtractionResult,
    msg_len: usize,
) {
    let is_first = summ.subgoal_registry.subgoals.is_empty();
    if is_first {
        let id = summ
            .subgoal_registry
            .push_active(result.current.clone(), msg_len.saturating_sub(1));
        if msg_len > 2 {
            summ.subgoal_registry.tag_range(1, msg_len - 2, id);
        }
        summ.subgoal_registry
            .extend_active(msg_len.saturating_sub(1));
        tracing::debug!(
            current = result.current.as_str(),
            id = id.0,
            retroactive_msgs = msg_len.saturating_sub(2),
            "first subgoal registered with retroactive tagging"
        );
    } else {
        summ.subgoal_registry
            .extend_active(msg_len.saturating_sub(1));
        tracing::debug!(current = result.current.as_str(), "active subgoal extended");
    }
}

/// Compute a hash of the last user message content.
fn last_user_content_hash(messages: &[Message]) -> Option<u64> {
    let content = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
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
fn recent_user_assistant_excerpt(
    messages: &[Message],
    take: usize,
    agent_visible_only: bool,
) -> Vec<(Role, String)> {
    messages
        .iter()
        .filter(|m| {
            let role_ok = matches!(m.role, Role::User | Role::Assistant);
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

/// Parse the structured LLM response for subgoal extraction.
///
/// Expected format:
/// ```text
/// CURRENT: <description>
/// COMPLETED: <description or NONE>
/// ```
///
/// Falls back to treating the entire response as the current subgoal on malformed input.
#[must_use]
pub fn parse_subgoal_extraction_response(response: &str) -> SubgoalExtractionResult {
    let trimmed = response.trim();

    if let Some(current_pos) = trimmed.find("CURRENT:") {
        let after_current = &trimmed[current_pos + "CURRENT:".len()..];
        let (current_line_raw, remainder_raw) = after_current
            .split_once('\n')
            .map_or((after_current, ""), |(l, r)| (l, r));
        let current_line = current_line_raw.trim();
        let remainder = remainder_raw.trim();

        if current_line.is_empty() {
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

    SubgoalExtractionResult {
        current: trimmed.to_string(),
        completed: None,
    }
}

/// Spawn a background task-goal extraction task.
fn spawn_task_goal_extraction(
    provider: AnyProvider,
    recent: Vec<(Role, String)>,
    supervisor: &std::sync::Arc<zeph_common::TaskSupervisor>,
) -> BlockingHandle<Option<String>> {
    let task = async move {
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
                    if trimmed.len() > MAX_GOAL_CHARS {
                        tracing::warn!(
                            len = trimmed.len(),
                            "extract_task_goal: LLM returned oversized goal; truncating"
                        );
                        let end = trimmed.floor_char_boundary(MAX_GOAL_CHARS);
                        Some(trimmed[..end].to_string())
                    } else {
                        Some(trimmed.to_string())
                    }
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
    };
    spawn_oneshot(
        supervisor,
        std::sync::Arc::from("agent.compaction.task_goal"),
        move || task,
    )
}

/// Spawn a background subgoal extraction task.
fn spawn_subgoal_extraction(
    provider: AnyProvider,
    recent: Vec<(Role, String)>,
    supervisor: &std::sync::Arc<zeph_common::TaskSupervisor>,
) -> BlockingHandle<Option<SubgoalExtractionResult>> {
    let task = async move {
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
    };
    spawn_oneshot(
        supervisor,
        std::sync::Arc::from("agent.compaction.subgoal"),
        move || task,
    )
}

fn spawn_oneshot<F, Fut, R>(
    supervisor: &std::sync::Arc<zeph_common::TaskSupervisor>,
    name: std::sync::Arc<str>,
    factory: F,
) -> BlockingHandle<R>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = R> + Send + 'static,
    R: Send + 'static,
{
    supervisor.spawn_oneshot(name, factory)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_well_formed_with_both() {
        let response = "CURRENT: Implement login\nCOMPLETED: Setup database";
        let result = parse_subgoal_extraction_response(response);
        assert_eq!(result.current, "Implement login");
        assert_eq!(result.completed, Some("Setup database".to_string()));
    }

    #[test]
    fn parse_well_formed_no_completed() {
        let response = "CURRENT: Fetch user data\nCOMPLETED: NONE";
        let result = parse_subgoal_extraction_response(response);
        assert_eq!(result.current, "Fetch user data");
        assert_eq!(result.completed, None);
    }

    #[test]
    fn parse_malformed_no_current_prefix() {
        let response = "Just some random text about subgoals";
        let result = parse_subgoal_extraction_response(response);
        assert_eq!(result.current, "Just some random text about subgoals");
        assert_eq!(result.completed, None);
    }

    #[test]
    fn parse_malformed_empty_current() {
        let response = "CURRENT: \nCOMPLETED: Setup";
        let result = parse_subgoal_extraction_response(response);
        assert_eq!(result.current.trim(), "CURRENT: \nCOMPLETED: Setup");
        assert_eq!(result.completed, None);
    }
}
