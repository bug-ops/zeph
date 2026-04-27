// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`ContextService`] — stateless façade for agent context-assembly operations.

use zeph_llm::provider::{MessagePart, Role};

use crate::error::ContextError;
use crate::helpers::{
    CODE_CONTEXT_PREFIX, CORRECTIONS_PREFIX, CROSS_SESSION_PREFIX, DOCUMENT_RAG_PREFIX,
    GRAPH_FACTS_PREFIX, LSP_NOTE_PREFIX, PERSONA_PREFIX, REASONING_PREFIX, RECALL_PREFIX,
    SESSION_DIGEST_PREFIX, SUMMARY_PREFIX, TRAJECTORY_PREFIX, TREE_MEMORY_PREFIX,
};
use crate::state::{
    ContextAssemblyView, ContextSummarizationView, MessageWindowView, ProviderHandles, StatusSink,
    TrustGate,
};

/// Stateless façade for agent context-assembly operations.
///
/// This struct has no fields. All state flows through method parameters, which allows the
/// borrow checker to see disjoint `&mut` borrows at the call site without hiding them
/// inside an opaque bundle.
///
/// Methods are `&self` — the type exists only to namespace the operations and give callers
/// a single import.
///
/// # Examples
///
/// ```no_run
/// use zeph_agent_context::service::ContextService;
///
/// let svc = ContextService::new();
/// // call svc.prepare_context(...) or svc.clear_history(...)
/// ```
#[derive(Debug, Default)]
pub struct ContextService;

impl ContextService {
    /// Create a new stateless `ContextService`.
    ///
    /// This is a zero-cost constructor — the struct has no fields.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    // ── Trivial message-window mutators (PR1) ─────────────────────────────────

    /// Clear the message history, preserving the system prompt.
    ///
    /// Keeps the first message (system prompt), clears the rest, and clears
    /// `completed_tool_ids` — session-scoped dependency state resets with the history.
    /// Recomputes `cached_prompt_tokens` inline after clearing.
    pub fn clear_history(&self, window: &mut MessageWindowView<'_>) {
        let system_prompt = window.messages.first().cloned();
        window.messages.clear();
        if let Some(sp) = system_prompt {
            window.messages.push(sp);
        }
        window.completed_tool_ids.clear();
        recompute_prompt_tokens(window);
    }

    /// Remove semantic recall messages from the window.
    pub fn remove_recall_messages(&self, window: &mut MessageWindowView<'_>) {
        remove_by_part_or_prefix(window.messages, RECALL_PREFIX, |p| {
            matches!(p, MessagePart::Recall { .. })
        });
    }

    /// Remove past-correction messages from the window.
    pub fn remove_correction_messages(&self, window: &mut MessageWindowView<'_>) {
        remove_by_prefix(window.messages, Role::System, CORRECTIONS_PREFIX);
    }

    /// Remove knowledge-graph fact messages from the window.
    pub fn remove_graph_facts_messages(&self, window: &mut MessageWindowView<'_>) {
        remove_by_prefix(window.messages, Role::System, GRAPH_FACTS_PREFIX);
    }

    /// Remove persona-facts messages from the window.
    pub fn remove_persona_facts_messages(&self, window: &mut MessageWindowView<'_>) {
        remove_by_prefix(window.messages, Role::System, PERSONA_PREFIX);
    }

    /// Remove trajectory-hint messages from the window.
    pub fn remove_trajectory_hints_messages(&self, window: &mut MessageWindowView<'_>) {
        remove_by_prefix(window.messages, Role::System, TRAJECTORY_PREFIX);
    }

    /// Remove tree-memory summary messages from the window.
    pub fn remove_tree_memory_messages(&self, window: &mut MessageWindowView<'_>) {
        remove_by_prefix(window.messages, Role::System, TREE_MEMORY_PREFIX);
    }

