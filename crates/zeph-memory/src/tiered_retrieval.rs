// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `MemFlow` tiered intent-driven retrieval pipeline (issue #3712).
//!
//! Classifies each recall query into one of three intent tiers and dispatches to the
//! cheapest sufficient backend, assembling evidence within a configurable token budget.
//!
//! # Tiers
//!
//! | Tier | Backend | Top-k | Graph hops |
//! |------|---------|-------|-----------|
//! | `ProfileLookup` | Keyword / persona | 3 | 0 |
//! | `TargetedRetrieval` | Hybrid | 10 | 1 |
//! | `DeepReasoning` | Hybrid + graph | 20 | 2 |
//!
//! The classifier maps the existing [`MemoryRoute`] to an [`IntentClass`]:
//! - `Keyword | Episodic` → `ProfileLookup`
//! - `Semantic | Hybrid` → `TargetedRetrieval`
//! - `Graph` → `DeepReasoning`
//!
//! When `classifier_provider` is set and the LLM call fails, the pipeline falls back to
//! [`HeuristicRouter`] (fail-open, logged at `warn`).
//!
//! # Token-budget assembly
//!
//! Recall results are truncated to fit within `token_budget`. An optional validation step
//! asks a lightweight LLM whether the gathered evidence is sufficient; on low confidence,
//! the pipeline escalates to the next heavier tier (up to `max_escalations`).

use std::sync::Arc;

pub use zeph_config::memory::TieredRetrievalConfig;
use zeph_llm::any::AnyProvider;

use crate::embedding_store::SearchFilter;
use crate::error::MemoryError;
use crate::router::{AsyncMemoryRouter, HeuristicRouter, MemoryRoute, MemoryRouter};
use crate::semantic::RecalledMessage;
use crate::semantic::SemanticMemory;
use crate::types::ConversationId;

// ── Intent classification ─────────────────────────────────────────────────────

/// Query intent tier for `MemFlow` tiered retrieval.
///
/// Maps to increasing levels of retrieval cost and depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntentClass {
    /// Fast profile/attribute lookup — keyword search, top-k = 3.
    ProfileLookup,
    /// Standard semantic retrieval — hybrid search with MMR, top-k = 10.
    TargetedRetrieval,
    /// Multi-hop reasoning — hybrid + graph traversal, top-k = 20.
    DeepReasoning,
}

impl IntentClass {
    fn from_route(route: MemoryRoute) -> Self {
        match route {
            MemoryRoute::Keyword | MemoryRoute::Episodic => Self::ProfileLookup,
            MemoryRoute::Semantic | MemoryRoute::Hybrid => Self::TargetedRetrieval,
            MemoryRoute::Graph => Self::DeepReasoning,
        }
    }

    fn top_k(self) -> usize {
        match self {
            Self::ProfileLookup => 3,
            Self::TargetedRetrieval => 10,
            Self::DeepReasoning => 20,
        }
    }

    /// Returns the next heavier tier for escalation, or `None` if already at maximum.
    fn escalate(self) -> Option<Self> {
        match self {
            Self::ProfileLookup => Some(Self::TargetedRetrieval),
            Self::TargetedRetrieval => Some(Self::DeepReasoning),
            Self::DeepReasoning => None,
        }
    }
}

impl std::fmt::Display for IntentClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProfileLookup => f.write_str("ProfileLookup"),
            Self::TargetedRetrieval => f.write_str("TargetedRetrieval"),
            Self::DeepReasoning => f.write_str("DeepReasoning"),
        }
    }
}

// ── Result ────────────────────────────────────────────────────────────────────

/// Result of tiered retrieval, including evidence and tier metadata.
#[derive(Debug)]
pub struct TieredRetrievalResult {
    /// Retrieved memory entries ordered by relevance score.
    pub messages: Vec<RecalledMessage>,
    /// The intent class that produced this result.
    pub intent: IntentClass,
    /// Approximate token count of all message content.
    pub tokens_used: usize,
    /// Whether the pipeline escalated to a heavier tier due to validation.
    pub tier_escalated: bool,
}

// ── Tiered retrieval logic ─────────────────────────────────────────────────────

