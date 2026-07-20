// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`TrackingAccess`] — command handler access to the trajectory sentinel, scope tracking,
//! and the durable goal subsystem.

use std::future::Future;
use std::pin::Pin;

use crate::CommandError;

/// Access to `/trajectory`, `/scope`, and `/goal`.
///
/// Implemented by `zeph-core::Agent<C>`. Part of the [`crate::AgentAccess`] supertrait.
pub trait TrackingAccess {
    // ----- /trajectory -----

    /// Handle `/trajectory [status|reset]` and return a user-visible result.
    fn handle_trajectory(&mut self, args: &str) -> String;

    // ----- /scope -----

    /// Handle `/scope [list [task_type]]` and return a user-visible result.
    fn handle_scope(&self, args: &str) -> String;

    // ----- /goal -----

    /// Execute a `/goal` subcommand (create, pause, resume, clear, complete, status, list).
    ///
    /// `args` contains everything after `/goal` (e.g., `"create buy groceries"`).
    ///
    /// Returns a formatted response string on success, or an error message string.
    /// The default implementation returns an error indicating that goals are not supported,
    /// which is the correct behaviour for contexts where goal tracking is not wired in.
    fn handle_goal<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        let _ = args;
        Box::pin(async move { Err(CommandError::new("/goal is not supported in this context")) })
    }

    /// Return a lightweight snapshot of the currently active goal, if any.
    ///
    /// Used by the TUI status bar and metrics bridge. The default returns `None`.
    fn active_goal_snapshot(&self) -> Option<crate::GoalSnapshot> {
        None
    }
}
