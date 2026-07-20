// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Implementations of `zeph-commands` sub-traits on `zeph-core` state types.
//!
//! Each `impl` wires a sub-trait from `zeph-commands::traits` to the corresponding
//! `Agent` subsystem. `build_command_context` assembles a [`CommandContext`] from
//! `&mut Agent<C>` for use at dispatch time.
//!
//! [`CommandContext`]: zeph_commands::CommandContext

use std::future::Future;
use std::pin::Pin;

use zeph_commands::CommandError;
use zeph_commands::traits::debug::DebugAccess;
use zeph_commands::traits::messages::MessageAccess;
use zeph_commands::traits::session::SessionAccess;
use zeph_commands::transcript::{TranscriptEntry, TranscriptRole};
use zeph_llm::provider::{Message, MessagePart, Role};

use super::log_commands;
use super::state::{DebugState, MetricsState, ProviderState, SecurityState, ToolState};
use super::tool_orchestrator::ToolOrchestrator;

// --- DebugAccess ---

impl DebugAccess for DebugState {
    fn log_status(&self) -> String {
        let mut out = String::new();
        log_commands::format_logging_status(&self.logging_config, &mut out);
        out
    }

    fn read_log_tail<'a>(
        &'a self,
        n: usize,
    ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>> {
        let file = self.logging_config.file.clone();
        Box::pin(async move {
            if file.is_empty() {
                return None;
            }
            let base = std::path::PathBuf::from(&file);
            // NOTE: raw spawn_blocking is intentional — this is a one-shot debug command
            // invoked by the user (not an agent hot-path task). The task_supervisor semaphore
            // guards CPU-bound agent work; gating a rare log-tail read there would add overhead
            // with no meaningful benefit.
            tokio::task::spawn_blocking(move || {
                let actual = log_commands::resolve_current_log_file(&base);
                actual.and_then(|p| log_commands::read_log_tail(&p, n))
            })
            .await
            .unwrap_or(None)
        })
    }

    fn scrub(&self, text: &str) -> String {
        crate::redact::scrub_content(text).into_owned()
    }

    fn dump_status(&self) -> Option<String> {
        self.debug_dumper
            .as_ref()
            .map(|d| d.dir().display().to_string())
    }

    fn dump_format_name(&self) -> String {
        format!("{:?}", self.dump_format).to_lowercase()
    }

    fn enable_dump(&mut self, dir: &str) -> Result<String, CommandError> {
        let path = std::path::PathBuf::from(dir);
        match crate::debug_dump::DebugDumper::new(&path, self.dump_format) {
            Ok(dumper) => {
                let display = dumper.dir().display().to_string();
                self.debug_dumper = Some(dumper);
                Ok(display)
            }
            Err(e) => Err(CommandError::new(e)),
        }
    }

    fn set_dump_format(&mut self, format_name: &str) -> Result<(), CommandError> {
        let fmt = match format_name {
            "json" => crate::debug_dump::DumpFormat::Json,
            "raw" => crate::debug_dump::DumpFormat::Raw,
            "trace" => crate::debug_dump::DumpFormat::Trace,
            other => {
                return Err(CommandError::new(format!(
                    "Unknown format '{other}'. Valid values: json, raw, trace."
                )));
            }
        };
        self.switch_format(fmt);
        Ok(())
    }
}

// --- MessageAccess ---
//
// The `MessageAccess` trait groups operations that span multiple state structs
// (`MessageState`, `ToolState`, `ProviderState`, `MetricsState`, `SecurityState`,
// `ToolOrchestrator`). A thin wrapper struct holds mutable references to all of them.

/// Wrapper that implements [`MessageAccess`] by holding mutable references to all state
/// structs touched by the clear/queue operations.
///
/// Note: the channel is NOT included here to avoid double-borrow conflicts with
/// `CommandContext::sink`. The `/clear-queue` handler calls `ctx.sink.send_queue_count(0)`
/// directly after `drain_queue()`.
pub(super) struct MessageAccessImpl<'a> {
    pub msg: &'a mut super::state::MessageState,
    pub tool_state: &'a mut ToolState,
    pub providers: &'a mut ProviderState,
    pub metrics: &'a MetricsState,
    pub security: &'a mut SecurityState,
    pub tool_orchestrator: &'a mut ToolOrchestrator,
}

