// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`SchedulerAccess`] — command handler access to the cron-based task scheduler.

use std::future::Future;
use std::pin::Pin;

use crate::CommandError;

/// Access to the `/scheduler` command (list scheduled tasks).
///
/// Implemented by `zeph-core::Agent<C>`. Part of the [`crate::AgentAccess`] supertrait.
pub trait SchedulerAccess {
    // ----- /scheduler -----

    /// List scheduled tasks.
    ///
    /// Returns `None` when the scheduler is not enabled.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the tool executor call fails.
    fn list_scheduled_tasks<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, CommandError>> + Send + 'a>>;
}