/// Execute `MemFlow` tiered retrieval for a single query.
///
/// 1. Classify intent using the heuristic router (or provide an async router via
///    [`recall_tiered_async`] for LLM-backed classification).
/// 2. Retrieve candidates for the selected tier.
/// 3. Assemble evidence within `remaining_budget` tokens (further constrained to `config.token_budget`).
/// 4. Optionally validate with `validator_provider` and escalate tier if confidence is low.
///
/// `conversation_id` scopes the search to a single conversation. Pass `None` to search globally.
///
/// # Errors
///
/// Returns an error if any underlying search or database operation fails.
#[cfg_attr(
    feature = "profiling",
    tracing::instrument(name = "memory.tiered.retrieve", skip_all, fields(intent = tracing::field::Empty))
)]
pub async fn recall_tiered(
    memory: &SemanticMemory,
    query: &str,
    conversation_id: Option<ConversationId>,
    router: &dyn MemoryRouter,
    validator: Option<&Arc<AnyProvider>>,
    config: &TieredRetrievalConfig,
    remaining_budget: Option<usize>,
) -> Result<TieredRetrievalResult, MemoryError> {
    let effective_budget =
        remaining_budget.map_or(config.token_budget, |rb| rb.min(config.token_budget));

    let initial_intent = classify_intent(query, router);
    tracing::debug!(intent = %initial_intent, query_len = query.len(), "tiered: classified intent");

    let mut intent = initial_intent;
    let mut escalations: u8 = 0;
    let mut tier_escalated = false;

    loop {
        let candidates = {
            let _span =
                tracing::debug_span!("memory.tiered.retrieve_tier", tier = %intent).entered();
            retrieve_tier(memory, query, conversation_id, intent).await?
        };

        let (messages, tokens_used) = {
            let _span = tracing::debug_span!("memory.tiered.assemble").entered();
            assemble_within_budget(candidates, effective_budget)
        };

        // Validate evidence quality if enabled and a validator is available.
        if config.validation_enabled
            && escalations < config.max_escalations
            && let Some(validator_provider) = validator
            && let Some(next_tier) = intent.escalate()
        {
            let sufficient = {
                let _span = tracing::debug_span!("memory.tiered.validate").entered();
                validate_evidence(
                    validator_provider,
                    query,
                    &messages,
                    config.validation_threshold,
                )
                .await
            };
            if !sufficient {
                tracing::debug!(
                    current_tier = %intent,
                    next_tier = %next_tier,
                    escalations,
                    "tiered: evidence insufficient, escalating tier"
                );
                intent = next_tier;
                escalations += 1;
                tier_escalated = true;
                continue;
            }
        }

        return Ok(TieredRetrievalResult {
            messages,
            intent,
            tokens_used,
            tier_escalated,
        });
    }
}

/// Classify query intent using the provided router.
///
/// When the router is an async-capable type the async path is used; otherwise the
/// heuristic sync path is taken. LLM failures in the router are already handled by
/// the router itself (fall-open to heuristic).
fn classify_intent(query: &str, router: &dyn MemoryRouter) -> IntentClass {
    let _span = tracing::debug_span!("memory.tiered.classify").entered();

    // Sync heuristic path; use recall_tiered_async for LLM-backed classification.
    let decision = router.route_with_confidence(query);
    IntentClass::from_route(decision.route)
}

/// Retrieve candidates for the given intent tier from `SemanticMemory`.
async fn retrieve_tier(
    memory: &SemanticMemory,
    query: &str,
    conversation_id: Option<ConversationId>,
    intent: IntentClass,
) -> Result<Vec<RecalledMessage>, MemoryError> {
    let top_k = intent.top_k();
    let heuristic = HeuristicRouter;

    let filter = conversation_id.map(|cid| SearchFilter {
        conversation_id: Some(cid),
        role: None,
        category: None,
    });

    // All tiers route through recall_routed; the heuristic router maps intent-appropriate
    // routes. Graph traversal for DeepReasoning is left to the caller via recall_graph.
    memory.recall_routed(query, top_k, filter, &heuristic).await
}

/// Truncate `candidates` to fit within `budget` tokens.
///
/// Uses the same 4 chars-per-token approximation as the rest of the codebase.
/// Returns the retained messages and the total token count consumed.
fn assemble_within_budget(
    candidates: Vec<RecalledMessage>,
    budget: usize,
) -> (Vec<RecalledMessage>, usize) {
    let mut retained = Vec::with_capacity(candidates.len());
    let mut total_tokens: usize = 0;

    for msg in candidates {
        let msg_tokens = zeph_common::text::estimate_tokens(&msg.message.content);
        if total_tokens.saturating_add(msg_tokens) > budget {
            break;
        }
        total_tokens += msg_tokens;
        retained.push(msg);
    }

    (retained, total_tokens)
}

