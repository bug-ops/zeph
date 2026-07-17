// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};

use super::error::OrchestrationError;
use super::graph::TaskId;

#[non_exhaustive]
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

/// Reference to a task graph node used by [`HandoffCommand::goto`].
///
/// Resolved against a `TaskGraph` by `dag::try_handoff`. A wire value that parses as a
/// `u32` is treated as [`TaskRef::ById`]; anything else becomes [`TaskRef::ByTitle`],
/// resolved by exact, case-sensitive title match — an ambiguous
/// (duplicate-title) or absent match is rejected by `try_handoff` as
/// `OrchestrationError::InvalidHandoffTarget`, never guessed (spec-080 §6 Ask First).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskRef {
    /// Reference by dense `TaskId` index.
    ById(TaskId),
    /// Reference by exact task title.
    ByTitle(String),
}

/// Parsed `Command`-style dynamic task handoff directive (spec-080, GitHub #6363).
///
/// Produced by [`parse_handoff_command`] from a node's trailing ` ```zeph-command ` fenced
/// output block (FR-B-002). `update` is carried here only so the zeph-core produce-side seam
/// (Phase 2) can sanitize and persist it into the cross-thread store — `zeph-orchestration`
/// never reads or applies it; `dag::try_handoff` consumes only `goto`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffCommand {
    /// Target task to route execution to next.
    pub goto: TaskRef,
    /// Key-value pairs to write into the cross-thread store's `orch/{graph_id}` namespace.
    /// Parsed and carried here for zeph-core to sanitize and persist (spec-080 §5.2 steps
    /// 2-3); never read or applied by zeph-orchestration.
    pub update: Vec<(String, String)>,
}

/// Wire-format shape of a parsed `zeph-command` JSON block, before `goto` resolution and
/// `update` value-type validation.
#[derive(Debug, Deserialize)]
struct RawHandoffCommand {
    goto: String,
    #[serde(default)]
    update: serde_json::Map<String, serde_json::Value>,
}

/// Fence marker opening a `zeph-command` block. Must appear at the start of a line.
const FENCE_OPEN: &str = "```zeph-command";
/// Fence marker closing a `zeph-command` block (plain markdown code-fence close).
const FENCE_CLOSE: &str = "```";

/// Parse `raw` into a [`TaskRef`]: a value that parses as `u32` is [`TaskRef::ById`],
/// anything else is [`TaskRef::ByTitle`].
fn parse_task_ref(raw: &str) -> TaskRef {
    match raw.parse::<u32>() {
        Ok(n) => TaskRef::ById(TaskId(n)),
        Err(_) => TaskRef::ByTitle(raw.to_owned()),
    }
}

/// Extract the JSON body of a sole-trailing ` ```zeph-command ` fenced block from `trimmed`.
///
/// Returns `None` when the fenced block is absent, is not the final content of `trimmed`
/// (FR-B-002's "sole trailing" requirement — any content after the closing fence, once
/// trailing whitespace is stripped by the caller, disqualifies the block), or the
/// fence-open marker is not immediately followed by whitespace (guards against a longer
/// identifier like ` ```zeph-command-x `).
///
/// # Limitations
///
/// Uses `rfind` for the opening marker, so a JSON string value inside the block that itself
/// contains the literal text `` ```zeph-command `` could misdirect extraction. This is an
/// MVP limitation, not a security boundary — the sanitizer scan (FR-B-003) runs on whatever
/// is extracted, and a genuinely adversarial payload is caught there, not here.
fn extract_fenced_body(trimmed: &str) -> Option<&str> {
    let before_close = trimmed.strip_suffix(FENCE_CLOSE)?;
    let open_idx = before_close.rfind(FENCE_OPEN)?;
    if open_idx != 0 && before_close.as_bytes()[open_idx - 1] != b'\n' {
        return None;
    }
    let after_marker = &before_close[open_idx + FENCE_OPEN.len()..];
    let starts_with_boundary = after_marker.chars().next().is_none_or(char::is_whitespace);
    if !starts_with_boundary {
        return None;
    }
    let body = after_marker.trim();
    if body.is_empty() { None } else { Some(body) }
}

