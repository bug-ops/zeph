// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::error::OrchestrationError;

/// Typed representation of a parsed `/plan` CLI command.
///
/// # Parsing ambiguity
///
/// Goals that begin with a reserved word (`status`, `list`, `cancel`, `confirm`, `resume`, `retry`)
/// will be interpreted as that subcommand, not as a goal. For example:
/// `/plan status report` parses as `Status(Some("report"))`.
/// To work around this, rephrase the goal: `/plan write a status report`.
#[derive(Debug, PartialEq)]
pub enum PlanCommand {
    /// `/plan <goal>` — decompose goal, confirm, execute, aggregate.
    Goal(String),
    /// `/plan status` or `/plan status <graph-id>` — show DAG progress.
    Status(Option<String>),
    /// `/plan list` — list recent graphs from persistence.
    List,
    /// `/plan cancel` or `/plan cancel <graph-id>` — cancel active/specific graph.
    Cancel(Option<String>),
    /// `/plan confirm` — confirm pending plan before execution.
    Confirm,
    /// `/plan resume` or `/plan resume <graph-id>` — resume a paused graph (Ask strategy).
    Resume(Option<String>),
    /// `/plan retry` or `/plan retry <graph-id>` — re-run failed tasks in a graph.
    Retry(Option<String>),
}

