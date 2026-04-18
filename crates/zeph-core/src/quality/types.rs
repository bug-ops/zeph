// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Data types for the MARCH self-check pipeline.

use serde::{Deserialize, Serialize};

/// A single factual claim extracted from an assistant response by the Proposer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assertion {
    /// Unique claim index within the turn (starts at 0).
    pub id: u32,
    /// Full text of the claim as extracted from the response.
    pub text: String,
    /// Short excerpt from the original response this claim is derived from.
    pub excerpt: String,
}

/// Verdict status for a single assertion returned by the Checker.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerdictStatus {
    /// Evidence directly confirms the claim.
    Supported,
    /// Evidence directly contradicts the claim.
    Contradicted,
    /// Evidence does not mention the claim (neutral; default when in doubt).
    Unsupported,
    /// The claim is not a factual assertion (e.g. small talk, meta-commentary).
    Irrelevant,
}

/// Checker verdict for a single assertion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssertionVerdict {
    /// Matches [`Assertion::id`].
    pub id: u32,
    /// Evidence strength: 0.0 = no support, 1.0 = unambiguous confirmation.
    ///
    /// This is evidence strength from the retrieved context, NOT judge self-confidence.
    pub evidence: f32,
    /// Verdict classification.
    pub status: VerdictStatus,
    /// Short rationale pointing to an evidence span (≤ 200 chars).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
}

/// Reason the pipeline was skipped for this turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SkipReason {
    /// No assistant message found in turn (tool-only turn or error bail-out).
    NoAssistantText,
    /// Trigger policy is `has_retrieval` but no retrieved context was found.
    NoRetrievedContext,
    /// Response exceeded `max_response_chars`; too large to check.
    ResponseTooLong { chars: usize },
    /// Feature is disabled via config.
    FeatureDisabled,
    /// Overall latency budget was exhausted before pipeline could run.
    BudgetExhausted,
    /// Provider was unavailable (not wired up or error at startup).
    ProviderUnavailable,
}

/// Outcome of a single pipeline stage (Proposer or Checker call).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StageOutcome {
    /// Stage completed successfully.
    Ok,
    /// Stage was skipped.
    Skipped(SkipReason),
    /// Stage timed out.
    Timeout { ms: u64 },
    /// LLM returned output that could not be parsed after retry.
    /// Raw text truncated to 4096 chars to keep debug dumps small.
    ParseError { raw_truncated: String },
    /// LLM returned an error.
    LlmError { msg: String },
}

/// Full self-check report for one turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelfCheckReport {
    /// Turn identifier (matches the agent turn counter).
    pub turn_id: u64,
    /// Assertions extracted by the Proposer.
    pub assertions: Vec<Assertion>,
    /// Verdicts from the Checker, one per assertion.
    pub verdicts: Vec<AssertionVerdict>,
    /// IDs of assertions that were flagged (contradicted or unsupported with low evidence).
    pub flagged_ids: Vec<u32>,
    /// Total wall-clock latency for the pipeline (ms).
    pub latency_ms: u64,
    /// Approximate tokens consumed by the Proposer call.
    pub proposer_tokens: u64,
    /// Approximate tokens consumed by the Checker call.
    pub checker_tokens: u64,
    /// Outcome of the Proposer stage.
    pub proposer_outcome: StageOutcome,
    /// Outcome of the Checker stage.
    pub checker_outcome: StageOutcome,
    /// Number of JSON parse retries across both stages.
    pub parse_retries: u32,
}