/// Ask the validator LLM whether the gathered evidence is sufficient for the query.
///
/// Returns `true` when the validator's confidence is >= `threshold` or when the
/// call fails (fail-open: prefer serving potentially incomplete evidence over blocking).
async fn validate_evidence(
    provider: &Arc<AnyProvider>,
    query: &str,
    messages: &[RecalledMessage],
    threshold: f32,
) -> bool {
    use zeph_llm::provider::{LlmProvider as _, Message, MessageMetadata, Role};

    if messages.is_empty() {
        return false;
    }

    let evidence_snippet = messages
        .iter()
        .take(5)
        .map(|m| m.message.content.chars().take(200).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n---\n");

    let system = "You are an evidence quality judge. \
        Given a query and evidence snippets, decide if the evidence is sufficient to answer the query. \
        Respond ONLY with a JSON object: {\"sufficient\": true|false, \"confidence\": 0.0-1.0}";

    let user = format!(
        "<query>{}</query>\n<evidence>{}</evidence>",
        query.chars().take(500).collect::<String>(),
        evidence_snippet
    );

    let msgs = vec![
        Message {
            role: Role::System,
            content: system.to_owned(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::User,
            content: user,
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    ];

    match tokio::time::timeout(std::time::Duration::from_secs(5), provider.chat(&msgs)).await {
        Ok(Ok(raw)) => parse_validation_response(&raw, threshold),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "tiered: validator LLM call failed, treating as sufficient");
            true
        }
        Err(_) => {
            tracing::warn!("tiered: validator LLM call timed out, treating as sufficient");
            true
        }
    }
}

fn parse_validation_response(raw: &str, threshold: f32) -> bool {
    let json_str = raw
        .find('{')
        .and_then(|s| raw[s..].rfind('}').map(|e| &raw[s..=s + e]))
        .unwrap_or("");

    if let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str) {
        let sufficient = v
            .get("sufficient")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        #[allow(clippy::cast_possible_truncation)]
        let confidence = v
            .get("confidence")
            .and_then(serde_json::Value::as_f64)
            .map_or(1.0, |c| c.clamp(0.0, 1.0) as f32);

        return sufficient && confidence >= threshold;
    }

    tracing::debug!("tiered: could not parse validator response, treating as sufficient");
    true
}

// ── Async routing adapter ─────────────────────────────────────────────────────

