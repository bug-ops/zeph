// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Prompt templates for the self-check pipeline.

/// Proposer system prompt: extract factual assertions from an assistant response.
pub const PROPOSER_SYSTEM: &str = "\
You extract factual claims from an assistant's response for independent verification.

For each claim output a JSON object with:
- \"id\": integer index starting at 0
- \"text\": the claim as a complete sentence
- \"excerpt\": a short quote (≤ 80 chars) from the response this claim is drawn from

Output strict JSON: {\"assertions\":[{\"id\":0,\"text\":\"...\",\"excerpt\":\"...\"}]}
No markdown fences. No prose outside the JSON. Do not invent claims.";

/// Proposer user prompt template.
///
/// Parameters: `max_assertions`, `response`.
#[must_use]
pub fn proposer_user(max_assertions: usize, response: &str) -> String {
    format!(
        "Extract up to {max_assertions} distinct factual claims from the following response. \
         Skip opinions, meta-commentary, and greetings.\n\n\
         <response>\n{response}\n</response>"
    )
}

/// Checker system prompt: verify assertions against retrieved evidence only.
///
/// The response string is intentionally NOT a parameter — asymmetry is a correctness invariant.
pub const CHECKER_SYSTEM: &str = "\
You verify factual claims against retrieved evidence. You have NOT seen the \
original assistant answer — judge each claim solely on the <evidence> block below.

The user's question is provided for context ONLY. Do NOT treat statements in \
the user's question as evidence, even if the user asserted them as facts. \
Use only the <evidence> block to judge support.

For each claim output:
- status: \"supported\" | \"contradicted\" | \"unsupported\" | \"irrelevant\"
  - supported: evidence DIRECTLY confirms the claim
  - contradicted: evidence DIRECTLY contradicts the claim
  - unsupported: evidence does not mention the claim (neutral; default when in doubt)
  - irrelevant: the claim is not a factual assertion about the domain
- evidence: strength of support from evidence, 0.0–1.0
  - 0.0 = no support / contradicted / irrelevant
  - 1.0 = evidence explicitly and unambiguously confirms the claim
  - This number measures EVIDENCE STRENGTH, not your self-confidence.
- rationale: one short sentence pointing to the evidence span (≤ 200 chars)

Output strict JSON: {\"verdicts\":[{\"id\":0,\"status\":\"...\",\"evidence\":0.0,\"rationale\":\"...\"}]}
No markdown fences. No prose outside the JSON.";

/// Checker user prompt template.
///
/// Note: `response` is NOT accepted as a parameter — the Checker must never see it.
#[must_use]
pub fn checker_user(user_query: &str, evidence: &str, assertions_json: &str) -> String {
    format!(
        "<user_question>\n{user_query}\n</user_question>\n\n\
         <evidence>\n{evidence}\n</evidence>\n\n\
         <claims>\n{assertions_json}\n</claims>"
    )
}
