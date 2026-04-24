// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! LLM-based retrieval strategy classifier for hybrid graph recall.
//!
//! [`classify_retrieval_strategy`] sends a one-shot prompt to the configured LLM
//! provider and returns the name of the selected strategy. On any error the
//! function silently returns `"synapse"` — it never propagates LLM failures to the caller.

use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{LlmProvider as _, Message, Role};

const CLASSIFY_PROMPT: &str = r#"Classify this query into one retrieval strategy. Reply with exactly one word.

Strategies:
- astar: factual lookups ("who is X", "what does X do", "find X")
- watercircles: exploratory ("tell me about X", "what relates to X", "overview of X")
- beam_search: multi-hop reasoning ("how does X connect to Y", "path from X to Z")
- synapse: default/unclear

Query: "#;

/// Classify a query's intent to select the best graph retrieval strategy.
///
/// Returns one of: `"astar"`, `"watercircles"`, `"beam_search"`, or `"synapse"`.
/// On any LLM error or unrecognised response, returns `"synapse"` as a safe fallback.
/// This function never propagates errors.
///
/// # Examples
///
/// ```no_run
/// # use zeph_memory::graph::strategy_classifier::classify_retrieval_strategy;
/// # use zeph_llm::any::AnyProvider;
/// # use zeph_llm::mock::MockProvider;
/// # async fn demo() {
/// let provider = AnyProvider::Mock(MockProvider::default());
/// let strategy = classify_retrieval_strategy(&provider, "who is Alice?").await;
/// assert!(["astar", "watercircles", "beam_search", "synapse"].contains(&strategy.as_str()));
/// # }
/// ```
pub async fn classify_retrieval_strategy(provider: &AnyProvider, query: &str) -> String {
    let _span = tracing::info_span!("memory.graph.classify_strategy").entered();

    let prompt = format!("{CLASSIFY_PROMPT}{query}");
    let messages = [Message {
        role: Role::User,
        content: prompt,
        ..Default::default()
    }];

    let response = match provider.chat(&messages).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "strategy classifier: LLM error, falling back to synapse"
            );
            return "synapse".to_owned();
        }
    };

    let word = response.trim().to_lowercase();
    match word.as_str() {
        "astar" | "watercircles" | "beam_search" | "synapse" => word,
        _ => "synapse".to_owned(),
    }
}
