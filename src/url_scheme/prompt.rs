// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Prompt confirmation gate for deep-link sessions (spec-066, TASK-7).
//!
//! Implements INV-NOTTY (no-TTY → discard) and the interactive y/N gate controlled by
//! `DeepLinkConfig::confirm_before_prompt`.

use std::io::IsTerminal as _;
use std::io::Write as _;

/// Result of the confirmation gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmResult {
    /// Prompt accepted; inject into session.
    Accepted,
    /// User explicitly declined with any input other than `y`/`Y`.
    Declined,
    /// No TTY available; prompt discarded per INV-NOTTY.
    Discarded,
}

/// Gate a deep-link prompt through user confirmation.
///
/// Behaviour:
/// - `confirm_before_prompt = false` → returns [`ConfirmResult::Accepted`] without interaction.
/// - No TTY → logs a `WARN` and returns [`ConfirmResult::Discarded`] (INV-NOTTY).
/// - TTY present → prints the prompt text and asks `Accept? [y/N]`; `y`/`Y` → Accepted, else Declined.
///
/// Both `Discarded` and `Declined` result in a blank session start; callers must log a WARN
/// entry before proceeding.
pub fn confirm_prompt(prompt: &str, confirm_before_prompt: bool) -> ConfirmResult {
    if !confirm_before_prompt {
        return ConfirmResult::Accepted;
    }

    if !std::io::stdin().is_terminal() {
        tracing::warn!(
            "deep-link: prompt discarded — no TTY available (INV-NOTTY); starting blank session"
        );
        return ConfirmResult::Discarded;
    }

    // Display decoded prompt and ask user.
    println!("Deep-link prompt:\n---\n{prompt}\n---");
    print!("Accept? [y/N] ");
    // Flush stdout so the prompt appears before readline.
    let _ = std::io::stdout().flush();

    let mut input = String::new();
    match std::io::stdin().read_line(&mut input) {
        Ok(_) => {
            if input.trim().eq_ignore_ascii_case("y") {
                ConfirmResult::Accepted
            } else {
                ConfirmResult::Declined
            }
        }
        Err(e) => {
            tracing::warn!("deep-link: failed to read confirmation input: {e}; discarding prompt");
            ConfirmResult::Discarded
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirm_prompt_accepted_when_confirm_disabled() {
        // confirm_before_prompt = false → always accepted regardless of TTY state.
        let result = confirm_prompt("test prompt", false);
        assert_eq!(result, ConfirmResult::Accepted);
    }

    #[test]
    fn confirm_prompt_discarded_when_no_tty_and_confirm_enabled() {
        // In test environments stdin is not a TTY, so this must return Discarded.
        // Only runs when stdin is actually not a terminal (CI / piped test).
        if std::io::stdin().is_terminal() {
            return; // Skip in interactive test sessions.
        }
        let result = confirm_prompt("test prompt", true);
        assert_eq!(result, ConfirmResult::Discarded);
    }
}