impl MessageAccess for MessageAccessImpl<'_> {
    fn clear_history(&mut self) {
        // Keep only the first message (system prompt), matching Agent::clear_history().
        let system_prompt = self.msg.messages.first().cloned();
        self.msg.messages.clear();
        if let Some(sp) = system_prompt {
            self.msg.messages.push(sp);
        }
        // Clear tool dependency state (reset between conversations).
        self.tool_state.completed_tool_ids.clear();
        // Recompute cached prompt token count after truncation.
        self.providers.cached_prompt_tokens = self
            .msg
            .messages
            .iter()
            .map(|m| self.metrics.token_counter.count_message_tokens(m) as u64)
            .sum();
        // Clear runtime per-turn caches.
        self.msg.pending_image_parts.clear();
        self.tool_orchestrator.clear_cache();
        self.security.user_provided_urls.write().clear();
        // Issue #6490 (MemGhost): reset the turn-scoped memory-consent trust tracker on
        // /clear, matching begin_turn's per-turn reset — see agent/mod.rs.
        *self.security.memory_consent_trust.write() = 0;
        self.msg.recompute_non_system_count();
    }

    fn queue_len(&self) -> usize {
        self.msg.message_queue.len()
    }

    fn drain_queue(&mut self) -> usize {
        let n = self.msg.message_queue.len();
        self.msg.message_queue.clear();
        n
    }

    fn notify_queue_count<'a>(
        &'a mut self,
        _count: usize,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        // No-op: the channel borrow is held by CommandContext::sink.
        // The /clear-queue handler calls ctx.sink.send_queue_count(0) directly.
        Box::pin(async {})
    }

    fn transcript_len(&self) -> usize {
        self.msg.non_system_len()
    }

    fn transcript_page(&self, start: usize, count: usize) -> Vec<TranscriptEntry> {
        let total = self.msg.non_system_len();
        if count == 0 || start >= total {
            return Vec::new();
        }
        let end = start.saturating_add(count).min(total);
        // Scan from whichever end of `messages` is ordinally closer to the requested page —
        // the bounded-default `/history N` case always asks for the tail (back_cost == 0) and
        // `/history all`'s first page always asks for the head (front_cost == 0), so both of
        // the common call patterns become O(count) instead of walking the full vector on every
        // call (#6427). Middle-of-history pages from repeated `/history next` still cost up to
        // O(min(start, total - end)), but that heavy full-history walk is explicitly the rare,
        // user-warned-about path ("this may take a moment"), not a hot one.
        let front_cost = start;
        let back_cost = total - end;
        if front_cost <= back_cost {
            self.msg
                .messages
                .iter()
                .filter(|m| m.role != Role::System)
                .skip(start)
                .take(end - start)
                .map(message_to_transcript_entry)
                .collect()
        } else {
            let mut page: Vec<TranscriptEntry> = self
                .msg
                .messages
                .iter()
                .rev()
                .filter(|m| m.role != Role::System)
                .skip(back_cost)
                .take(end - start)
                .map(message_to_transcript_entry)
                .collect();
            page.reverse();
            page
        }
    }

    fn history_cursor(&self) -> usize {
        self.msg.history_cursor
    }

    fn set_history_cursor(&mut self, pos: usize) {
        self.msg.history_cursor = pos;
    }
}

/// Convert one non-system [`Message`] into a display [`TranscriptEntry`] for `/history`
/// (spec-068 §13.6).
///
/// Tool-bearing messages (a `User` message carrying a `ToolResult` part, or an `Assistant`
/// message that is tool-use-only with no visible text) are collapsed to a single
/// [`TranscriptRole::Tool`] line, mirroring the TUI's existing tool-output collapsing
/// convention (`is_tool_use_only`, `crates/zeph-tui/src/app/state.rs`).
fn message_to_transcript_entry(m: &Message) -> TranscriptEntry {
    // `Role` is `#[non_exhaustive]`; System is filtered out by the caller before this
    // function ever sees a message, so `Assistant` is the only remaining non-`User` case.
    if m.role == Role::User {
        if let Some(content) = m.parts.iter().find_map(|p| match p {
            MessagePart::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        }) {
            return TranscriptEntry {
                role: TranscriptRole::Tool,
                content,
                tool_name: None,
            };
        }
        return TranscriptEntry {
            role: TranscriptRole::User,
            content: m.content.clone(),
            tool_name: None,
        };
    }

    if !m.content.trim().is_empty() {
        return TranscriptEntry {
            role: TranscriptRole::Assistant,
            content: m.content.clone(),
            tool_name: None,
        };
    }
    if let Some((name, input)) = m.parts.iter().find_map(|p| match p {
        MessagePart::ToolUse { name, input, .. } => Some((name.clone(), input.clone())),
        _ => None,
    }) {
        return TranscriptEntry {
            role: TranscriptRole::Tool,
            content: format!("called with {input}"),
            tool_name: Some(name),
        };
    }
    TranscriptEntry {
        role: TranscriptRole::Assistant,
        content: String::new(),
        tool_name: None,
    }
}

