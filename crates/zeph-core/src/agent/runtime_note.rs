// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-iteration runtime notes appended to the unsent tail tool-result message.
//!
//! Volatile per-iteration state (remaining tool budget, LSP diagnostics, utility-gate hints) used
//! to be written into the system prompt or pushed as `Role::System` messages in front of the
//! cached history, which busted the provider prompt cache on every iteration (#6786, #6817).
//! A [`MessagePart::RuntimeNote`] is appended to the tail `User` message that carries the tool
//! results instead, so the already-sent prefix stays byte-identical.
//!
//! Contract (`specs/001-system-invariants` section 4): at most one note per message, only on the
//! agent-visible tail `User` message with `ToolResult` parts (loop iteration 1 or later, or the
//! loop exit for hints that would otherwise be lost), never persisted, never part of
//! `Message::content`. The rendered tag carries a per-session nonce that the system prompt names
//! as the only genuine id; tool output is not escaped for this tag (file reads stay byte-exact),
//! so a forged note is recognisable by its id, not blocked.
//!
//! The tool budget in the note is advisory and conservative: it counts every LLM dispatch of the
//! turn at any reflection depth, while each nested loop still enforces its own `max_iterations`.

use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::hash::{Hash, Hasher};

use zeph_agent_context::helpers::BudgetHint;
use zeph_common::delimiter::{DelimiterTag, escape_delimiter_tags};
use zeph_common::sanitize::strip_control_chars_preserve_whitespace;
use zeph_llm::provider::{MessagePart, NoteAppend, Role};

use crate::agent::Agent;
use crate::channel::Channel;

/// Per-session random tag attribute that authenticates a rendered `<runtime-note>`.
///
/// Generated once per session so the rendered prefix stays stable across requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RuntimeNoteNonce(uuid::Uuid);

impl RuntimeNoteNonce {
    fn generate() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl fmt::Display for RuntimeNoteNonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.simple())
    }
}

/// One block of agent-authored state rendered inside a runtime note.
pub(crate) enum RuntimeNoteSection {
    /// Remaining tool-call budget (and cost budget) snapshot.
    ToolBudget(BudgetHint),
    /// Drained LSP hover/diagnostics notes.
    Lsp(String),
    /// Utility-gate retrieve/verify hint.
    UtilityHint(String),
}

impl RuntimeNoteSection {
    fn render(&self) -> Option<String> {
        match self {
            Self::ToolBudget(hint) => hint.format_xml(),
            // LSP hover text carries third-party doc comments: drop control, bidi and
            // zero-width characters (newlines and tabs kept for layout) before escaping.
            Self::Lsp(text) => {
                let clean = strip_control_chars_preserve_whitespace(text);
                Some(escape_delimiter_tags(&clean, &[DelimiterTag::RuntimeNote]).into_owned())
            }
            Self::UtilityHint(text) => {
                Some(escape_delimiter_tags(text, &[DelimiterTag::RuntimeNote]).into_owned())
            }
        }
    }
}

/// Turn-scoped count of LLM dispatches made by the tool loop.
///
/// Incremented once per dispatch at any reflection depth and reset at turn start.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ToolLoopBudget {
    used: usize,
}

impl ToolLoopBudget {
    /// Dispatches already made this turn.
    pub(crate) fn used(self) -> usize {
        self.used
    }

    fn record_dispatch(&mut self) {
        self.used = self.used.saturating_add(1);
    }
}

/// Budget thresholds (remaining dispatches) at which a note is worth emitting.
///
/// `ceil(max / 2)`, `ceil(max / 4)`, and each of 3, 2, 1, deduplicated and kept below `max`.
fn budget_thresholds(max: usize) -> BTreeSet<usize> {
    [max.div_ceil(2), max.div_ceil(4), 3, 2, 1]
        .into_iter()
        .filter(|&t| t > 0 && t < max)
        .collect()
}

/// Session state of the runtime-note seam.
#[derive(Debug)]
pub(crate) struct RuntimeNoteState {
    nonce: RuntimeNoteNonce,
    budget: ToolLoopBudget,
    emitted_thresholds: BTreeSet<usize>,
    seen_lsp: HashSet<u64>,
    pending_utility_hints: Vec<String>,
}