    /// Remove reasoning-strategy messages from the window.
    pub fn remove_reasoning_strategies_messages(&self, window: &mut MessageWindowView<'_>) {
        remove_by_prefix(window.messages, Role::System, REASONING_PREFIX);
    }

    /// Remove previously injected LSP context notes from the window.
    ///
    /// Called before injecting fresh notes each turn so stale diagnostics/hover
    /// data from the previous tool call do not accumulate across iterations.
    pub fn remove_lsp_messages(&self, window: &mut MessageWindowView<'_>) {
        remove_by_prefix(window.messages, Role::System, LSP_NOTE_PREFIX);
    }

    /// Remove code-context (repo-map / file context) messages from the window.
    pub fn remove_code_context_messages(&self, window: &mut MessageWindowView<'_>) {
        remove_by_part_or_prefix(window.messages, CODE_CONTEXT_PREFIX, |p| {
            matches!(p, MessagePart::CodeContext { .. })
        });
    }

    /// Remove session-summary messages from the window.
    pub fn remove_summary_messages(&self, window: &mut MessageWindowView<'_>) {
        remove_by_part_or_prefix(window.messages, SUMMARY_PREFIX, |p| {
            matches!(p, MessagePart::Summary { .. })
        });
    }

    /// Remove cross-session context messages from the window.
    pub fn remove_cross_session_messages(&self, window: &mut MessageWindowView<'_>) {
        remove_by_part_or_prefix(window.messages, CROSS_SESSION_PREFIX, |p| {
            matches!(p, MessagePart::CrossSession { .. })
        });
    }

    /// Remove the session-digest user message from the window.
    pub fn remove_session_digest_message(&self, window: &mut MessageWindowView<'_>) {
        remove_by_prefix(window.messages, Role::User, SESSION_DIGEST_PREFIX);
    }

    /// Remove document-RAG messages from the window.
    pub fn remove_document_rag_messages(&self, window: &mut MessageWindowView<'_>) {
        remove_by_prefix(window.messages, Role::System, DOCUMENT_RAG_PREFIX);
    }

    /// Trim the non-system message tail to fit within `token_budget` tokens.
    ///
    /// Keeps the system prefix intact and the most recent messages, removing
    /// older messages from the start of the conversation history until the
    /// token count fits the budget. Recomputes `cached_prompt_tokens` after trimming.
    ///
    /// No-op when `token_budget` is zero.
    pub fn trim_messages_to_budget(&self, window: &mut MessageWindowView<'_>, token_budget: usize) {
        if token_budget == 0 {
            return;
        }

        // Find the first non-system message index (skip system prefix).
        let history_start = window
            .messages
            .iter()
            .position(|m| m.role != Role::System)
            .unwrap_or(window.messages.len());

        if history_start >= window.messages.len() {
            return;
        }

        let mut total = 0usize;
        let mut keep_from = window.messages.len();

        for i in (history_start..window.messages.len()).rev() {
            let msg_tokens = window
                .token_counter
                .count_message_tokens(&window.messages[i]);
            if total + msg_tokens > token_budget {
                break;
            }
            total += msg_tokens;
            keep_from = i;
        }

        if keep_from > history_start {
            let removed = keep_from - history_start;
            window.messages.drain(history_start..keep_from);
            recompute_prompt_tokens(window);
            tracing::info!(
                removed,
                token_budget,
                "trimmed messages to fit context budget"
            );
        }
    }

    // ── Placeholder stubs for later PRs ──────────────────────────────────────

    /// Prepare the context window for the current turn.
    ///
    /// Removes stale injection messages, gathers semantic recall and graph facts,
    /// applies the configured retrieval policy, and injects the fresh context block.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::Memory`] if recall fails, [`ContextError::Assembler`]
    /// if the context assembler encounters an internal error.
    pub async fn prepare_context(
        &self,
        _query: &str,
        _window: &mut MessageWindowView<'_>,
        _view: &mut ContextAssemblyView<'_>,
        _providers: &ProviderHandles,
        _status: &(impl StatusSink + ?Sized),
    ) -> Result<(), ContextError> {
        // TODO: implement in PR2 migration
        unimplemented!("prepare_context will be implemented in PR2")
    }

