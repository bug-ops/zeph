// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

// Handler stubs for ACP 0.11 migration. Implementations land in PR 2 (#3267).
// See .local/plan/acp-migration-plan.md §4 Steps 3–4 for the full contract.

pub(crate) mod authenticate;
pub(crate) mod cancel;
#[cfg(feature = "unstable-session-close")]
pub(crate) mod close_session;
pub(crate) mod dispatch;
#[cfg(feature = "unstable-session-fork")]
pub(crate) mod fork_session;
pub(crate) mod initialize;
pub(crate) mod list_sessions;
pub(crate) mod load_session;
#[cfg(feature = "unstable-logout")]
pub(crate) mod logout;
pub(crate) mod new_session;
pub(crate) mod prompt;
#[cfg(feature = "unstable-session-resume")]
pub(crate) mod resume_session;
pub(crate) mod set_session_config_option;
pub(crate) mod set_session_mode;
#[cfg(feature = "unstable-session-model")]
pub(crate) mod set_session_model;