impl Default for RuntimeNoteState {
    fn default() -> Self {
        Self {
            nonce: RuntimeNoteNonce::generate(),
            budget: ToolLoopBudget::default(),
            emitted_thresholds: BTreeSet::new(),
            seen_lsp: HashSet::new(),
            pending_utility_hints: Vec::new(),
        }
    }
}

impl RuntimeNoteState {
    /// Reset every turn-scoped counter; the nonce survives so the rendered prefix stays stable.
    pub(crate) fn begin_turn(&mut self) {
        self.budget = ToolLoopBudget::default();
        self.emitted_thresholds.clear();
        self.seen_lsp.clear();
        self.pending_utility_hints.clear();
    }

    /// The session nonce rendered in every note's `id` attribute.
    pub(crate) fn nonce(&self) -> RuntimeNoteNonce {
        self.nonce
    }

    /// Dispatches counted so far this turn.
    #[cfg(test)]
    pub(crate) fn budget_used(&self) -> usize {
        self.budget.used()
    }

    /// Hints waiting for an eligible tail message.
    #[cfg(test)]
    pub(crate) fn pending_utility_hints(&self) -> &[String] {
        &self.pending_utility_hints
    }

    /// Queue a utility-gate hint for the next eligible tail message.
    pub(crate) fn push_utility_hint(&mut self, hint: String) {
        self.pending_utility_hints.push(hint);
    }

    /// Whether `remaining` crossed a threshold not yet emitted this turn; marks every crossed
    /// threshold as emitted so a jump over several of them (nested reflection) emits once.
    fn take_budget_threshold(&mut self, max: usize, remaining: usize) -> bool {
        let crossed: Vec<usize> = budget_thresholds(max)
            .into_iter()
            .filter(|t| remaining <= *t && !self.emitted_thresholds.contains(t))
            .collect();
        let any = !crossed.is_empty();
        self.emitted_thresholds.extend(crossed);
        any
    }

    /// Whether `text` is new this turn (LSP notes are deduplicated by content hash).
    fn note_lsp_once(&mut self, text: &str) -> bool {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        self.seen_lsp.insert(hasher.finish())
    }

    /// Render the `<runtime-note>` block, or `None` when every section is empty.
    fn render(&self, sections: &[RuntimeNoteSection]) -> Option<String> {
        let body: Vec<String> = sections
            .iter()
            .filter_map(RuntimeNoteSection::render)
            .collect();
        if body.is_empty() {
            return None;
        }
        Some(format!(
            "<runtime-note id=\"{}\">\n{}\n</runtime-note>",
            self.nonce,
            body.join("\n")
        ))
    }
}

impl<C: Channel> Agent<C> {
    /// Start of a user turn: reset the turn-scoped note state and drop LSP notes nobody consumed
    /// (queued after the previous turn's last tool batch), so they never surface as stale
    /// diagnostics in this turn. Called once at the real turn entry, never from a nested
    /// self-reflection pass, so the budget stays turn-scoped at any depth.
    pub(in crate::agent) fn begin_runtime_note_turn(&mut self) {
        self.services.session.runtime_notes.begin_turn();
        if let Some(lsp) = self.services.session.lsp_hooks.as_mut() {
            let tc = std::sync::Arc::clone(&self.runtime.metrics.token_counter);
            let _ = lsp.drain_notes(&tc);
        }
    }

