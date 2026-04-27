// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`AgentChannel`] trait — minimal async sink the tool dispatcher needs from a channel.
//!
//! This trait is sealed: only types declared in `zeph-core` can implement it. The single
//! implementor is `AgentChannelView<'a, C>` in `crates/zeph-core/src/agent/channel_impl.rs`.
//!
//! # Design rationale
//!
//! `zeph-agent-tools` cannot depend on `zeph-channels` (which depends on `zeph-core`), so it
//! defines its own minimal channel trait rather than reusing `zeph_core::channel::Channel`.
//! This avoids a circular dependency while keeping the dispatcher generic over the channel type.
//!
//! `zeph-core` provides the impl via:
//! ```text
//! impl<C: zeph_core::channel::Channel> AgentChannel for AgentChannelView<'_, C>
//! ```
//! which is orphan-safe because `AgentChannelView` is a local type in `zeph-core`.
//!
//! # TODO(critic): `send_stop_hint` takes a primitive `&str` reason instead of an enum.
//! Any new variant added to `zeph_core::channel::StopHint` MUST be mirrored at dispatcher
//! emit-sites AND at `AgentChannelView::send_stop_hint` match arms.
//! See `critic-3515-3516.md` F3.

use std::future::Future;

use crate::sealed::Sealed;

/// Error returned by every [`AgentChannel`] method.
///
/// Concrete because the trait is sealed and there is exactly one implementor
/// (`AgentChannelView<'_, C>`).
#[derive(Debug, thiserror::Error)]
#[error("agent channel error: {0}")]
pub struct ChannelSinkError(String);

impl ChannelSinkError {
    /// Construct a [`ChannelSinkError`] from any message.
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

/// Borrowed payload for a tool-start event.
///
/// Mirrors the relevant fields of `zeph_core::channel::ToolStartEvent` but uses borrowed
/// strings to avoid per-call allocation in the dispatcher hot path. The `AgentChannelView`
/// impl converts this to the canonical event before forwarding.
#[derive(Debug, Clone, Copy)]
pub struct ToolEventStart<'a> {
    /// Name of the tool being started.
    pub tool_name: &'a str,
    /// Unique ID for this tool use invocation.
    pub tool_use_id: &'a str,
    /// Parent tool use ID for nested invocations, if any.
    pub parent_id: Option<&'a str>,
    /// Short human-readable summary of the tool arguments.
    pub args_summary: Option<&'a str>,
}

/// Borrowed payload for a tool-output event.
///
/// `body` uses `&'a str` to borrow from the already-owned `String` in the dispatcher.
/// F4 note: for large tool outputs this avoids an allocation + copy through the seam;
/// callers can pass the full `String`'s borrow without cloning.
#[derive(Debug, Clone, Copy)]
pub struct ToolEventOutput<'a> {
    /// Name of the tool that produced the output.
    pub tool_name: &'a str,
    /// Tool use ID this output belongs to.
    pub tool_use_id: &'a str,
    /// Full body of the tool output (may be large — KB to MB for shell/grep results).
    pub body: &'a str,
    /// Whether the output represents an error condition.
    pub is_error: bool,
    /// Whether the output was streamed incrementally.
    pub streamed: bool,
}

/// Minimal async sink the tool dispatcher needs from an agent channel implementation.
///
/// **Implementor side:** `zeph-core` provides exactly one impl —
/// `impl<C: Channel> AgentChannel for AgentChannelView<'_, C>` — which forwards through the
/// wrapped `&mut C`. The trait is intentionally narrow so the impl is mechanical and the
/// surface area for breakage is small.
///
/// **Caller side:** the dispatcher takes `Ch: AgentChannel` as a generic parameter (NOT
/// `&mut dyn AgentChannel`). Generic dispatch keeps the per-token hot path free of
/// `Box`/`Pin` allocations. The trait is not dyn-safe because `impl Future` return types
/// require monomorphization.
///
/// **Sealing:** this trait is sealed via [`Sealed`]. External crates cannot implement it.
/// Adding a method to `AgentChannel` is non-breaking for downstream — there are no
/// downstream impls.
pub trait AgentChannel: Sealed + Send {
    /// Send free-form assistant text to the user surface.
    fn send(&mut self, text: &str) -> impl Future<Output = Result<(), ChannelSinkError>> + Send;

    /// Emit a transient status line (TUI spinner / CLI dim line / Telegram typing-then-edit).
    fn send_status(
        &mut self,
        text: &str,
    ) -> impl Future<Output = Result<(), ChannelSinkError>> + Send;

    /// Best-effort typing indicator. No-op on channels that don't support it.
    fn send_typing(&mut self) -> impl Future<Output = Result<(), ChannelSinkError>> + Send;

    /// Flush any per-token chunks accumulated during streaming.
    fn flush_chunks(&mut self) -> impl Future<Output = Result<(), ChannelSinkError>> + Send;

    /// Ask the user a yes/no confirmation question.
    fn confirm(
        &mut self,
        prompt: &str,
    ) -> impl Future<Output = Result<bool, ChannelSinkError>> + Send;

    /// Notify the channel that the assistant turn stopped for `reason`.
    ///
    /// `reason` is a primitive string code such as `"max_tokens"`, `"cancelled"`,
    /// `"max_turn_requests"`, or `"timeout"`. The `AgentChannelView` impl maps these to the
    /// concrete `zeph_core::channel::StopHint` enum and forwards.
    fn send_stop_hint(
        &mut self,
        reason: &str,
    ) -> impl Future<Output = Result<(), ChannelSinkError>> + Send;

    /// Emit a tool-start event.
    ///
    /// The payload is [`ToolEventStart`] — a small borrowed struct local to this crate.
    /// The `AgentChannelView` impl converts this to the canonical `zeph_core::channel::ToolStartEvent`
    /// before forwarding to `Channel::send_tool_start`.
    fn send_tool_start(
        &mut self,
        event: ToolEventStart<'_>,
    ) -> impl Future<Output = Result<(), ChannelSinkError>> + Send;

    /// Emit a tool-output event.
    ///
    /// Same conversion contract as [`send_tool_start`](Self::send_tool_start).
    fn send_tool_output(
        &mut self,
        event: ToolEventOutput<'_>,
    ) -> impl Future<Output = Result<(), ChannelSinkError>> + Send;
}