// --- SessionAccess ---
//
// `SessionAccess` is shared (non-mut), so it's implemented on a wrapper holding only
// the channel reference (from which `supports_exit` is read).

/// Concrete implementation of [`SessionAccess`] holding the pre-read `supports_exit` flag.
///
/// Reading the flag before constructing `CommandContext` avoids the need for a `&C` reference
/// in the context, which would conflict with `ctx.sink` holding `&mut C`.
pub(super) struct SessionAccessImpl {
    pub supports_exit: bool,
    pub history_expand_default_lines: usize,
}

impl SessionAccess for SessionAccessImpl {
    fn supports_exit(&self) -> bool {
        self.supports_exit
    }

    fn history_expand_default_lines(&self) -> usize {
        self.history_expand_default_lines
    }
}

// --- Null impls for agent-command dispatch block ---
//
// When dispatching agent-access commands (graph, memory, model, etc.) the `Agent<C>` itself
// occupies `ctx.agent`. The debug/messages/session/sink fields are filled with no-op sentinels
// because those handlers do not call those sub-traits.

/// No-op [`DebugAccess`] for the agent-command dispatch block.
pub(super) struct NullDebugAccess;

impl zeph_commands::traits::debug::DebugAccess for NullDebugAccess {
    fn log_status(&self) -> String {
        String::new()
    }

    fn read_log_tail<'a>(
        &'a self,
        _n: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send + 'a>> {
        Box::pin(async { None })
    }

    fn scrub(&self, text: &str) -> String {
        text.to_owned()
    }

    fn dump_status(&self) -> Option<String> {
        None
    }

    fn dump_format_name(&self) -> String {
        String::new()
    }

    fn enable_dump(&mut self, _dir: &str) -> Result<String, CommandError> {
        Ok(String::new())
    }

    fn set_dump_format(&mut self, _format_name: &str) -> Result<(), CommandError> {
        Ok(())
    }
}

/// No-op [`MessageAccess`] for the agent-command dispatch block.
pub(super) struct NullMessageAccess;

impl zeph_commands::traits::messages::MessageAccess for NullMessageAccess {
    fn clear_history(&mut self) {}

    fn queue_len(&self) -> usize {
        0
    }

    fn drain_queue(&mut self) -> usize {
        0
    }

    fn notify_queue_count<'a>(
        &'a mut self,
        _count: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }

    fn transcript_len(&self) -> usize {
        0
    }

    fn transcript_page(&self, _start: usize, _count: usize) -> Vec<TranscriptEntry> {
        Vec::new()
    }

    fn history_cursor(&self) -> usize {
        0
    }

    fn set_history_cursor(&mut self, _pos: usize) {}
}

/// No-op [`SessionAccess`] for the agent-command dispatch block.
pub(super) struct NullSessionAccess;

impl SessionAccess for NullSessionAccess {
    fn supports_exit(&self) -> bool {
        false
    }

