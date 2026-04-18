// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/loop` slash command handler.
//!
//! Syntax:
//! - `/loop <prompt> every <N> <unit>` — start a repeating loop
//! - `/loop stop`                       — cancel the active loop
//!
//! Units: `s`, `sec`, `secs`, `second`, `seconds`,
//!        `m`, `min`, `mins`, `minute`, `minutes`,
//!        `h`, `hr`, `hrs`, `hour`, `hours`.

use std::future::Future;
use std::pin::Pin;

use crate::context::CommandContext;
use crate::{CommandError, CommandHandler, CommandOutput, SlashCategory};

/// Start or stop a repeating prompt loop.
pub struct LoopCommand;

impl CommandHandler<CommandContext<'_>> for LoopCommand {
    fn name(&self) -> &'static str {
        "/loop"
    }

    fn description(&self) -> &'static str {
        "Repeat a prompt on a fixed interval, or stop the active loop"
    }

    fn args_hint(&self) -> &'static str {
        "<prompt> every <N> <unit> | stop | status"
    }

    fn category(&self) -> SlashCategory {
        SlashCategory::Advanced
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a mut CommandContext<'_>,
        args: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>> {
        Box::pin(async move {
            match ctx.agent.handle_loop(args).await? {
                msg if msg.is_empty() => Ok(CommandOutput::Silent),
                msg => Ok(CommandOutput::Message(msg)),
            }
        })
    }
}

/// Parse `/loop <prompt> every <N> <unit>` — returns `(prompt, interval_secs)`.
///
/// Returns `Err` with a user-visible message on any syntax or value error.
///
/// # Errors
///
/// Returns `Err` when the syntax is invalid or `N` is not a positive integer.
pub fn parse_loop_args(args: &str) -> Result<(String, u64), CommandError> {
    // Find " every " as the separator between prompt and interval spec.
    // We search from the right so prompts that contain the word "every" work correctly.
    let sep = " every ";
    let sep_pos = args.rfind(sep).ok_or_else(|| {
        CommandError::new(
            "Usage: /loop <prompt> every <N> <unit>  (e.g. /loop check logs every 10 minutes)",
        )
    })?;

    let prompt = args[..sep_pos].trim().to_owned();
    if prompt.is_empty() {
        return Err(CommandError::new("Prompt must not be empty."));
    }

    let interval_str = args[sep_pos + sep.len()..].trim();
    let (n_str, unit) = interval_str.split_once(' ').ok_or_else(|| {
        CommandError::new("Expected format: every <N> <unit>  (e.g. every 5 minutes)")
    })?;

    let n: u64 = n_str
        .parse()
        .map_err(|_| CommandError::new(format!("Expected a positive integer, got '{n_str}'")))?;
    if n == 0 {
        return Err(CommandError::new("Interval must be greater than zero."));
    }

    let multiplier: u64 = match unit.trim() {
        "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
        other => {
            return Err(CommandError::new(format!(
                "Unknown time unit '{other}'. Use: s/sec/seconds, m/min/minutes, h/hr/hours"
            )));
        }
    };

    Ok((prompt, n * multiplier))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_seconds() {
        let (prompt, secs) = parse_loop_args("check logs every 10 seconds").unwrap();
        assert_eq!(prompt, "check logs");
        assert_eq!(secs, 10);
    }

    #[test]
    fn parse_minutes() {
        let (prompt, secs) = parse_loop_args("summarize recent activity every 5 minutes").unwrap();
        assert_eq!(prompt, "summarize recent activity");
        assert_eq!(secs, 300);
    }

    #[test]
    fn parse_hours() {
        let (prompt, secs) = parse_loop_args("daily report every 1 hour").unwrap();
        assert_eq!(prompt, "daily report");
        assert_eq!(secs, 3600);
    }

    #[test]
    fn parse_short_units() {
        let (_, s) = parse_loop_args("ping every 30 s").unwrap();
        assert_eq!(s, 30);
        let (_, m) = parse_loop_args("ping every 2 m").unwrap();
        assert_eq!(m, 120);
        let (_, h) = parse_loop_args("ping every 1 h").unwrap();
        assert_eq!(h, 3600);
    }

    #[test]
    fn parse_prompt_with_every_word() {
        // "every" appears in the prompt too — rfind picks the last occurrence.
        let (prompt, secs) = parse_loop_args("check every file in dir every 15 sec").unwrap();
        assert_eq!(prompt, "check every file in dir");
        assert_eq!(secs, 15);
    }

    #[test]
    fn parse_missing_every() {
        assert!(parse_loop_args("check logs 10 seconds").is_err());
    }

    #[test]
    fn parse_empty_prompt() {
        assert!(parse_loop_args("every 5 seconds").is_err());
    }

    #[test]
    fn parse_zero_n() {
        assert!(parse_loop_args("ping every 0 seconds").is_err());
    }

    #[test]
    fn parse_bad_unit() {
        assert!(parse_loop_args("ping every 5 fortnights").is_err());
    }

    #[test]
    fn parse_bad_n() {
        assert!(parse_loop_args("ping every abc seconds").is_err());
    }
}
