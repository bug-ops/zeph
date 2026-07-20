// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`PolicyAccess`] — command handler access to the tool-call policy enforcer.

use std::future::Future;
use std::pin::Pin;

use crate::CommandError;

/// Access to the `/policy` command (policy status and dry-run checks).
///
/// Implemented by `zeph-core::Agent<C>`. Part of the [`crate::AgentAccess`] supertrait.
pub trait PolicyAccess {
    // ----- /policy -----

    /// Handle `/policy [status|check ...]` and return a user-visible result.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the policy is misconfigured or the subcommand is unknown.
    fn handle_policy<'a>(
        &'a mut self,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;
}
