// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`OrchestrationAccess`] — command handler access to the task orchestration (DAG plan)
//! and experiment engine subsystems.

use std::future::Future;
use std::pin::Pin;

use crate::CommandError;

/// Access to the `/plan` and `/experiment` commands.
///
/// Implemented by `zeph-core::Agent<C>`. Part of the [`crate::AgentAccess`] supertrait.
pub trait OrchestrationAccess {
    // ----- /plan -----

    /// Dispatch a `/plan` command and send output via the agent channel.
    ///
    /// `input` is the full trimmed command string (e.g. `"/plan status"`).
    /// Returns `Ok(())` on success.
    ///
    /// # Errors
    ///
    /// Returns `Err` when a channel send or orchestration error occurs.
    fn handle_plan<'a>(
        &'a mut self,
        input: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;

    // ----- /experiment -----

    /// Dispatch a `/experiment` command and send output via the agent channel.
    ///
    /// `input` is the full trimmed command string (e.g. `"/experiment start"`).
    ///
    /// # Errors
    ///
    /// Returns `Err` when a channel send or experiment operation fails.
    fn handle_experiment<'a>(
        &'a mut self,
        input: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>>;
}