impl PlanCommand {
    /// Parse from raw input text starting with `/plan`.
    ///
    /// # Errors
    ///
    /// Returns [`OrchestrationError::InvalidCommand`] if parsing fails.
    pub fn parse(input: &str) -> Result<Self, OrchestrationError> {
        let rest = input
            .strip_prefix("/plan")
            .ok_or_else(|| {
                OrchestrationError::InvalidCommand("input must start with /plan".into())
            })?
            .trim();

        if rest.is_empty() {
            return Err(OrchestrationError::InvalidCommand(
                "usage: /plan <goal> | /plan status [id] | /plan list | /plan cancel [id] \
                 | /plan confirm | /plan resume [id] | /plan retry [id]\n\
                 Note: goals starting with a reserved word are parsed as subcommands."
                    .into(),
            ));
        }

        let (cmd, args) = rest.split_once(' ').unwrap_or((rest, ""));
        let cmd = cmd.trim();
        let args = args.trim();

        match cmd {
            "status" => Ok(Self::Status(if args.is_empty() {
                None
            } else {
                Some(args.to_owned())
            })),
            "list" => {
                if !args.is_empty() {
                    return Err(OrchestrationError::InvalidCommand(
                        "/plan list takes no arguments".into(),
                    ));
                }
                Ok(Self::List)
            }
            "cancel" => Ok(Self::Cancel(if args.is_empty() {
                None
            } else {
                Some(args.to_owned())
            })),
            "confirm" => {
                if !args.is_empty() {
                    return Err(OrchestrationError::InvalidCommand(
                        "/plan confirm takes no arguments".into(),
                    ));
                }
                Ok(Self::Confirm)
            }
            "resume" => Ok(Self::Resume(if args.is_empty() {
                None
            } else {
                Some(args.to_owned())
            })),
            "retry" => Ok(Self::Retry(if args.is_empty() {
                None
            } else {
                Some(args.to_owned())
            })),
            // Everything else is treated as a goal (the full `rest` string, not just `cmd`).
            // This means `/plan refactor the auth module` captures the whole phrase.
            _ => Ok(Self::Goal(rest.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_goal_simple() {
        let cmd = PlanCommand::parse("/plan refactor auth module").unwrap();
        assert_eq!(cmd, PlanCommand::Goal("refactor auth module".into()));
    }

    #[test]
    fn parse_goal_multi_word() {
        let cmd = PlanCommand::parse("/plan build a new feature for the dashboard").unwrap();
        assert_eq!(
            cmd,
            PlanCommand::Goal("build a new feature for the dashboard".into())
        );
    }

    #[test]
    fn parse_status_no_id() {
        let cmd = PlanCommand::parse("/plan status").unwrap();
        assert_eq!(cmd, PlanCommand::Status(None));
    }

    #[test]
    fn parse_status_with_id() {
        let cmd = PlanCommand::parse("/plan status abc-123").unwrap();
        assert_eq!(cmd, PlanCommand::Status(Some("abc-123".into())));
    }

    #[test]
    fn parse_list() {
        let cmd = PlanCommand::parse("/plan list").unwrap();
        assert_eq!(cmd, PlanCommand::List);
    }

    #[test]
    fn parse_list_with_args_returns_error() {
        let err = PlanCommand::parse("/plan list all modules").unwrap_err();
        assert!(
            matches!(err, OrchestrationError::InvalidCommand(ref m) if m.contains("no arguments")),
            "expected no-arguments error, got: {err:?}"
        );
    }

    #[test]
    fn parse_list_trailing_args_documents_known_ambiguity() {
        // "/plan list all modules" could be mistaken for a list-all request,
        // but we now return an error instead of silently dropping "all modules".
        let result = PlanCommand::parse("/plan list all modules");
        assert!(
            result.is_err(),
            "should error, not silently drop trailing args"
        );
    }

    #[test]
    fn parse_cancel_no_id() {
        let cmd = PlanCommand::parse("/plan cancel").unwrap();
        assert_eq!(cmd, PlanCommand::Cancel(None));
    }

    #[test]
    fn parse_cancel_with_id() {
        let cmd = PlanCommand::parse("/plan cancel abc-123").unwrap();
        assert_eq!(cmd, PlanCommand::Cancel(Some("abc-123".into())));
    }

    #[test]
    fn parse_cancel_with_phrase_ambiguity() {
        // Known limitation: "/plan cancel the old endpoints" parses as Cancel, not Goal.
        let cmd = PlanCommand::parse("/plan cancel the old endpoints").unwrap();
        assert_eq!(
            cmd,
            PlanCommand::Cancel(Some("the old endpoints".into())),
            "cancel captures the rest as optional id — known ambiguity with natural language goals"
        );
    }

    #[test]
    fn parse_confirm() {
        let cmd = PlanCommand::parse("/plan confirm").unwrap();
        assert_eq!(cmd, PlanCommand::Confirm);
    }

    #[test]
    fn parse_empty_after_prefix_returns_error() {
        let err = PlanCommand::parse("/plan").unwrap_err();
        assert!(
            matches!(err, OrchestrationError::InvalidCommand(ref m) if m.contains("usage")),
            "expected usage error, got: {err:?}"
        );
    }

    #[test]
    fn parse_whitespace_only_after_prefix_returns_error() {
        let err = PlanCommand::parse("/plan   ").unwrap_err();
        assert!(matches!(err, OrchestrationError::InvalidCommand(ref m) if m.contains("usage")));
    }

    #[test]
    fn parse_wrong_prefix_returns_error() {
        let err = PlanCommand::parse("/foo bar").unwrap_err();
        assert!(matches!(err, OrchestrationError::InvalidCommand(ref m) if m.contains("/plan")));
    }

    #[test]
    fn parse_goal_with_unreserved_word_at_start() {
        // "create" is NOT a reserved subcommand — treated as goal.
        let cmd = PlanCommand::parse("/plan create a status report").unwrap();
        assert_eq!(cmd, PlanCommand::Goal("create a status report".into()));
    }

    #[test]
    fn parse_resume_no_id() {
        let cmd = PlanCommand::parse("/plan resume").unwrap();
        assert_eq!(cmd, PlanCommand::Resume(None));
    }

    #[test]
    fn parse_resume_with_id() {
        let cmd = PlanCommand::parse("/plan resume abc-123").unwrap();
        assert_eq!(cmd, PlanCommand::Resume(Some("abc-123".into())));
    }

    #[test]
    fn parse_retry_no_id() {
        let cmd = PlanCommand::parse("/plan retry").unwrap();
        assert_eq!(cmd, PlanCommand::Retry(None));
    }

    #[test]
    fn parse_retry_with_id() {
        let cmd = PlanCommand::parse("/plan retry abc-123").unwrap();
        assert_eq!(cmd, PlanCommand::Retry(Some("abc-123".into())));
    }

    #[test]
    fn parse_confirm_with_trailing_args_returns_error() {
        let err = PlanCommand::parse("/plan confirm abc-123").unwrap_err();
        assert!(
            matches!(err, OrchestrationError::InvalidCommand(ref m) if m.contains("no arguments")),
            "expected no-arguments error, got: {err:?}"
        );
    }

    #[test]
    fn parse_confirm_with_phrase_returns_error() {
        // "/plan confirm the test results" should error, not silently Confirm.
        let err = PlanCommand::parse("/plan confirm the test results").unwrap_err();
        assert!(
            matches!(err, OrchestrationError::InvalidCommand(ref m) if m.contains("no arguments")),
            "expected no-arguments error, got: {err:?}"
        );
    }
}
