// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Regression guard for #5987: `zeph_commands::COMMANDS` (the static list `/help` renders
//! from) has drifted from the real `CommandRegistry` registrations in `Agent::run` at
//! least 4 times. This asserts every handler registered in either of the two production
//! registries (`build_session_debug_registry`, `build_agent_command_registry`) has a
//! matching name in `zeph_commands::COMMANDS`, so a future handler addition without a
//! matching `COMMANDS` entry fails CI instead of silently hiding the command from `/help`.

use crate::agent::slash_commands::{build_agent_command_registry, build_session_debug_registry};

/// Registered in `#[cfg(test)]` builds only (`build_session_debug_registry`) to exercise
/// the non-fatal `CommandError` dispatch path — not a real user-facing command, so it is
/// intentionally excluded from the drift check below.
const TEST_ONLY_STUB_NAME: &str = "/test-error";

#[test]
fn every_registered_command_has_a_commands_rs_entry() {
    let mut missing = Vec::new();

    for info in build_session_debug_registry().list() {
        if info.name == TEST_ONLY_STUB_NAME {
            continue;
        }
        if !zeph_commands::COMMANDS.iter().any(|c| c.name == info.name) {
            missing.push(info.name);
        }
    }
    for info in build_agent_command_registry().list() {
        if !zeph_commands::COMMANDS.iter().any(|c| c.name == info.name) {
            missing.push(info.name);
        }
    }

    assert!(
        missing.is_empty(),
        "handlers registered but missing from zeph_commands::COMMANDS (would be hidden \
         from /help — see #5987): {missing:?}"
    );
}