/// Returns `true` if `text`'s trailing content contains a well-formed `zeph-command`
/// fence-open marker (start-of-line, followed by a whitespace/EOF boundary) that is
/// genuinely positioned as the trailing block — either closed with nothing but the fence
/// itself following (mirrors `extract_fenced_body`'s own `strip_suffix(FENCE_CLOSE)`
/// precondition), or opened-and-never-closed at all, i.e. abandoned at the very tail of
/// the message.
///
/// Lets callers (zeph-core, spec-080 Phase 2) distinguish "no handoff attempted at all"
/// (returns `false` — ordinary `Completed`, FR-B-001) from "a handoff was attempted but
/// [`parse_handoff_command`] returned `None`" (returns `true` while parsing failed —
/// malformed, must become `TaskOutcome::Failed`, FR-B-009). `parse_handoff_command`
/// itself cannot make this distinction — it returns `None` for both cases equally.
///
/// # Position gating (critic finding P4)
///
/// An earlier version matched *any* well-formed-looking fence-open anywhere in `text`,
/// which false-positived on ordinary output that merely mentions/demonstrates the
/// `` ```zeph-command `` marker (e.g. an agent explaining the syntax) followed by real,
/// unrelated closing content — that block is not the trailing directive, so it must fall
/// through to ordinary `Completed`, not be discarded as malformed `Failed`. This function
/// now only recognizes a fence-open as "attempted" when it is closed with nothing after
/// the close but whitespace, or left open with no closing fence anywhere after it — both
/// of which genuinely describe the tail of the message, never a mid-response aside.
///
/// # Examples
///
/// ```rust
/// use zeph_orchestration::command::has_handoff_fence;
///
/// assert!(has_handoff_fence("output\n```zeph-command\n{not valid json\n```"));
/// assert!(!has_handoff_fence("plain output, no command block"));
/// ```
#[must_use]
pub fn has_handoff_fence(text: &str) -> bool {
    let trimmed = text.trim_end();

    // Closed case: mirrors extract_fenced_body's own sole-trailing precondition exactly
    // — the open marker must be the one immediately preceding the text's final closing
    // fence, with correct start-of-line/whitespace boundaries on both sides.
    if let Some(before_close) = trimmed.strip_suffix(FENCE_CLOSE)
        && let Some(open_idx) = before_close.rfind(FENCE_OPEN)
    {
        let boundary_before = open_idx == 0 || before_close.as_bytes()[open_idx - 1] == b'\n';
        let after_marker = &before_close[open_idx + FENCE_OPEN.len()..];
        let boundary_after = after_marker.chars().next().is_none_or(char::is_whitespace);
        if boundary_before && boundary_after {
            return true;
        }
    }

    // Unclosed case: the last fence-open marker in the text has no closing fence
    // anywhere after it — a genuinely truncated/abandoned attempt at the tail, not an
    // earlier demonstration later superseded by unrelated, properly-closed content.
    if let Some(open_idx) = trimmed.rfind(FENCE_OPEN) {
        let boundary_before = open_idx == 0 || trimmed.as_bytes()[open_idx - 1] == b'\n';
        let after_marker = &trimmed[open_idx + FENCE_OPEN.len()..];
        let boundary_after = after_marker.chars().next().is_none_or(char::is_whitespace);
        let closed_afterward = after_marker.contains(FENCE_CLOSE);
        if boundary_before && boundary_after && !closed_afterward {
            return true;
        }
    }

    false
}

