// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`SessionControlAccess`] — command handler access to commands that act on the running
//! session/conversation: recap, compaction, reset, status, undo/redo, and conversation-session
//! persistence.
//!
//! Not to be confused with [`crate::traits::session::SessionAccess`], which exposes passive,
//! read-only session/channel properties (`supports_exit`, `history_expand_default_lines`) and
//! is consumed immutably. This trait groups commands that mutate or act on the live session.

use std::future::Future;
use std::pin::Pin;

use crate::CommandError;

/// Access to session/conversation control commands: recap, compaction, reset, status,
/// undo/redo, and conversation-session persistence.
///
/// Implemented by `zeph-core::Agent<C>`. Part of the [`crate::AgentAccess`] supertrait.
pub trait SessionControlAccess {
    // ----- /recap -----

    /// Produce the session recap text.
    ///
    /// Returns the cached digest when available, otherwise generates a fresh summary of the
    /// current conversation. Non-fatal: on LLM timeout or error the implementor returns a
    /// user-visible message rather than `Err`.
    ///
    /// # Errors
    ///
    /// Returns `Err` only on unrecoverable internal agent errors.
    fn session_recap<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /compact -----

    /// Compact the context window and return a user-visible status string.
    ///
    /// Delegates to the agent's compaction subsystem. Returns a message describing
    /// whether compaction ran, was rejected by the probe, or there was nothing to compact.
    ///
    /// # Errors
    ///
    /// Returns `Err` when an internal agent error occurs.
    fn compact_context<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /new -----

    /// Start a new conversation and return a user-visible status string.
    ///
    /// `keep_plan` preserves the current plan. `no_digest` skips saving a digest of
    /// the previous conversation. Returns a formatted string with old and new session IDs.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the reset operation fails.
    fn reset_conversation<'a>(
        &'a mut self,
        keep_plan: bool,
        no_digest: bool,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /cache-stats -----

    /// Return formatted tool orchestrator cache statistics.
    fn cache_stats(&self) -> String;

    // ----- /status -----

    /// Return a formatted session status string.
    ///
    /// # Errors
    ///
    /// Returns `Err` when an internal agent error occurs.
    fn session_status<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /guardrail -----

    /// Return formatted guardrail status.
    fn guardrail_status(&self) -> String;

    // ----- /focus -----

    /// Return formatted Focus Agent status.
    fn focus_status(&self) -> String;

    // ----- /sidequest -----

    /// Return formatted `SideQuest` eviction stats.
    fn sidequest_status(&self) -> String;

    // ----- /image -----

    /// Load an image from `path` and enqueue it for the next message.
    ///
    /// Returns a user-visible confirmation or error string.
    ///
    /// # Errors
    ///
    /// Returns `Err` when an internal agent error occurs.
    fn load_image<'a>(
        &'a mut self,
        path: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /undo, /redo -----

    /// Execute `/undo [N]` or `/undo list`.
    ///
    /// `args` is everything after `/undo`. Empty string means undo 1 step.
    /// Returns a formatted response string. The default returns a "not supported" message.
    fn handle_undo<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        let _ = args;
        Box::pin(async move { Ok("Undo is not supported in this context.".to_owned()) })
    }

    /// Execute `/redo`.
    ///
    /// Returns a formatted response string. The default returns a "not supported" message.
    fn handle_redo<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        let _ = args;
        Box::pin(async move { Ok("Redo is not supported in this context.".to_owned()) })
    }

    // ----- /conv -----

    /// Execute `/conv [list]` or `/conv show <id>` (spec-068, #5343).
    ///
    /// `args` is everything after `/conv`. Empty string and `"list"` both list durable
    /// conversation-sessions; `"show <id>"` returns one session's metadata. Mirrors
    /// `zeph serve-sessions`'s `GET /sessions`/`GET /sessions/:id` REST endpoints, reading
    /// through the same `zeph_session::SessionStore`.
    ///
    /// Returns a formatted response string. The default returns a "not supported" message —
    /// only channels backed by an `Agent` with `[session] enabled = true` override this.
    fn handle_conv<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        let _ = args;
        Box::pin(async move {
            Ok("Conversation-session persistence is not enabled in this context.".to_owned())
        })
    }
}
