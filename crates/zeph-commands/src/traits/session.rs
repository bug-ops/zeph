// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`SessionAccess`] trait for command handlers that need basic session properties.
//!
//! Used by `/exit` and `/quit` handlers to check whether the channel supports exit.

/// Access to basic session and channel properties.
///
/// Implemented by `zeph-core` on a thin wrapper that exposes channel and lifecycle state
/// without revealing the full `Agent<C>` structure.
///
/// `Sync` is required so that a shared reference `&dyn SessionAccess` can be sent across
/// thread boundaries in async futures, making `CommandContext` `Send`.
pub trait SessionAccess: Send + Sync {
    /// Returns `true` if the channel supports a hard exit (e.g., CLI).
    ///
    /// When `false`, `/exit` and `/quit` report an error to the user instead of exiting.
    fn supports_exit(&self) -> bool;
}
