// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Supervisor verifier for autonomous goal sessions.
//!
//! [`GoalSupervisor`] makes a single LLM call with structured JSON output to decide whether
//! the goal condition has been satisfied. It is NOT a full subagent — no tools, no memory
//! access. Trust model: heuristic verifier only, not a security boundary.

use std::time::Duration;

use serde::Deserialize;
use zeph_llm::any::AnyProvider;

use super::autonomous::SupervisorVerdict;
use crate::quality::parser::{ChatJsonError, chat_json};

/// Errors returned by the supervisor.
#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    /// LLM provider returned an error.
    #[error("supervisor LLM error: {0}")]
    Llm(String),
    /// Verification call timed out.
    #[error("supervisor timed out after {0}ms")]
    Timeout(u64),
    /// LLM output could not be parsed as a verdict.
    #[error("supervisor response was not valid JSON: {0}")]
    Parse(String),
    /// Rate-limited (HTTP 429).
    #[error("supervisor rate-limited (429)")]
    RateLimited,
}

impl From<ChatJsonError> for SupervisorError {
    fn from(e: ChatJsonError) -> Self {
        match e {
            ChatJsonError::Llm(inner) => {
                let msg = inner.to_string();
                if msg.contains("429") {
                    Self::RateLimited
                } else {
                    Self::Llm(msg)
                }
            }
            ChatJsonError::Timeout(ms) => Self::Timeout(ms),
            ChatJsonError::Parse(raw) => Self::Parse(raw),
        }
    }
}

/// Internal deserialization target for the supervisor's JSON response.
#[derive(Deserialize)]
struct RawVerdict {
    achieved: bool,
    reasoning: String,
    #[serde(default)]
    confidence: f32,
    #[serde(default)]
    suggestions: Vec<String>,
}

const SUPERVISOR_SYSTEM: &str = "\
You are an autonomous goal verification assistant. \
Your task is to determine whether a stated goal condition has been achieved based on the \
agent's conversation summary and its recent actions.\n\
\n\
Respond with strict JSON only — no prose, no markdown fences:\n\
{\n\
  \"achieved\": <bool>,\n\
  \"reasoning\": \"<one or two sentence explanation>\",\n\
  \"confidence\": <float 0.0..1.0>,\n\
  \"suggestions\": [\"<optional improvement suggestion>\", ...]\n\
}\n\
Be conservative: only set achieved=true when the evidence clearly and completely satisfies \
the goal condition.";

fn supervisor_user(
    goal_condition: &str,
    conversation_summary: &str,
    recent_actions: &[String],
) -> String {
    let actions = if recent_actions.is_empty() {
        "(none)".to_owned()
    } else {
        recent_actions
            .iter()
            .map(|a| format!("- {a}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "Goal condition:\n{goal_condition}\n\n\
         Conversation summary:\n{conversation_summary}\n\n\
         Recent actions:\n{actions}"
    )
}

/// Single-call supervisor verifier for autonomous goal sessions.
///
/// Uses a configurable LLM provider (ideally different from the main agent provider to avoid
/// self-confirmation bias). On HTTP 429 the caller should wait and retry once before counting
/// the failure toward the consecutive failure limit.
pub struct GoalSupervisor {
    provider: AnyProvider,
    timeout: Duration,
}

impl GoalSupervisor {
    /// Create a new supervisor with the given provider and per-call timeout.
    #[must_use]
    pub fn new(provider: AnyProvider, timeout: Duration) -> Self {
        Self { provider, timeout }
    }

    /// Verify whether `goal_condition` has been achieved.
    ///
    /// Makes at most **two** LLM calls (initial + one retry on JSON parse failure via
    /// [`chat_json`]). Rate-limit (429) errors are surfaced as [`SupervisorError::RateLimited`]
    /// so the caller can apply backoff before counting the failure.
    ///
    /// # Errors
    ///
    /// Returns [`SupervisorError`] on provider error, timeout, or unrecoverable parse failure.
    #[tracing::instrument(name = "goal.supervisor.verify", skip_all, level = "debug", err)]
    pub async fn verify(
        &self,
        goal_condition: &str,
        conversation_summary: &str,
        recent_actions: &[String],
    ) -> Result<SupervisorVerdict, SupervisorError> {
        let user = supervisor_user(goal_condition, conversation_summary, recent_actions);
        tracing::debug!("goal.supervisor.verify: calling provider");
        let (raw, _tokens, _attempt): (RawVerdict, _, _) =
            chat_json(&self.provider, SUPERVISOR_SYSTEM, &user, self.timeout)
                .await
                .map_err(SupervisorError::from)?;
        tracing::debug!(
            achieved = raw.achieved,
            confidence = raw.confidence,
            "goal.supervisor.verify: done"
        );
        Ok(SupervisorVerdict {
            achieved: raw.achieved,
            reasoning: raw.reasoning,
            confidence: raw.confidence.clamp(0.0, 1.0),
            suggestions: raw.suggestions,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quality::parser::ChatJsonError;
    use zeph_llm::LlmError;

    #[test]
    fn supervisor_user_contains_all_sections() {
        let msg = supervisor_user(
            "the build must pass",
            "agent ran cargo build",
            &[
                "ran cargo build".to_owned(),
                "no errors reported".to_owned(),
            ],
        );
        assert!(msg.contains("Goal condition:"), "goal section missing");
        assert!(msg.contains("the build must pass"), "goal text missing");
        assert!(
            msg.contains("Conversation summary:"),
            "summary section missing"
        );
        assert!(
            msg.contains("agent ran cargo build"),
            "summary text missing"
        );
        assert!(msg.contains("Recent actions:"), "actions section missing");
        assert!(msg.contains("- ran cargo build"), "first action missing");
        assert!(
            msg.contains("- no errors reported"),
            "second action missing"
        );
    }

    #[test]
    fn supervisor_user_empty_actions_shows_none() {
        let msg = supervisor_user("goal", "summary", &[]);
        assert!(msg.contains("(none)"), "empty actions should show (none)");
    }

    #[test]
    fn supervisor_error_from_chat_json_error_llm_preserved() {
        let llm_err = ChatJsonError::Llm(LlmError::Other("backend failure".into()));
        let sup_err = SupervisorError::from(llm_err);
        assert!(
            matches!(sup_err, SupervisorError::Llm(ref msg) if msg.contains("backend failure")),
            "Llm variant must preserve the error message"
        );
    }

    #[test]
    fn supervisor_error_from_chat_json_error_llm_429_becomes_rate_limited() {
        let llm_err = ChatJsonError::Llm(LlmError::Other("HTTP 429 rate limit".into()));
        let sup_err = SupervisorError::from(llm_err);
        assert!(
            matches!(sup_err, SupervisorError::RateLimited),
            "429 in message must become RateLimited"
        );
    }

    #[test]
    fn supervisor_error_from_chat_json_error_timeout() {
        let err = SupervisorError::from(ChatJsonError::Timeout(5000));
        assert!(
            matches!(err, SupervisorError::Timeout(5000)),
            "Timeout variant must preserve milliseconds"
        );
    }

    #[test]
    fn supervisor_error_from_chat_json_error_parse() {
        let err = SupervisorError::from(ChatJsonError::Parse("bad json".to_owned()));
        assert!(
            matches!(err, SupervisorError::Parse(ref s) if s == "bad json"),
            "Parse variant must preserve the raw string"
        );
    }
}
