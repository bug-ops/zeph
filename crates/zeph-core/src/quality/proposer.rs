// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Proposer stage: extract factual assertions from an assistant response.

use std::time::Duration;

use serde::Deserialize;
use zeph_llm::any::AnyProvider;

use super::parser::{ChatJsonError, chat_json};
use super::prompts::{PROPOSER_SYSTEM, proposer_user};
use super::types::{Assertion, StageOutcome};

#[derive(Debug, Deserialize)]
struct ProposerOutput {
    assertions: Vec<Assertion>,
}

/// Call the Proposer and return extracted assertions plus stage diagnostics.
///
/// Clamps the result to `max_assertions`. Returns an empty vec on parse failure
/// with the outcome capturing the error detail.
pub async fn run_proposer(
    provider: &AnyProvider,
    response: &str,
    max_assertions: usize,
    per_call_timeout: Duration,
) -> (Vec<Assertion>, u64, StageOutcome, u32) {
    let user = proposer_user(max_assertions, response);
    match chat_json::<ProposerOutput>(provider, PROPOSER_SYSTEM, &user, per_call_timeout).await {
        Ok((mut out, tokens, attempt)) => {
            out.assertions.truncate(max_assertions);
            let retries = attempt.saturating_sub(1);
            (out.assertions, tokens, StageOutcome::Ok, retries)
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
