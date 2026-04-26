// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

// TODO(critic): #3497 follow-up — replace remaining super:: paths with absolute crate:: paths for clarity

pub mod agent_tests; // path-preserving — DO NOT rename (used by 24+ modules via crate::agent::agent_tests::*)

#[cfg(test)]
mod compaction_e2e;
#[cfg(test)]
mod confirmation_propagation_tests;
#[cfg(test)]
mod flush_orphaned_tests;
#[cfg(test)]
mod inline_tool_loop_tests;
#[cfg(test)]
mod pre_execution_audit_tests;
#[cfg(test)]
mod secret_reason_truncation;
#[cfg(test)]
mod shutdown_summary_tests;
#[cfg(test)]
mod small_misc_tests;
