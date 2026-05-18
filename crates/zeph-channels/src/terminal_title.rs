// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Terminal title management for CLI sessions (#3904).
//!
//! Sets the terminal window title via ANSI escape sequence `\x1b]2;<title>\x07`.
//! Only effective in terminal emulators that support OSC 2 (virtually all modern ones).

use std::io::Write as _;

/// Set the terminal window title.
///
/// Strips all Unicode control characters from `title` before writing to prevent
/// ANSI escape injection (e.g. from a user-controlled agent name in config).
/// No-ops if stdout is not connected to a terminal.
pub fn set_terminal_title(title: &str) {
    if !crossterm::tty::IsTty::is_tty(&std::io::stdout()) {
        return;
    }
    let safe: String = title.chars().filter(|c| !c.is_control()).collect();
    let _ = write!(std::io::stdout(), "\x1b]2;{safe}\x07");
    let _ = std::io::stdout().flush();
}

/// Set the title to `[action required] <agent_name>` to signal that the agent
/// is waiting for user input.
pub fn set_action_required(agent_name: &str) {
    set_terminal_title(&format!("[action required] {agent_name}"));
}

/// Reset the title to `<agent_name>` when the agent starts a new turn.
pub fn clear_action_required(agent_name: &str) {
    set_terminal_title(agent_name);
}

#[cfg(test)]
mod tests {
    #[test]
    fn strips_control_chars() {
        let title = "foo\x1b[31mbar\x07baz";
        let safe: String = title.chars().filter(|c| !c.is_control()).collect();
        assert!(!safe.contains('\x1b'));
        assert!(!safe.contains('\x07'));
        assert!(safe.contains("foo"));
        assert!(safe.contains("bar"));
        assert!(safe.contains("baz"));
    }

    #[test]
    fn normal_title_unchanged() {
        let title = "zeph - agent ready";
        let safe: String = title.chars().filter(|c| !c.is_control()).collect();
        assert_eq!(safe, title);
    }
}