/// Parse a node's raw output text for a trailing Command-style handoff directive.
///
/// Looks for a sole-trailing ` ```zeph-command\n{"goto": "<id\|title>", "update": {...}}\n``` `
/// fenced JSON block (FR-B-002) and returns the parsed [`HandoffCommand`]. Returns `None`
/// both when no such block is present at all, and when a trailing block is present but
/// malformed, partial, or has a non-string `update` value — the caller (zeph-core,
/// Phase 2) is responsible for distinguishing "absent" (ordinary `Completed`, FR-B-001)
/// from "malformed" (`TaskOutcome::Failed`, FR-B-009); this function only answers "did a
/// well-formed directive parse."
///
/// This is a pure, I/O-free function — no sanitizer scan, no store access, no routing
/// decision. Callers must run the result through the sanitizer/`ExfiltrationGuard` path
/// (FR-B-003) before it drives anything.
///
/// # Examples
///
/// ```rust
/// use zeph_orchestration::command::{parse_handoff_command, HandoffCommand, TaskRef};
/// use zeph_orchestration::TaskId;
///
/// let text = "Investigated the failure.\n```zeph-command\n{\"goto\": \"2\", \"update\": {\"finding\": \"root cause found\"}}\n```";
/// let cmd = parse_handoff_command(text).unwrap();
/// assert_eq!(cmd.goto, TaskRef::ById(TaskId(2)));
/// assert_eq!(cmd.update, vec![("finding".to_string(), "root cause found".to_string())]);
///
/// assert!(parse_handoff_command("plain output, no command block").is_none());
/// ```
#[must_use]
pub fn parse_handoff_command(text: &str) -> Option<HandoffCommand> {
    let trimmed = text.trim_end();
    let body = extract_fenced_body(trimmed)?;
    let raw: RawHandoffCommand = serde_json::from_str(body).ok()?;

    let mut update = Vec::with_capacity(raw.update.len());
    for (key, value) in raw.update {
        let value = value.as_str()?.to_owned();
        update.push((key, value));
    }

    Some(HandoffCommand {
        goto: parse_task_ref(&raw.goto),
        update,
    })
}

#[cfg(test)]
mod handoff_command_tests {
    use super::*;

    #[test]
    fn parse_task_ref_numeric_is_by_id() {
        assert_eq!(parse_task_ref("3"), TaskRef::ById(TaskId(3)));
    }

    #[test]
    fn parse_task_ref_non_numeric_is_by_title() {
        assert_eq!(
            parse_task_ref("summarize findings"),
            TaskRef::ByTitle("summarize findings".to_string())
        );
    }

    #[test]
    fn parse_handoff_command_by_id_with_update() {
        let text =
            "some output\n```zeph-command\n{\"goto\": \"1\", \"update\": {\"k\": \"v\"}}\n```";
        let cmd = parse_handoff_command(text).expect("should parse");
        assert_eq!(cmd.goto, TaskRef::ById(TaskId(1)));
        assert_eq!(cmd.update, vec![("k".to_string(), "v".to_string())]);
    }

    #[test]
    fn parse_handoff_command_by_title_no_update() {
        let text = "```zeph-command\n{\"goto\": \"fallback task\"}\n```";
        let cmd = parse_handoff_command(text).expect("should parse");
        assert_eq!(cmd.goto, TaskRef::ByTitle("fallback task".to_string()));
        assert!(cmd.update.is_empty());
    }

    #[test]
    fn parse_handoff_command_absent_block_returns_none() {
        assert!(parse_handoff_command("just plain text output").is_none());
    }

    #[test]
    fn parse_handoff_command_trailing_whitespace_after_fence_is_ok() {
        let text = "```zeph-command\n{\"goto\": \"0\"}\n```\n\n  \n";
        assert!(parse_handoff_command(text).is_some());
    }

    #[test]
    fn parse_handoff_command_extra_text_after_fence_is_malformed() {
        let text = "```zeph-command\n{\"goto\": \"0\"}\n```\nsome trailing prose";
        assert!(parse_handoff_command(text).is_none());
    }

