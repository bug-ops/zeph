// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`SkillAccess`] — command handler access to the skill registry: stats, versions,
//! trust management, and feedback recording.

use std::future::Future;
use std::pin::Pin;

use crate::CommandError;

/// Access to skill management (`/skill`, `/skills`) and skill outcome feedback (`/feedback`).
///
/// Implemented by `zeph-core::Agent<C>`. Part of the [`crate::AgentAccess`] supertrait.
pub trait SkillAccess {
    // ----- /skill -----

    /// Handle `/skill [subcommand]` and return a user-visible result.
    ///
    /// Subcommands: `stats`, `versions`, `activate`, `approve`, `reset`, `trust`,
    /// `block`, `unblock`, `install`, `remove`, `create`, `scan`, `reject`.
    ///
    /// # Errors
    ///
    /// Returns `Err` when a database or I/O operation fails.
    fn handle_skill<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /skills -----

    /// Handle `/skills [subcommand]` and return a user-visible result.
    ///
    /// Subcommands: (none) list all; `confusability` show pairs with high embedding similarity.
    ///
    /// # Errors
    ///
    /// Returns `Err` when a database or embedding operation fails.
    fn handle_skills<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /feedback -----

    /// Handle `/feedback <skill_name> <message>` and return a user-visible result.
    ///
    /// Records skill outcome feedback and optionally triggers skill improvement.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the database operation fails.
    fn handle_feedback_command<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;
}