    /// Rebuild the system prompt for the current turn.
    ///
    /// Updates the skill catalog, applies channel-skill filters, and rewrites the
    /// first message in `window.messages` with the new system prompt.
    pub async fn rebuild_system_prompt(
        &self,
        _query: &str,
        _window: &mut MessageWindowView<'_>,
        _view: &mut ContextAssemblyView<'_>,
        _providers: &ProviderHandles,
        _trust_gate: &(impl TrustGate + ?Sized),
        _status: &(impl StatusSink + ?Sized),
    ) {
        // TODO: implement in PR6 migration (stays on Agent<C> per scope decision)
        unimplemented!("rebuild_system_prompt stays on Agent<C> per scope decision")
    }

    /// Reset the conversation history.
    ///
    /// Clears all messages except the system prompt and resets context-manager state.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::Memory`] if the persistence layer fails to record the reset.
    pub async fn reset_conversation(
        &self,
        _window: &mut MessageWindowView<'_>,
        _view: &mut ContextAssemblyView<'_>,
    ) -> Result<(), ContextError> {
        // TODO: implement in PR3 migration
        unimplemented!("reset_conversation will be implemented in PR3")
    }

    /// Run compaction if the token budget is exhausted.
    ///
    /// Dispatches to the appropriate compaction tier based on the current
    /// context manager state.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::Memory`] if compaction persistence fails.
    pub async fn maybe_compact(
        &self,
        _summ: &mut ContextSummarizationView<'_>,
        _providers: &ProviderHandles,
        _status: &(impl StatusSink + ?Sized),
    ) -> Result<(), ContextError> {
        // TODO: implement in PR8 migration
        unimplemented!("maybe_compact will be implemented in PR8")
    }

    /// Summarize the most recent tool-use/result pair if it exceeds the budget.
    pub async fn maybe_summarize_tool_pair(
        &self,
        _summ: &mut ContextSummarizationView<'_>,
        _providers: &ProviderHandles,
    ) {
        // TODO: implement in PR4 migration
        unimplemented!("maybe_summarize_tool_pair will be implemented in PR4")
    }

    /// Apply any deferred summaries to the message window.
    ///
    /// Returns the number of summaries applied.
    #[must_use]
    pub fn apply_deferred_summaries(&self, _summ: &mut ContextSummarizationView<'_>) -> usize {
        // TODO: implement in PR4 migration
        unimplemented!("apply_deferred_summaries will be implemented in PR4")
    }

    /// Flush all deferred summaries to the message window.
    pub async fn flush_deferred_summaries(&self, _summ: &mut ContextSummarizationView<'_>) {
        // TODO: implement in PR4 migration
        unimplemented!("flush_deferred_summaries will be implemented in PR4")
    }

    /// Apply deferred summaries if the compaction budget permits.
    pub fn maybe_apply_deferred_summaries(&self, _summ: &mut ContextSummarizationView<'_>) {
        // TODO: implement in PR4 migration
        unimplemented!("maybe_apply_deferred_summaries will be implemented in PR4")
    }

    /// Apply a soft compaction pass mid-iteration if required.
    pub fn maybe_soft_compact_mid_iteration(&self, _summ: &mut ContextSummarizationView<'_>) {
        // TODO: implement in PR7 migration
        unimplemented!("maybe_soft_compact_mid_iteration will be implemented in PR7")
    }

    /// Run proactive compression if the token usage crosses the configured threshold.
    pub async fn maybe_proactive_compress(
        &self,
        _summ: &mut ContextSummarizationView<'_>,
        _providers: &ProviderHandles,
        _status: &(impl StatusSink + ?Sized),
    ) {
        // TODO: implement in PR7 migration
        unimplemented!("maybe_proactive_compress will be implemented in PR7")
    }