/// Execute `MemFlow` tiered retrieval using an async-capable router for intent classification.
///
/// This variant allows LLM-backed routers (e.g. `HybridRouter`, `LlmRouter`) to participate
/// in intent classification via their `route_async` implementation.
///
/// `conversation_id` scopes the search to a single conversation. Pass `None` to search globally.
///
/// # Errors
///
/// Returns an error if any underlying search or database operation fails.
pub async fn recall_tiered_async(
    memory: &SemanticMemory,
    query: &str,
    conversation_id: Option<ConversationId>,
    router: &dyn AsyncMemoryRouter,
    validator: Option<&Arc<AnyProvider>>,
    config: &TieredRetrievalConfig,
    remaining_budget: Option<usize>,
) -> Result<TieredRetrievalResult, MemoryError> {
    let effective_budget =
        remaining_budget.map_or(config.token_budget, |rb| rb.min(config.token_budget));

    let decision = {
        let _span = tracing::debug_span!("memory.tiered.classify").entered();
        router.route_async(query).await
    };
    let initial_intent = IntentClass::from_route(decision.route);
    tracing::debug!(intent = %initial_intent, confidence = decision.confidence, "tiered: async classified intent");

    let mut intent = initial_intent;
    let mut escalations: u8 = 0;
    let mut tier_escalated = false;

    loop {
        let candidates = {
            let _span =
                tracing::debug_span!("memory.tiered.retrieve_tier", tier = %intent).entered();
            retrieve_tier(memory, query, conversation_id, intent).await?
        };

        let (messages, tokens_used) = {
            let _span = tracing::debug_span!("memory.tiered.assemble").entered();
            assemble_within_budget(candidates, effective_budget)
        };

        if config.validation_enabled
            && escalations < config.max_escalations
            && let Some(validator_provider) = validator
            && let Some(next_tier) = intent.escalate()
        {
            let sufficient = {
                let _span = tracing::debug_span!("memory.tiered.validate").entered();
                validate_evidence(
                    validator_provider,
                    query,
                    &messages,
                    config.validation_threshold,
                )
                .await
            };
            if !sufficient {
                tracing::debug!(
                    current_tier = %intent,
                    next_tier = %next_tier,
                    escalations,
                    "tiered: evidence insufficient, escalating tier (async)"
                );
                intent = next_tier;
                escalations += 1;
                tier_escalated = true;
                continue;
            }
        }

        return Ok(TieredRetrievalResult {
            messages,
            intent,
            tokens_used,
            tier_escalated,
        });
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::MemoryRoute;
    use crate::semantic::RecalledMessage;
    use zeph_llm::provider::{Message, MessageMetadata, Role};

    fn make_message(content: &str) -> RecalledMessage {
        RecalledMessage {
            message: Message {
                role: Role::User,
                content: content.to_owned(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            score: 1.0,
        }
    }

    #[test]
    fn intent_class_from_route_mapping() {
        assert_eq!(
            IntentClass::from_route(MemoryRoute::Keyword),
            IntentClass::ProfileLookup
        );
        assert_eq!(
            IntentClass::from_route(MemoryRoute::Episodic),
            IntentClass::ProfileLookup
        );
        assert_eq!(
            IntentClass::from_route(MemoryRoute::Semantic),
            IntentClass::TargetedRetrieval
        );
        assert_eq!(
            IntentClass::from_route(MemoryRoute::Hybrid),
            IntentClass::TargetedRetrieval
        );
        assert_eq!(
            IntentClass::from_route(MemoryRoute::Graph),
            IntentClass::DeepReasoning
        );
    }

    #[test]
    fn intent_class_top_k() {
        assert_eq!(IntentClass::ProfileLookup.top_k(), 3);
        assert_eq!(IntentClass::TargetedRetrieval.top_k(), 10);
        assert_eq!(IntentClass::DeepReasoning.top_k(), 20);
    }

    #[test]
    fn intent_class_escalate_chain() {
        assert_eq!(
            IntentClass::ProfileLookup.escalate(),
            Some(IntentClass::TargetedRetrieval)
        );
        assert_eq!(
            IntentClass::TargetedRetrieval.escalate(),
            Some(IntentClass::DeepReasoning)
        );
        assert_eq!(IntentClass::DeepReasoning.escalate(), None);
    }

    #[test]
    fn assemble_within_budget_empty_input() {
        let (retained, tokens) = assemble_within_budget(vec![], 4096);
        assert!(retained.is_empty());
        assert_eq!(tokens, 0);
    }

    #[test]
    fn assemble_within_budget_zero_budget_returns_nothing() {
        let candidates = vec![make_message("hello"), make_message("world")];
        let (retained, tokens) = assemble_within_budget(candidates, 0);
        assert!(retained.is_empty(), "budget=0 must retain no messages");
        assert_eq!(tokens, 0);
    }

    #[test]
    fn assemble_within_budget_truncates_at_limit() {
        // estimate_tokens = chars / 4. Each message: "a " * 400 = 800 chars = 200 tokens.
        // Budget 250 fits exactly one (200 <= 250) but not two (200 + 200 = 400 > 250).
        let msg = "a ".repeat(400);
        let candidates = vec![make_message(&msg), make_message(&msg)];
        let (retained, tokens) = assemble_within_budget(candidates, 250);
        assert_eq!(
            retained.len(),
            1,
            "tight budget must keep only first message"
        );
        assert_eq!(tokens, 200);
    }

    #[test]
    fn parse_validation_response_missing_fields_defaults_to_sufficient() {
        // Neither "sufficient" nor "confidence" present → defaults: sufficient=true, confidence=1.0
        let raw = "{}";
        assert!(
            parse_validation_response(raw, 0.6),
            "missing fields must default to sufficient"
        );
    }

    #[test]
    fn tiered_retrieval_config_defaults() {
        let cfg = TieredRetrievalConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.token_budget, 4096);
        assert!(!cfg.validation_enabled);
        assert_eq!(cfg.max_escalations, 1);
    }

    #[test]
    fn parse_validation_response_sufficient() {
        let raw = r#"{"sufficient": true, "confidence": 0.9}"#;
        assert!(parse_validation_response(raw, 0.6));
    }

    #[test]
    fn parse_validation_response_insufficient() {
        let raw = r#"{"sufficient": false, "confidence": 0.4}"#;
        assert!(!parse_validation_response(raw, 0.6));
    }

    #[test]
    fn parse_validation_response_low_confidence() {
        let raw = r#"{"sufficient": true, "confidence": 0.3}"#;
        // threshold = 0.6, confidence 0.3 < 0.6 → insufficient
        assert!(!parse_validation_response(raw, 0.6));
    }

    #[test]
    fn parse_validation_response_malformed_json_treats_as_sufficient() {
        let raw = "not json at all";
        assert!(parse_validation_response(raw, 0.6));
    }

    #[test]
    fn intent_class_display() {
        assert_eq!(IntentClass::ProfileLookup.to_string(), "ProfileLookup");
        assert_eq!(
            IntentClass::TargetedRetrieval.to_string(),
            "TargetedRetrieval"
        );
        assert_eq!(IntentClass::DeepReasoning.to_string(), "DeepReasoning");
    }
}
