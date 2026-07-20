// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`MiscAccess`] — command handler access to a small residual set of commands that do not
//! share a subsystem with any other sub-trait: the prompt-repeat loop, test notifications,
//! and web search.

use std::future::Future;
use std::pin::Pin;

use crate::CommandError;

/// Access to `/loop`, `/notify-test`, and `/search`.
///
/// Implemented by `zeph-core::Agent<C>`. Part of the [`crate::AgentAccess`] supertrait.
pub trait MiscAccess {
    // ----- /loop -----

    /// Handle `/loop <prompt> every <N> <unit>` or `/loop stop`.
    ///
    /// Starts a repeating loop that injects `prompt` as a new agent turn on each tick,
    /// or stops the currently active loop. Returns a user-visible ACK string.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the arguments are malformed or the interval is below the minimum.
    fn handle_loop<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /notify-test -----

    /// Fire a test notification via all enabled notification channels.
    ///
    /// Returns a status message for the user. If all channels are disabled or the
    /// notifier is not configured, returns a user-visible explanation.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the notification send fails.
    fn notify_test<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /search -----

    /// Execute `/search <query> [--limit N]` by dispatching a `web_search` tool call.
    ///
    /// `args` is everything after `/search`. Returns the rendered result summary, or a
    /// usage/error message. The default returns a "not supported" message.
    fn handle_web_search<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        let _ = args;
        Box::pin(async move { Ok("Web search is not supported in this context.".to_owned()) })
    }
}