    fn history_expand_default_lines(&self) -> usize {
        20
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::agent::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use zeph_llm::provider::MessageMetadata;

    fn make_agent() -> Agent<MockChannel> {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        Agent::new(provider, channel, registry, None, 5, executor)
    }

    fn msg(role: Role, content: &str) -> Message {
        Message {
            role,
            content: content.to_string(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }
    }

    fn access(agent: &mut Agent<MockChannel>) -> MessageAccessImpl<'_> {
        MessageAccessImpl {
            msg: &mut agent.msg,
            tool_state: &mut agent.services.tool_state,
            providers: &mut agent.runtime.providers,
            metrics: &agent.runtime.metrics,
            security: &mut agent.services.security,
            tool_orchestrator: &mut agent.tool_orchestrator,
        }
    }

    /// `TranscriptEntry` has no `PartialEq` (see `zeph-commands/src/transcript.rs`) — project
    /// into a comparable tuple for `assert_eq!` in these tests.
    fn comparable(entries: &[TranscriptEntry]) -> Vec<(TranscriptRole, String, Option<String>)> {
        entries
            .iter()
            .map(|e| (e.role, e.content.clone(), e.tool_name.clone()))
            .collect()
    }

    /// Reference implementation matching the pre-#6427 behavior exactly (the same filter,
    /// skip, take chain), used to prove the optimized `transcript_page` produces identical
    /// output for every case, including interleaved `Role::System` messages.
    fn reference_page(messages: &[Message], start: usize, count: usize) -> Vec<TranscriptEntry> {
        messages
            .iter()
            .filter(|m| m.role != Role::System)
            .skip(start)
            .take(count)
            .map(message_to_transcript_entry)
            .collect()
    }

    #[test]
    fn transcript_len_empty_history_is_zero() {
        let mut agent = make_agent();
        let ctx = access(&mut agent);
        // Fresh agent has only the system-prompt message.
        assert_eq!(ctx.transcript_len(), 0);
    }

    #[test]
    fn transcript_page_empty_history_returns_empty() {
        let mut agent = make_agent();
        let ctx = access(&mut agent);
        assert!(ctx.transcript_page(0, 20).is_empty());
    }

    #[test]
    fn transcript_page_count_zero_returns_empty() {
        let mut agent = make_agent();
        agent.push_message(msg(Role::User, "hi"));
        let ctx = access(&mut agent);
        assert!(ctx.transcript_page(0, 0).is_empty());
    }

    #[test]
    fn transcript_page_start_beyond_len_returns_empty() {
        let mut agent = make_agent();
        agent.push_message(msg(Role::User, "hi"));
        let ctx = access(&mut agent);
        assert!(ctx.transcript_page(5, 3).is_empty());
        assert!(ctx.transcript_page(1, 3).is_empty()); // start == total is also out of range
    }

    #[test]
    fn transcript_len_and_page_ignore_system_only_history() {
        let mut agent = make_agent();
        agent.push_message(msg(Role::System, "extra system note"));
        agent.push_message(msg(Role::System, "another note"));
        let ctx = access(&mut agent);
        assert_eq!(ctx.transcript_len(), 0);
        assert!(ctx.transcript_page(0, 10).is_empty());
    }

    #[test]
    fn transcript_len_counts_only_non_system_and_tracks_incrementally() {
        let mut agent = make_agent();
        agent.push_message(msg(Role::User, "u1"));
        agent.push_message(msg(Role::System, "lsp note"));
        agent.push_message(msg(Role::Assistant, "a1"));
        agent.push_message(msg(Role::User, "u2"));
        let ctx = access(&mut agent);
        assert_eq!(ctx.transcript_len(), 3);
    }

    #[test]
    fn transcript_page_bounded_default_tail_matches_reference() {
        // Mirrors the `/history N` hot path: start = total - n (near the tail), exercising
        // the back-scan branch.
        let mut agent = make_agent();
        for i in 0..30 {
            agent.push_message(msg(Role::User, &format!("u{i}")));
            agent.push_message(msg(Role::Assistant, &format!("a{i}")));
        }
        let messages = agent.msg.messages.clone();
        let ctx = access(&mut agent);
        let total = ctx.transcript_len();
        let n = 20.min(total);
        let start = total - n;
        let expected = reference_page(&messages, start, n);
        let actual = ctx.transcript_page(start, n);
        assert_eq!(comparable(&actual), comparable(&expected));
        assert_eq!(actual.len(), n);
    }

    #[test]
    fn transcript_page_front_scan_then_next_continuation_matches_reference() {
        // Mirrors `/history all` -> repeated `/history next`: start = 0, then start = cursor,
        // cursor + count, ... — exercising the front-scan branch across a page boundary.
        let mut agent = make_agent();
        for i in 0..45 {
            agent.push_message(msg(Role::User, &format!("u{i}")));
        }
        let messages = agent.msg.messages.clone();
        let ctx = access(&mut agent);
        let total = ctx.transcript_len();
        let page_size = 20;

        let mut cursor = 0;
        while cursor < total {
            let count = page_size.min(total - cursor);
            let expected = reference_page(&messages, cursor, count);
            let actual = ctx.transcript_page(cursor, count);
            assert_eq!(
                comparable(&actual),
                comparable(&expected),
                "mismatch at cursor={cursor}"
            );
            cursor += count;
        }
    }

    #[test]
    fn transcript_page_matches_reference_with_interleaved_system_messages() {
        // Simulates focus checkpoints / LSP notes / knowledge blocks scattered throughout
        // history (#6427) — the non-system counter must still yield byte-identical output to
        // the naive filter-skip-take reference for every (start, count) pair, regardless of
        // which end `transcript_page` chooses to scan from.
        let mut agent = make_agent();
        for i in 0..40 {
            agent.push_message(msg(Role::User, &format!("u{i}")));
            if i % 3 == 0 {
                agent.push_message(msg(Role::System, &format!("note{i}")));
            }
            agent.push_message(msg(Role::Assistant, &format!("a{i}")));
        }
        let messages = agent.msg.messages.clone();
        let ctx = access(&mut agent);
        let total = ctx.transcript_len();
        assert_eq!(
            total,
            messages.iter().filter(|m| m.role != Role::System).count()
        );

        for start in [0, 1, total / 2, total.saturating_sub(1), total] {
            for count in [0, 1, 5, total] {
                let expected = reference_page(&messages, start, count);
                let actual = ctx.transcript_page(start, count);
                assert_eq!(
                    comparable(&actual),
                    comparable(&expected),
                    "mismatch at start={start}, count={count}"
                );
            }
        }
    }

    #[test]
    fn clear_history_resets_transcript_len_to_zero() {
        let mut agent = make_agent();
        agent.push_message(msg(Role::User, "u1"));
        agent.push_message(msg(Role::Assistant, "a1"));
        let mut ctx = access(&mut agent);
        assert_eq!(ctx.transcript_len(), 2);
        ctx.clear_history();
        assert_eq!(ctx.transcript_len(), 0);
    }

    /// Regression for critic finding C1 (#6490): `/clear` must reset the turn-scoped
    /// memory-consent trust tracker, matching the doc comment's stated intent
    /// (`sanitize.rs`/`memory_tools.rs` both claim "reset at turn boundaries, /clear").
    #[test]
    fn clear_history_resets_memory_consent_trust_to_zero() {
        let mut agent = make_agent();
        *agent.services.security.memory_consent_trust.write() = 2; // ExternalUntrusted
        let mut ctx = access(&mut agent);
        ctx.clear_history();
        assert_eq!(*agent.services.security.memory_consent_trust.read(), 0);
    }

    /// Regression for the direct-append + recompute pattern used by `builder.rs:237`
    /// (durable-log replay seeding) — the batch-mutation counterpart to the incremental
    /// `push_message` path already covered above.
    #[test]
    fn with_preloaded_messages_recomputes_non_system_count() {
        let agent = make_agent();
        let preloaded = vec![
            msg(Role::User, "u1"),
            msg(Role::System, "focus checkpoint"),
            msg(Role::Assistant, "a1"),
            msg(Role::User, "u2"),
        ];
        let mut agent = agent.with_preloaded_messages(preloaded);
        let expected = agent
            .msg
            .messages
            .iter()
            .filter(|m| m.role != Role::System)
            .count();
        let ctx = access(&mut agent);
        assert_eq!(ctx.transcript_len(), expected);
        assert_eq!(ctx.transcript_len(), 3);
    }

    /// Regression for the "mutation happens inside `zeph-agent-context` through a borrowed
    /// `message_window_view()`, recompute after the call returns" pattern — the pattern used
    /// by 9 of the 11 batch-mutation sites (`assembly.rs`'s `clear_history`/
    /// `remove_lsp_messages`/`remove_code_context_messages`, `persistence/history.rs`,
    /// `summarization/*.rs`). Distinct from `MessageAccessImpl::clear_history` (tested above),
    /// which mutates `messages` directly and never goes through `ContextService`.
    #[test]
    fn agent_clear_history_recomputes_non_system_count() {
        let mut agent = make_agent();
        agent.push_message(msg(Role::User, "u1"));
        agent.push_message(msg(Role::Assistant, "a1"));
        assert_eq!(access(&mut agent).transcript_len(), 2);
        agent.clear_history();
        assert_eq!(access(&mut agent).transcript_len(), 0);
    }
}