    #[test]
    fn parse_handoff_command_malformed_json_returns_none() {
        let text = "```zeph-command\n{not valid json\n```";
        assert!(parse_handoff_command(text).is_none());
    }

    #[test]
    fn parse_handoff_command_missing_goto_returns_none() {
        let text = "```zeph-command\n{\"update\": {}}\n```";
        assert!(parse_handoff_command(text).is_none());
    }

    #[test]
    fn parse_handoff_command_non_string_update_value_returns_none() {
        let text = "```zeph-command\n{\"goto\": \"0\", \"update\": {\"k\": 42}}\n```";
        assert!(parse_handoff_command(text).is_none());
    }

    #[test]
    fn parse_handoff_command_similar_fence_name_is_not_matched() {
        // Guard: a longer identifier like ```zeph-command-x must not be mistaken for
        // the real fence marker.
        let text = "```zeph-command-x\n{\"goto\": \"0\"}\n```";
        assert!(parse_handoff_command(text).is_none());
    }

    #[test]
    fn parse_handoff_command_no_leading_text_is_ok() {
        let text = "```zeph-command\n{\"goto\": \"0\"}\n```";
        assert!(parse_handoff_command(text).is_some());
    }

    // ── has_handoff_fence (spec-080 Phase 2 parse-ambiguity gap) ───────────────────

    #[test]
    fn has_handoff_fence_absent_returns_false() {
        assert!(!has_handoff_fence("just plain text output"));
    }

    #[test]
    fn has_handoff_fence_well_formed_returns_true() {
        let text = "some output\n```zeph-command\n{\"goto\": \"1\"}\n```";
        assert!(has_handoff_fence(text));
    }

    #[test]
    fn has_handoff_fence_malformed_json_still_returns_true() {
        // Fence marker present even though the body is not valid JSON — this is exactly
        // the "attempted but malformed" case the function exists to detect.
        let text = "```zeph-command\n{not valid json\n```";
        assert!(has_handoff_fence(text));
        assert!(parse_handoff_command(text).is_none());
    }

    #[test]
    fn has_handoff_fence_closed_block_with_real_trailing_content_is_not_attempted() {
        // Regression for critic finding P4: a fence block that is NOT the trailing
        // content of the message (e.g. the agent demonstrating the syntax mid-response,
        // followed by substantive, unrelated real content) must not be treated as an
        // attempted handoff — only a block that is genuinely the tail of the message
        // counts. Before the P4 fix this returned true (false positive), discarding the
        // node's real, good output as malformed `TaskOutcome::Failed`.
        let text = "Let me explain the syntax:\n```zeph-command\n{\"goto\": \"1\"}\n```\n\
                     Now here is my actual finding: the bug was in auth.rs.";
        assert!(!has_handoff_fence(text));
        assert!(parse_handoff_command(text).is_none());
    }

    #[test]
    fn has_handoff_fence_no_closing_fence_still_returns_true() {
        let text = "```zeph-command\n{\"goto\": \"0\"}";
        assert!(has_handoff_fence(text));
        assert!(parse_handoff_command(text).is_none());
    }

    #[test]
    fn has_handoff_fence_similar_fence_name_returns_false() {
        // Guard mirrors parse_handoff_command_similar_fence_name_is_not_matched: a longer
        // identifier like ```zeph-command-x must not be mistaken for the real fence
        // marker, or ordinary output discussing the feature would be wrongly failed.
        let text = "```zeph-command-x\n{\"goto\": \"0\"}\n```";
        assert!(!has_handoff_fence(text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

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
        assert_matches!(err, OrchestrationError::InvalidCommand(ref m) if m.contains("usage"));
    }

    #[test]
    fn parse_wrong_prefix_returns_error() {
        let err = PlanCommand::parse("/foo bar").unwrap_err();
        assert_matches!(err, OrchestrationError::InvalidCommand(ref m) if m.contains("/plan"));
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
