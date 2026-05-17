// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/agents` fleet view handler: lists autonomous goal sessions and sub-agent definitions.

use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use zeph_config::autonomous::AutonomousState;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Snapshot of a single autonomous goal session for the fleet view.
///
/// Constructed by querying `AutonomousRegistry` (`zeph-core`) and forwarded to
/// [`format_fleet_section`] for display.
#[derive(Debug, Clone)]
pub struct FleetEntry {
    /// UUID string of the goal.
    pub goal_id: String,
    /// First 80 characters of goal text (may be followed by `…`).
    pub goal_text_short: String,
    /// Current autonomous state.
    pub state: AutonomousState,
    /// Number of turns executed so far in this session.
    pub turns_executed: u32,
    /// Maximum turns allowed for this session.
    pub max_turns: u32,
    /// Wall-clock time elapsed since the session started.
    pub elapsed: Duration,
}

/// Format the "Autonomous Goals" header section from a list of [`FleetEntry`] values.
///
/// Returns an empty string when `entries` is empty so callers can skip the section
/// entirely if there are no active sessions.
///
/// # Examples
///
/// ```rust
/// use std::time::Duration;
/// use zeph_commands::handlers::agents_fleet::{FleetEntry, format_fleet_section};
/// use zeph_config::autonomous::AutonomousState;
///
/// let entries = vec![FleetEntry {
///     goal_id: "a1b2c3d4".to_owned(),
///     goal_text_short: "Deploy the new API".to_owned(),
///     state: AutonomousState::Running,
///     turns_executed: 12,
///     max_turns: 20,
///     elapsed: Duration::from_secs(272),
/// }];
///
/// let out = format_fleet_section(&entries);
/// assert!(out.contains("Autonomous Goals:"));
/// assert!(out.contains("a1b2c"));
/// assert!(out.contains("running"));
/// assert!(out.contains("12/20"));
/// ```
#[must_use]
pub fn format_fleet_section(entries: &[FleetEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }

    let mut out = String::from("Autonomous Goals:\n");
    let _ = writeln!(
        out,
        "  {:<8}  {:<30}  {:<10}  {:<8}  Elapsed",
        "ID", "Goal (truncated)", "State", "Turns"
    );

    for e in entries {
        let short_id = &e.goal_id[..8.min(e.goal_id.len())];
        let elapsed = format_elapsed(e.elapsed);
        let _ = writeln!(
            out,
            "  {:<8}  {:<30}  {:<10}  {:<8}  {}",
            short_id,
            truncate_display(&e.goal_text_short, 30),
            e.state.to_string(),
            format!("{}/{}", e.turns_executed, e.max_turns),
            elapsed,
        );
    }

    out
}

fn format_elapsed(d: Duration) -> String {
    let total = d.as_secs();
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}h {m}m {s}s")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

fn truncate_display(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_chars {
        s.to_owned()
    } else {
        let end = s.floor_char_boundary(max_chars.saturating_sub(1));
        format!("{}…", &s[..end])
    }
}

/// Show all active autonomous goal sessions and sub-agent definitions.
///
/// Autonomous goal sessions appear first (via [`format_fleet_section`]), followed
/// by the standard sub-agent definition list produced by `/agents list`.
/// The sub-agent section is omitted when no definitions are found.
pub struct AgentsFleetCommand;

impl CommandHandler<CommandContext<'_>> for AgentsFleetCommand {
    fn name(&self) -> &'static str {
        "/agents"
    }

    fn description(&self) -> &'static str {
        "List active autonomous goal sessions and sub-agent definitions"
    }

    fn args_hint(&self) -> &'static str {
        "[list|show|create|edit|delete <name>]"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Integration
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        use tracing::Instrument as _;
        let span = tracing::info_span!("commands.agents.handle");
        Box::pin(
            async move {
                let result = ctx.agent.handle_agents(args).await?;
                Ok(CommandOutput::Message(result))
            }
            .instrument(span),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        id: &str,
        text: &str,
        state: AutonomousState,
        turns: u32,
        max: u32,
        secs: u64,
    ) -> FleetEntry {
        FleetEntry {
            goal_id: id.to_owned(),
            goal_text_short: text.to_owned(),
            state,
            turns_executed: turns,
            max_turns: max,
            elapsed: Duration::from_secs(secs),
        }
    }

    #[test]
    fn empty_entries_returns_empty_string() {
        assert_eq!(format_fleet_section(&[]), "");
    }

    #[test]
    fn single_running_entry_contains_expected_fields() {
        let entries = vec![entry(
            "a1b2c3d4e5",
            "Deploy the new API",
            AutonomousState::Running,
            12,
            20,
            272,
        )];
        let out = format_fleet_section(&entries);
        assert!(out.contains("Autonomous Goals:"), "header missing");
        assert!(out.contains("a1b2c3d4"), "short ID missing");
        assert!(out.contains("running"), "state missing");
        assert!(out.contains("12/20"), "turns counter missing");
        assert!(out.contains("4m 32s"), "elapsed time missing");
    }

    #[test]
    fn two_entries_both_appear() {
        let entries = vec![
            entry(
                "aaaa1111",
                "First goal",
                AutonomousState::Running,
                3,
                20,
                60,
            ),
            entry(
                "bbbb2222",
                "Second goal",
                AutonomousState::Verifying,
                5,
                10,
                75,
            ),
        ];
        let out = format_fleet_section(&entries);
        assert!(out.contains("aaaa1111"), "first id missing");
        assert!(out.contains("bbbb2222"), "second id missing");
        assert!(out.contains("verifying"), "verifying state missing");
    }

    #[test]
    fn elapsed_hours_formatted_correctly() {
        let entries = vec![entry(
            "cccc3333",
            "Long running goal",
            AutonomousState::Running,
            1,
            5,
            3665,
        )];
        let out = format_fleet_section(&entries);
        assert!(out.contains("1h"), "hours missing");
        assert!(out.contains("1m"), "minutes missing");
    }

    #[test]
    fn elapsed_seconds_only() {
        let entries = vec![entry(
            "dddd4444",
            "Short goal",
            AutonomousState::Achieved,
            1,
            5,
            45,
        )];
        let out = format_fleet_section(&entries);
        assert!(out.contains("45s"), "seconds missing");
    }

    #[test]
    fn long_goal_text_truncated() {
        let long = "a".repeat(80);
        let entries = vec![entry("eeee5555", &long, AutonomousState::Running, 0, 5, 0)];
        let out = format_fleet_section(&entries);
        assert!(out.contains('…'), "truncation indicator missing");
    }

    #[test]
    fn agents_fleet_command_name() {
        assert_eq!(AgentsFleetCommand.name(), "/agents");
        assert!(!AgentsFleetCommand.description().is_empty());
    }

    // Test format_elapsed edge cases
    #[test]
    fn format_elapsed_zero() {
        assert_eq!(format_elapsed(Duration::ZERO), "0s");
    }

    #[test]
    fn format_elapsed_one_minute() {
        assert_eq!(format_elapsed(Duration::from_mins(1)), "1m 0s");
    }

    #[test]
    fn format_elapsed_one_hour() {
        assert_eq!(format_elapsed(Duration::from_hours(1)), "1h 0m 0s");
    }
}
