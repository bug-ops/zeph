// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared test harness used by every `#[cfg(test)]` module in `crate::agent`.
//! Re-exported from `crate::agent::agent_tests` for cross-module visibility.

pub(crate) use common::*;

mod common;
mod feedback_quick_harness_tests;
mod hot_reload_and_dispatch_tests;
mod image_attachment_tests;
mod lifecycle_tests;
mod metrics_summary_tests;
mod model_help_status_exit_tests;
mod orchestration_persistence_tests;
mod subagent_command_tests;
