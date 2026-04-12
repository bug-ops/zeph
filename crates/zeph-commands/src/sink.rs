// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Minimal channel I/O trait for slash command handlers.
//!
//! [`ChannelSink`] is the subset of the `Channel` trait actually called by command handlers.
//! Using a trait object (`&mut dyn ChannelSink`) instead of a generic `C: Channel` allows
//! `CommandHandler` to be object-safe and removes the `C` generic from `CommandRegistry`.
//!
//! `zeph-core` implements `ChannelSink` for all concrete channel types via a blanket impl.

use std::future::Future;
use std::pin::Pin;

use super::CommandError;

/// Async I/O interface required by slash command handlers.
///
/// This trait covers exactly the methods called by command handler implementations.
/// `zeph-core` provides a blanket `impl<C: Channel> ChannelSink for C`.
///
/// # Object safety
///
/// All methods return `Pin<Box<dyn Future<...>>>` so the trait is object-safe and can be
/// stored as `&mut dyn ChannelSink`.
pub trait ChannelSink: Send {
    /// Send a text message to the user.
    fn send<'a>(
        &'a mut self,
        msg: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), CommandError>> + Send + 'a>>;

    /// Flush any buffered chunks to the user.
    fn flush_chunks<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), CommandError>> + Send + 'a>>;

    /// Set the pending send-queue item count (used by `/clear-queue`).
    fn send_queue_count<'a>(
        &'a mut self,
        count: usize,
    ) -> Pin<Box<dyn Future<Output = Result<(), CommandError>> + Send + 'a>>;

    /// Returns `true` if the channel supports a hard exit (e.g., CLI).
    fn supports_exit(&self) -> bool;
}
