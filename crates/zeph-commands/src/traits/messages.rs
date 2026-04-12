// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`MessageAccess`] trait for command handlers that read or clear conversation state.
//!
//! Used by `/clear`, `/reset`, and `/clear-queue` handlers.

/// Access to conversation message history and related runtime caches.
///
/// Implemented by `zeph-core` on a struct holding `MessageState`, `ToolState`,
/// `ProviderState`, `MetricsState`, and the tool orchestrator. Grouped into one trait
/// because all of these are mutated together by the clear operation.
pub trait MessageAccess: Send {
    /// Clear conversation history, keeping only the system prompt (first message).
    ///
    /// Also clears tool dependency state, recomputes prompt token count, clears pending
    /// image parts, the tool orchestrator cache, and the user-provided URL tracking set.
    fn clear_history(&mut self);

    /// Return the number of messages currently queued for processing.
    fn queue_len(&self) -> usize;

    /// Discard all queued messages. Returns the number that were discarded.
    fn drain_queue(&mut self) -> usize;

    /// Notify the channel of the updated queue count after clearing, if supported.
    ///
    /// Implementations that cannot access the channel (due to borrow splitting) may be no-ops;
    /// the `/clear-queue` handler calls `ctx.sink.send_queue_count(0)` directly.
    fn notify_queue_count<'a>(
        &'a mut self,
        count: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
}
