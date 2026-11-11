// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/goal check` self-check against the current conversation (spec 089).
//!
//! A single advisory [`GoalSupervisor::verify`] call that judges a goal condition against the
//! recent conversation. It reuses the autonomous supervisor (provider and timeout come from
//! `[goals] supervisor_provider` / `supervisor_timeout_secs`), never touches
//! `AutonomousSession` state and is rejected while an autonomous session is running.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tracing::Instrument as _;
use zeph_commands::CommandError;
use zeph_common::text::truncate_chars;
use zeph_llm::provider::{Message, MessagePart, Role};

use super::Agent;
use crate::channel::Channel;
use crate::goal::GoalAccounting;
use crate::goal::autonomous::SupervisorVerdict;
use crate::goal::supervisor::{GoalSupervisor, SupervisorError};
use crate::redact::redact_secrets;

const CONDITION_MAX_CHARS: usize = 500;
const RECENT_ACTIONS_MAX: usize = 10;
const ACTION_ARGS_MAX_CHARS: usize = 100;
pub(super) const STATUS_VERIFYING: &str = "Verifying goal\u{2026}";
const ACTIVE_GOAL_LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Where the condition evaluated by `/goal check` came from, in priority order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GoalConditionSource {
    /// Text given after `/goal check`.
    Explicit,
    /// The active goal from the goal store.
    ActiveGoal,
    /// The most recent user request in the conversation.
    LastUserMessage,
}

impl GoalConditionSource {
    const fn label(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::ActiveGoal => "active goal",
            Self::LastUserMessage => "last user message",
        }
    }
}

/// What started a goal check; labels the report and the span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GoalCheckTrigger {
    /// The `/goal check` command.
    Command,
    /// The end-of-turn auto-check (`[goals] auto_check`).
    TurnEnd,
}

impl GoalCheckTrigger {
    const fn header(self) -> &'static str {
        match self {
            Self::Command => "Goal check (advisory)",
            Self::TurnEnd => "Goal check (auto, advisory)",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::TurnEnd => "turn_end",
        }
    }
}

/// Result of [`GoalCheckJob::run`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum GoalCheckOutcome {
    /// No condition could be resolved, so no supervisor call was made.
    NoCondition,
    /// A formatted advisory report.
    Report(String),
}

/// How a job obtains the active goal when no explicit condition is given.
pub(super) enum ActiveGoalLookup {
    /// Do not look one up.
    Skip,
    /// The goal text is already known (auto-check reads the cached snapshot synchronously).
    Known(String),
    /// Refresh the accounting cache first, then read it (`/goal check`).
    Refresh(Arc<GoalAccounting>),
}

/// Return the condition argument when `args` is a `/goal check` invocation.
///
/// Accepts `check`, `check <condition>` and `check: <condition>`.
pub(super) fn goal_check_args(args: &str) -> Option<&str> {
    let rest = args.trim().strip_prefix("check")?;
    if !rest.is_empty() && !rest.starts_with(|c: char| c.is_whitespace() || c == ':') {
        return None;
    }
    Some(rest.trim_start_matches(':').trim())
}

