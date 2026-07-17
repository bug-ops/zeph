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
        self.msg
            .messages
            .iter()
            .filter(|m| m.role != Role::System)
            .count()
    }

    fn transcript_page(&self, start: usize, count: usize) -> Vec<TranscriptEntry> {
        self.msg
            .messages
            .iter()
            .filter(|m| m.role != Role::System)
            .skip(start)
            .take(count)
            .map(message_to_transcript_entry)
            .collect()
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
