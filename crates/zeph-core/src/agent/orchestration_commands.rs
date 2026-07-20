// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`zeph_commands::OrchestrationAccess`] implementation for [`Agent<C>`]: `/plan` and
//! `/experiment`.
//!
//! [`Agent<C>`]: super::Agent

use std::future::Future;
use std::pin::Pin;

use zeph_commands::{CommandError, OrchestrationAccess};

use super::Agent;
use super::command_macros::delegate_cmd;
use crate::channel::Channel;

impl<C: Channel + Send + 'static> OrchestrationAccess for Agent<C> {
    // ----- /plan -----

    #[cfg(feature = "scheduler")]
    delegate_cmd!(handle_plan, dispatch_plan_command_as_string, input: &'a str => String);

    #[cfg(not(feature = "scheduler"))]
    fn handle_plan<'a>(
        &'a mut self,
        _input: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(async move { Ok(String::new()) })
    }

    // ----- /experiment -----

    delegate_cmd!(handle_experiment, handle_experiment_command_as_string, input: &'a str => String);
}
