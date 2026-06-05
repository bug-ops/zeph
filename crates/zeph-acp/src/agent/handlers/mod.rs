// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

pub(crate) mod authenticate;
pub(crate) mod cancel;
pub(crate) mod close_session;
pub(crate) mod delete_session;
pub(crate) mod dispatch;
#[cfg(feature = "unstable-session-fork")]
pub(crate) mod fork_session;
pub(crate) mod initialize;
pub(crate) mod list_sessions;
pub(crate) mod load_session;
pub(crate) mod logout;
pub(crate) mod new_session;
pub(crate) mod prompt;
pub(crate) mod resume_session;
pub(crate) mod set_session_config_option;
pub(crate) mod set_session_mode;
