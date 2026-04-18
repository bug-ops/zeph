// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Checker stage: verify assertions against retrieved evidence.
//!
//! # Asymmetry invariant
//!
//! The checker prompt functions do NOT accept the original response string.
//! This is a compile-time enforced correctness property: the Checker must never
//! see what the assistant said — it judges only against retrieved evidence.

use std::time::Duration;

use serde::Deserialize;
use zeph_llm::any::AnyProvider;

use super::parser::{ChatJsonError, chat_json};
use super::prompts::{CHECKER_SYSTEM, checker_user};
use super::types::{Assertion, AssertionVerdict, StageOutcome};

#[derive(Debug, Deserialize)]
struct CheckerOutput {
    verdicts: Vec<AssertionVerdict>,
}

/// Call the Checker and return verdicts plus stage diagnostics.
///
/// The `_response` parameter is intentionally absent — the function signature
/// enforces the asymmetry invariant at the type level.
pub async fn run_checker(
    provider: &AnyProvider,
    assertions: &[Assertion],
    evidence: &str,
    user_query: &str,
    per_call_timeout: Duration,
) -> (Vec<AssertionVerdict>, u64, StageOutcome, u32) {
    let assertions_json = match serde_json::to_string(assertions) {
        Ok(j) => j,
        Err(e) => {
            return (
                vec![],
                0,
                StageOutcome::LlmError {
                    msg: format!("assertion serialization failed: {e}"),
                },
                0,
            );
        }
    };

    let user = checker_user(user_query, evidence, &assertions_json);
    match chat_json::<CheckerOutput>(provider, CHECKER_SYSTEM, &user, per_call_timeout).await {
        Ok((out, tokens, attempt)) => {
            let retries = attempt.saturating_sub(1);
            (out.verdicts, tokens, StageOutcome::Ok, retries)
        }
        Err(ChatJsonError::Llm(e)) => (vec![], 0, StageOutcome::LlmError { msg: e.to_string() }, 0),
        Err(ChatJsonError::Timeout(ms)) => (vec![], 0, StageOutcome::Timeout { ms }, 0),
        Err(ChatJsonError::Parse(raw)) => (
            vec![],
            0,
            StageOutcome::ParseError { raw_truncated: raw },
            1,
        ),
    }
}