/// Text of the most recent genuine user request: agent-visible, `Role::User`, with text and
/// without tool results (tool-result carriers are `Role::User` too).
fn last_user_text(messages: &[Message]) -> Option<String> {
    messages.iter().rev().find_map(|m| {
        if m.role != Role::User || !m.metadata.visibility.is_agent_visible() {
            return None;
        }
        if m.parts
            .iter()
            .any(|p| matches!(p, MessagePart::ToolResult { .. }))
        {
            return None;
        }
        let text = if m.parts.is_empty() {
            m.content.clone()
        } else {
            m.parts
                .iter()
                .filter_map(|p| match p {
                    MessagePart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let text = text.trim();
        if text.is_empty() || text.starts_with("/goal") {
            return None;
        }
        Some(text.to_owned())
    })
}

/// Last tool invocations as `name(args)`, oldest first, with secrets scrubbed from the args.
pub(super) fn recent_tool_actions(messages: &[Message]) -> Vec<String> {
    let mut actions: Vec<String> = messages
        .iter()
        .rev()
        .flat_map(|m| m.parts.iter().rev())
        .filter_map(|p| match p {
            MessagePart::ToolUse { name, input, .. } => {
                let args = input.to_string();
                let args = redact_secrets(&args);
                Some(format!(
                    "{name}({})",
                    truncate_chars(&args, ACTION_ARGS_MAX_CHARS)
                ))
            }
            _ => None,
        })
        .take(RECENT_ACTIONS_MAX)
        .collect();
    actions.reverse();
    actions
}

/// Condition text given to `/goal check`: trimmed and never empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ExplicitCondition(String);

impl ExplicitCondition {
    /// `None` when `raw` is empty or whitespace-only, i.e. no explicit condition was given.
    pub(super) fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        (!trimmed.is_empty()).then(|| Self(trimmed.to_owned()))
    }
}

fn resolve_condition(
    explicit: Option<&ExplicitCondition>,
    active_goal: Option<String>,
    last_user: Option<String>,
) -> Option<(GoalConditionSource, String)> {
    let (source, text) = match explicit {
        Some(ExplicitCondition(text)) => (GoalConditionSource::Explicit, text.clone()),
        None => active_goal
            .map(|t| (GoalConditionSource::ActiveGoal, t))
            .or_else(|| last_user.map(|t| (GoalConditionSource::LastUserMessage, t)))?,
    };
    // Scrub before truncating so a secret cut mid-token cannot escape the scrub.
    let text = redact_secrets(&text);
    Some((
        source,
        truncate_chars(&text, CONDITION_MAX_CHARS).to_owned(),
    ))
}

fn format_report(
    trigger: GoalCheckTrigger,
    source: GoalConditionSource,
    condition: &str,
    result: &Result<SupervisorVerdict, SupervisorError>,
    timeout_secs: u64,
) -> String {
    let header = format!(
        "{}\nCondition [{}]: {condition}\n",
        trigger.header(),
        source.label()
    );
    match result {
        Ok(v) => {
            let verdict = if v.achieved {
                "ACHIEVED"
            } else {
                "NOT ACHIEVED"
            };
            let mut out = format!(
                "{header}Verdict: {verdict} (confidence {:.2})\nReasoning: {}",
                v.confidence, v.reasoning
            );
            if !v.achieved && !v.suggestions.is_empty() {
                out.push_str("\nSuggestions:");
                for s in &v.suggestions {
                    out.push_str("\n- ");
                    out.push_str(s);
                }
            }
            out
        }
        Err(SupervisorError::Timeout(_)) => {
            format!("{header}Verification timed out after {timeout_secs}s.")
        }
        Err(SupervisorError::RateLimited) => {
            format!("{header}Verification is temporarily rate-limited, try again shortly.")
        }
        Err(SupervisorError::Llm(e)) => format!("{header}Verification failed: {e}"),
        Err(SupervisorError::Parse(_)) => {
            format!("{header}Verification failed: the verifier returned an unparsable response.")
        }
    }
}

pub(super) struct GoalCheckJob {
    pub(super) trigger: GoalCheckTrigger,
    pub(super) explicit: Option<ExplicitCondition>,
    pub(super) last_user: Option<String>,
    pub(super) summary: String,
    pub(super) recent_actions: Vec<String>,
    pub(super) supervisor: GoalSupervisor,
    pub(super) timeout_secs: u64,
    pub(super) active_goal: ActiveGoalLookup,
    pub(super) status_tx: Option<zeph_llm::status::StatusSender>,
}

impl GoalCheckJob {
    async fn resolve_active_goal(&self) -> Option<String> {
        match &self.active_goal {
            ActiveGoalLookup::Skip => None,
            ActiveGoalLookup::Known(text) => Some(text.clone()),
            ActiveGoalLookup::Refresh(accounting) => {
                let lookup = tokio::time::timeout(ACTIVE_GOAL_LOOKUP_TIMEOUT, accounting.refresh())
                    .instrument(tracing::info_span!("core.goal.active_lookup"))
                    .await;
                match lookup {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        tracing::debug!(error = %e, "goal check: active goal lookup failed");
                    }
                    Err(_) => tracing::debug!("goal check: active goal lookup timed out"),
                }
                accounting.snapshot().map(|s| s.text)
            }
        }
    }

    pub(super) async fn run(self) -> GoalCheckOutcome {
        let active_goal = if self.explicit.is_none() {
            self.resolve_active_goal().await
        } else {
            None
        };
        let Some((source, condition)) =
            resolve_condition(self.explicit.as_ref(), active_goal, self.last_user)
        else {
            return GoalCheckOutcome::NoCondition;
        };

        if let Some(tx) = &self.status_tx {
            let _ = tx.set(STATUS_VERIFYING);
        }
        let result = self
            .supervisor
            .verify(&condition, &self.summary, &self.recent_actions)
            .instrument(tracing::info_span!(
                "core.goal.turn_selfcheck",
                source = source.label(),
                trigger = self.trigger.label()
            ))
            .await;
        if let Some(tx) = &self.status_tx {
            let _ = tx.clear();
        }
        GoalCheckOutcome::Report(format_report(
            self.trigger,
            source,
            &condition,
            &result,
            self.timeout_secs,
        ))
    }
}

impl<C: Channel + Send + 'static> Agent<C> {
    /// Handle `/goal check [condition]`: one advisory verification of a goal condition.
    ///
    /// Does not require `[goals] enabled`; the active-goal source is used only when the goal
    /// store is available. Rejected while an autonomous goal session is running.
    pub(super) fn goal_check<'a>(
        &mut self,
        condition: &str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        if self.services.autonomous.session.is_some() {
            return Box::pin(async {
                Ok("An autonomous goal session is running; use /goal status.".to_owned())
            });
        }
        let explicit = ExplicitCondition::parse(condition);
        let active_goal = if explicit.is_none() {
            self.ensure_goal_accounting()
                .map_or(ActiveGoalLookup::Skip, ActiveGoalLookup::Refresh)
        } else {
            ActiveGoalLookup::Skip
        };
        let summary = self.build_conversation_summary();
        let job = GoalCheckJob {
            trigger: GoalCheckTrigger::Command,
            explicit,
            last_user: last_user_text(&self.msg.messages),
            summary,
            recent_actions: recent_tool_actions(&self.msg.messages),
            supervisor: self.build_supervisor(),
            timeout_secs: self.runtime.config.goals.supervisor_timeout_secs,
            active_goal,
            status_tx: self.services.session.status_tx.clone(),
        };
        Box::pin(async move {
            Ok(match job.run().await {
                GoalCheckOutcome::Report(report) => report,
                GoalCheckOutcome::NoCondition => {
                    "No goal condition to check. State one: /goal check <condition>".to_owned()
                }
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;
    use zeph_llm::provider::MessageVisibility;

    use super::*;

    fn user(text: &str) -> Message {
        Message::from_parts(Role::User, vec![MessagePart::Text { text: text.into() }])
    }

    fn tool_use(name: &str, input: serde_json::Value) -> Message {
        Message::from_parts(
            Role::Assistant,
            vec![MessagePart::ToolUse {
                id: "t1".into(),
                name: name.into(),
                input,
            }],
        )
    }

    fn tool_result() -> Message {
        Message::from_parts(
            Role::User,
            vec![
                MessagePart::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "tool output".into(),
                    is_error: false,
                },
                MessagePart::Text {
                    text: "runtime note".into(),
                },
            ],
        )
    }

    #[test]
    fn check_args_parsing() {
        assert_eq!(goal_check_args("check"), Some(""));
        assert_eq!(goal_check_args("check tests pass"), Some("tests pass"));
        assert_eq!(goal_check_args("check: tests pass"), Some("tests pass"));
        assert_eq!(goal_check_args("checkpoint"), None);
        assert_eq!(goal_check_args("status"), None);
    }

    #[test]
    fn last_user_text_skips_tool_result_carriers() {
        let msgs = vec![
            user("fix the failing test"),
            tool_use("shell", serde_json::json!({})),
            tool_result(),
        ];
        assert_eq!(
            last_user_text(&msgs).as_deref(),
            Some("fix the failing test")
        );
    }

    #[test]
    fn last_user_text_skips_invisible_and_goal_commands() {
        let mut hidden = user("hidden");
        hidden.metadata.visibility = MessageVisibility::UserOnly;
        let msgs = vec![user("real request"), user("/goal check"), hidden];
        assert_eq!(last_user_text(&msgs).as_deref(), Some("real request"));
        assert_eq!(last_user_text(&[]), None);
    }

    #[test]
    fn last_user_text_falls_back_to_content_without_parts() {
        let msgs = vec![Message::from_legacy(Role::User, "legacy text")];
        assert_eq!(last_user_text(&msgs).as_deref(), Some("legacy text"));
    }

    #[test]
    fn recent_actions_are_bounded_ordered_and_truncated() {
        let mut msgs = Vec::new();
        for i in 0..15 {
            msgs.push(tool_use("shell", serde_json::json!({ "n": i })));
        }
        let actions = recent_tool_actions(&msgs);
        assert_eq!(actions.len(), RECENT_ACTIONS_MAX);
        assert!(actions[0].contains("\"n\":5"));
        assert!(actions[9].contains("\"n\":14"));

        let long = tool_use("shell", serde_json::json!({ "cmd": "x".repeat(500) }));
        let a = recent_tool_actions(&[long]);
        assert!(a[0].len() <= "shell()".len() + ACTION_ARGS_MAX_CHARS);
    }

    #[test]
    fn recent_actions_scrub_secrets() {
        let secret = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGH";
        let msgs = vec![tool_use(
            "shell",
            serde_json::json!({ "cmd": format!("curl -H 'x: {secret}'") }),
        )];
        let a = recent_tool_actions(&msgs);
        assert!(!a[0].contains(secret), "{}", a[0]);
    }

    #[test]
    fn condition_priority_explicit_then_goal_then_last_user() {
        let explicit = ExplicitCondition::parse("do x");
        let r = resolve_condition(explicit.as_ref(), Some("g".into()), Some("u".into())).unwrap();
        assert_eq!(r.0, GoalConditionSource::Explicit);
        let r = resolve_condition(None, Some("g".into()), Some("u".into())).unwrap();
        assert_eq!((r.0, r.1.as_str()), (GoalConditionSource::ActiveGoal, "g"));
        let blank = ExplicitCondition::parse("  ");
        let r = resolve_condition(blank.as_ref(), None, Some("u".into())).unwrap();
        assert_eq!(r.0, GoalConditionSource::LastUserMessage);
        assert!(resolve_condition(None, None, None).is_none());
    }

    #[test]
    fn explicit_condition_is_trimmed_and_non_empty() {
        assert_eq!(ExplicitCondition::parse(""), None);
        assert_eq!(ExplicitCondition::parse(" \t\n"), None);
        assert_eq!(
            ExplicitCondition::parse("  ship it "),
            Some(ExplicitCondition("ship it".to_owned()))
        );
    }

    fn verdict(achieved: bool) -> SupervisorVerdict {
        SupervisorVerdict {
            achieved,
            reasoning: "because".into(),
            confidence: 0.8,
            suggestions: vec!["run tests".into()],
        }
    }

    #[test]
    fn report_shows_all_fields_and_suggestions_only_when_not_achieved() {
        let r = format_report(
            GoalCheckTrigger::Command,
            GoalConditionSource::Explicit,
            "c",
            &Ok(verdict(false)),
            30,
        );
        assert!(r.contains("NOT ACHIEVED") && r.contains("0.80") && r.contains("because"));
        assert!(r.contains("- run tests") && r.contains("[explicit]"));
        let r = format_report(
            GoalCheckTrigger::Command,
            GoalConditionSource::Explicit,
            "c",
            &Ok(verdict(true)),
            30,
        );
        assert!(r.contains("ACHIEVED") && !r.contains("Suggestions"));
    }

    #[test]
    fn report_timeout_shows_configured_value() {
        let r = format_report(
            GoalCheckTrigger::Command,
            GoalConditionSource::ActiveGoal,
            "c",
            &Err(SupervisorError::Timeout(1)),
            45,
        );
        assert!(r.contains("45s"), "{r}");
    }

    fn job(explicit: &str, last_user: Option<&str>, provider: MockProvider) -> GoalCheckJob {
        GoalCheckJob {
            trigger: GoalCheckTrigger::Command,
            explicit: ExplicitCondition::parse(explicit),
            last_user: last_user.map(Into::into),
            summary: "summary".into(),
            recent_actions: vec![],
            supervisor: GoalSupervisor::new(AnyProvider::Mock(provider), Duration::from_secs(5)),
            timeout_secs: 5,
            active_goal: ActiveGoalLookup::Skip,
            status_tx: None,
        }
    }

    async fn run_report(j: GoalCheckJob) -> String {
        match j.run().await {
            GoalCheckOutcome::Report(r) => r,
            GoalCheckOutcome::NoCondition => panic!("expected a report"),
        }
    }

    #[tokio::test]
    async fn run_without_any_condition_makes_no_llm_call() {
        let out = job("", None, MockProvider::failing()).run().await;
        assert_eq!(out, GoalCheckOutcome::NoCondition);
    }

    #[tokio::test]
    async fn run_reports_verdict_and_source() {
        let provider = MockProvider::with_responses(vec![
            r#"{"achieved":true,"reasoning":"done","confidence":0.9,"suggestions":[]}"#.into(),
        ]);
        let out = run_report(job("", Some("fix it"), provider)).await;
        assert!(
            out.contains("[last user message]") && out.contains("ACHIEVED"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn run_provider_failure_is_non_fatal_message() {
        let out = run_report(job("cond", None, MockProvider::failing())).await;
        assert!(out.contains("Verification failed"), "{out}");
    }

    #[tokio::test]
    async fn run_sends_status_and_clears_it() {
        let (tx, mut rx) = zeph_llm::status::status_channel(zeph_llm::status::StatusSource::Agent);
        let provider = MockProvider::with_responses(vec![
            r#"{"achieved":false,"reasoning":"no","confidence":0.5}"#.into(),
        ]);
        let mut j = job("cond", None, provider);
        j.status_tx = Some(tx);
        j.run().await;
        // Latest wins: Set and Clear collapse. A Clear is delivered only when the same source
        // had set a status, so receiving it proves the verifying status was shown and cleared.
        assert_eq!(rx.next().await, Some(zeph_llm::status::StatusUpdate::Clear));
    }

    fn agent_with_verdict(json: &str) -> Agent<crate::agent::agent_tests::MockChannel> {
        use crate::agent::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        Agent::new(
            mock_provider(vec![json.into()]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
    }

    #[tokio::test]
    async fn goal_check_rejected_during_autonomous_session() {
        let mut agent = agent_with_verdict("{}");
        agent.services.autonomous.start_session("g1", "goal", 5);
        let out = agent.goal_check("anything").await.unwrap();
        assert!(out.contains("/goal status"), "{out}");
        assert!(agent.services.autonomous.session.is_some());
    }

    #[test]
    fn condition_is_scrubbed_before_truncation() {
        let secret = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGH";
        let text = format!("{} {secret}", "a".repeat(CONDITION_MAX_CHARS - 10));
        let explicit = ExplicitCondition::parse(&text);
        let (_, cond) = resolve_condition(explicit.as_ref(), None, None).unwrap();
        assert!(!cond.contains("sk-ant"), "{cond}");
    }

    #[tokio::test]
    async fn goal_check_without_arg_uses_last_real_user_message() {
        let mut agent = agent_with_verdict(
            r#"{"achieved":false,"reasoning":"r","confidence":0.4,"suggestions":["s"]}"#,
        );
        agent.msg.messages.push(user("write the parser"));
        agent
            .msg
            .messages
            .push(tool_use("shell", serde_json::json!({})));
        agent.msg.messages.push(tool_result());
        let out = agent.goal_check("").await.unwrap();
        assert!(
            out.contains("[last user message]") && out.contains("write the parser"),
            "{out}"
        );
        assert!(!out.contains("runtime note"), "{out}");
    }

    #[tokio::test]
    async fn goal_check_works_with_goals_disabled() {
        let mut agent = agent_with_verdict(
            r#"{"achieved":true,"reasoning":"ok","confidence":0.7,"suggestions":[]}"#,
        );
        assert!(!agent.runtime.config.goals.enabled);
        let out = agent.goal_check("tests pass").await.unwrap();
        assert!(
            out.contains("[explicit]") && out.contains("ACHIEVED"),
            "{out}"
        );
        assert!(agent.services.autonomous.session.is_none());
    }
}