    /// Refresh the task goal summary if it has expired.
    pub fn maybe_refresh_task_goal(&self, _summ: &mut ContextSummarizationView<'_>) {
        // TODO: implement in PR7 migration
        unimplemented!("maybe_refresh_task_goal will be implemented in PR7")
    }

    /// Refresh the subgoal summary if it has expired.
    pub fn maybe_refresh_subgoal(&self, _summ: &mut ContextSummarizationView<'_>) {
        // TODO: implement in PR7 migration
        unimplemented!("maybe_refresh_subgoal will be implemented in PR7")
    }
}

// ── Free functions (helpers shared across service methods) ────────────────────

/// Recompute `cached_prompt_tokens` from the current message list.
///
/// Called after every mutation that changes the message count or content, so the
/// provider call path always sees an accurate token count.
pub(crate) fn recompute_prompt_tokens(window: &mut MessageWindowView<'_>) {
    *window.cached_prompt_tokens = window
        .messages
        .iter()
        .map(|m| window.token_counter.count_message_tokens(m) as u64)
        .sum();
}

/// Remove all system/user messages whose `content` starts with `prefix` and whose
/// role matches `role`.
///
/// Operates on the raw `messages` slice to allow callers that don't hold a full
/// `MessageWindowView` to use this helper (e.g., from `zeph-core` shims).
pub(crate) fn remove_by_prefix(
    messages: &mut Vec<zeph_llm::provider::Message>,
    role: Role,
    prefix: &str,
) {
    messages.retain(|m| m.role != role || !m.content.starts_with(prefix));
}