    /// Index of the unsent tail message that may carry a runtime note: the last non-`System`
    /// message, if it is an agent-visible `User` message with `ToolResult` parts and no note yet.
    fn runtime_note_target(&self) -> Option<usize> {
        let idx = self
            .msg
            .messages
            .iter()
            .rposition(|m| m.role != Role::System)?;
        let msg = &self.msg.messages[idx];
        let eligible = msg.role == Role::User
            && msg.metadata.visibility.is_agent_visible()
            && msg
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolResult { .. }))
            && !msg
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::RuntimeNote { .. }));
        eligible.then_some(idx)
    }

    /// Collect the sections due now. Drains the LSP queue and the pending utility hints, so
    /// call it only once a target message exists. `budget_used` is `Some` for a dispatch (the
    /// budget threshold schedule applies) and `None` for a loop-exit flush.
    fn collect_runtime_note_sections(
        &mut self,
        budget_used: Option<usize>,
    ) -> Vec<RuntimeNoteSection> {
        let mut sections = Vec::new();

        let max = self.tool_orchestrator.max_iterations;
        if let Some(used) = budget_used
            && self.runtime.config.budget_hint_enabled
            && max > 0
        {
            let remaining = max.saturating_sub(used);
            if self
                .services
                .session
                .runtime_notes
                .take_budget_threshold(max, remaining)
            {
                sections.push(RuntimeNoteSection::ToolBudget(
                    self.budget_hint_snapshot(used),
                ));
            }
        }

        if let Some(lsp) = self.services.session.lsp_hooks.as_mut() {
            let tc = std::sync::Arc::clone(&self.runtime.metrics.token_counter);
            if let Some(text) = lsp.drain_notes(&tc)
                && self.services.session.runtime_notes.note_lsp_once(&text)
            {
                sections.push(RuntimeNoteSection::Lsp(text));
            }
        }

        sections.extend(
            std::mem::take(&mut self.services.session.runtime_notes.pending_utility_hints)
                .into_iter()
                .map(RuntimeNoteSection::UtilityHint),
        );
        sections
    }

    /// Append one note with the due sections to the tail tool-result message, if any.
    fn append_runtime_note_to_tail(&mut self, budget_used: Option<usize>) {
        let Some(idx) = self.runtime_note_target() else {
            return;
        };
        let sections = self.collect_runtime_note_sections(budget_used);
        let Some(text) = self.services.session.runtime_notes.render(&sections) else {
            return;
        };

        let part = MessagePart::RuntimeNote { text: text.clone() };
        let delta = self.runtime.metrics.token_counter.count_part_tokens(&part) as u64;
        if self.msg.messages[idx].append_runtime_note(text) == NoteAppend::Appended {
            self.runtime.providers.cached_prompt_tokens = self
                .runtime
                .providers
                .cached_prompt_tokens
                .saturating_add(delta);
        }
    }

    /// Runtime-note seam, run once per tool-loop dispatch right before the LLM call.
    ///
    /// Counts the dispatch, and on loop iteration 1 or later appends one note with the due
    /// sections to the tail tool-result message. Never inserts or reorders messages, so the
    /// request prefix stays byte-identical to the previous iteration's.
    #[tracing::instrument(name = "core.agent.runtime_note", skip_all, level = "debug")]
    pub(in crate::agent) fn attach_runtime_note(&mut self, iteration: usize) {
        let used = self.services.session.runtime_notes.budget.used();
        self.services.session.runtime_notes.budget.record_dispatch();
        if iteration == 0 {
            return;
        }
        self.append_runtime_note_to_tail(Some(used));
    }

    /// Loop-exit flush: deliver pending hints and LSP notes (never a budget) onto the still
    /// unsent tail tool-result message when the loop stops without another dispatch, so a hint
    /// such as "tool loop stopped early" reaches the model on the next turn.
    pub(in crate::agent) fn flush_runtime_note(&mut self) {
        self.append_runtime_note_to_tail(None);
    }

    /// Prompt tokens used by the durable LLM-step fingerprint: `cached_prompt_tokens` minus the
    /// tokens of every runtime note in history. Notes are never persisted, so a crash-resumed
    /// run rebuilds the history without them and must still compute the same fingerprint.
    pub(in crate::agent) fn durable_fingerprint_tokens(&self) -> u64 {
        let counter = &self.runtime.metrics.token_counter;
        let note_tokens: u64 = self
            .msg
            .messages
            .iter()
            .flat_map(|m| &m.parts)
            .filter(|p| matches!(p, MessagePart::RuntimeNote { .. }))
            .map(|p| counter.count_part_tokens(p) as u64)
            .sum();
        self.runtime
            .providers
            .cached_prompt_tokens
            .saturating_sub(note_tokens)
    }
}