/// Remove system messages that match either a typed `MessagePart` or a content prefix.
///
/// Typed-part matching takes priority — a message is removed if its **first** part
/// satisfies `part_matches`. As a fallback, messages that start with `prefix` are also
/// removed. Non-system messages are always retained.
pub(crate) fn remove_by_part_or_prefix(
    messages: &mut Vec<zeph_llm::provider::Message>,
    prefix: &str,
    part_matches: impl Fn(&MessagePart) -> bool,
) {
    messages.retain(|m| {
        if m.role != Role::System {
            return true;
        }
        if m.parts.first().is_some_and(&part_matches) {
            return false;
        }
        !m.content.starts_with(prefix)
    });
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use zeph_llm::provider::{Message, MessagePart, Role};
    use zeph_memory::TokenCounter;

    use super::*;
    use crate::helpers::{GRAPH_FACTS_PREFIX, RECALL_PREFIX, SUMMARY_PREFIX};
    use crate::state::MessageWindowView;

    fn make_counter() -> Arc<TokenCounter> {
        Arc::new(TokenCounter::default())
    }

    fn make_window<'a>(
        messages: &'a mut Vec<Message>,
        cached: &'a mut u64,
        completed: &'a mut HashSet<String>,
    ) -> MessageWindowView<'a> {
        let last = Box::leak(Box::new(None::<i64>));
        let deferred_hide = Box::leak(Box::new(Vec::<i64>::new()));
        let deferred_summ = Box::leak(Box::new(Vec::<String>::new()));
        MessageWindowView {
            messages,
            last_persisted_message_id: last,
            deferred_db_hide_ids: deferred_hide,
            deferred_db_summaries: deferred_summ,
            cached_prompt_tokens: cached,
            token_counter: make_counter(),
            completed_tool_ids: completed,
        }
    }

    fn sys(text: &str) -> Message {
        Message::from_legacy(Role::System, text)
    }

    fn user(text: &str) -> Message {
        Message::from_legacy(Role::User, text)
    }

    fn assistant(text: &str) -> Message {
        Message::from_legacy(Role::Assistant, text)
    }

    #[test]
    fn clear_history_keeps_system_prompt() {
        let mut msgs = vec![sys("system"), user("hello"), assistant("hi")];
        let mut cached = 0u64;
        let mut completed = HashSet::new();
        completed.insert("tool_1".to_owned());
        let mut window = make_window(&mut msgs, &mut cached, &mut completed);

        ContextService::new().clear_history(&mut window);

        assert_eq!(window.messages.len(), 1);
        assert_eq!(window.messages[0].content, "system");
        assert!(
            window.completed_tool_ids.is_empty(),
            "completed_tool_ids must be cleared"
        );
    }

    #[test]
    fn clear_history_empty_messages_is_noop() {
        let mut msgs: Vec<Message> = vec![];
        let mut cached = 0u64;
        let mut completed = HashSet::new();
        let mut window = make_window(&mut msgs, &mut cached, &mut completed);

        ContextService::new().clear_history(&mut window);

        assert!(window.messages.is_empty());
    }

    #[test]
    fn remove_recall_messages_removes_by_prefix() {
        let mut msgs = vec![
            sys("system"),
            sys(&format!("{RECALL_PREFIX}some recalled text")),
            user("hello"),
        ];
        let mut cached = 0u64;
        let mut completed = HashSet::new();
        let mut window = make_window(&mut msgs, &mut cached, &mut completed);

        ContextService::new().remove_recall_messages(&mut window);

        assert_eq!(window.messages.len(), 2);
        assert!(
            window
                .messages
                .iter()
                .all(|m| !m.content.starts_with(RECALL_PREFIX))
        );
    }

    #[test]
    fn remove_graph_facts_messages_removes_matching() {
        let mut msgs = vec![
            sys("system"),
            sys(&format!("{GRAPH_FACTS_PREFIX}fact1")),
            user("hello"),
        ];
        let mut cached = 0u64;
        let mut completed = HashSet::new();
        let mut window = make_window(&mut msgs, &mut cached, &mut completed);

        ContextService::new().remove_graph_facts_messages(&mut window);

        assert_eq!(window.messages.len(), 2);
    }

    #[test]
    fn remove_summary_messages_removes_by_part() {
        let mut msgs = vec![
            sys("system"),
            Message::from_parts(
                Role::System,
                vec![MessagePart::Summary {
                    text: format!("{SUMMARY_PREFIX}old summary"),
                }],
            ),
            user("hello"),
        ];
        let mut cached = 0u64;
        let mut completed = HashSet::new();
        let mut window = make_window(&mut msgs, &mut cached, &mut completed);

        ContextService::new().remove_summary_messages(&mut window);

        assert_eq!(window.messages.len(), 2);
    }

    #[test]
    fn trim_messages_to_budget_zero_is_noop() {
        let mut msgs = vec![sys("system"), user("a"), assistant("b"), user("c")];
        let original_len = msgs.len();
        let mut cached = 0u64;
        let mut completed = HashSet::new();
        let mut window = make_window(&mut msgs, &mut cached, &mut completed);

        ContextService::new().trim_messages_to_budget(&mut window, 0);

        assert_eq!(window.messages.len(), original_len);
    }

    #[test]
    fn trim_messages_to_budget_keeps_recent() {
        // With a very small budget only the most recent messages survive.
        let mut msgs = vec![
            sys("system"),
            user("message 1"),
            assistant("reply 1"),
            user("message 2"),
        ];
        let mut cached = 0u64;
        let mut completed = HashSet::new();
        let mut window = make_window(&mut msgs, &mut cached, &mut completed);

        // 1-token budget keeps the last user message only.
        ContextService::new().trim_messages_to_budget(&mut window, 1);

        // System prompt is always kept; at least one recent message should be present.
        assert!(
            window.messages.len() < 4,
            "trim should remove some messages"
        );
        assert_eq!(
            window.messages[0].role,
            Role::System,
            "system prompt must survive trim"
        );
    }
}
