// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::time::Duration;

use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;

use super::config::{QualityConfig, TriggerPolicy};
use super::parser::{chat_json, parse_json};
use super::pipeline::{RetrievedContext, SelfCheckPipeline};
use super::proposer::run_proposer;

fn mock_provider(responses: Vec<&str>) -> AnyProvider {
    AnyProvider::Mock(MockProvider::with_responses(
        responses.into_iter().map(String::from).collect(),
    ))
}

fn default_cfg() -> QualityConfig {
    QualityConfig::default()
}

// ─── Parser tests ────────────────────────────────────────────────────────────

#[test]
fn parse_json_strips_markdown_fences() {
    let raw = "```json\n{\"assertions\":[]}\n```";
    let v: serde_json::Value = parse_json(raw).unwrap();
    assert_eq!(v["assertions"].as_array().unwrap().len(), 0);
}

#[test]
fn parse_json_finds_brace_span_in_prose() {
    let raw = "Here is the result: {\"x\": 99}. Done.";
    let v: serde_json::Value = parse_json(raw).unwrap();
    assert_eq!(v["x"], 99);
}

#[tokio::test]
async fn chat_json_retries_once_on_parse_failure() {
    let valid_json = r#"{"assertions":[{"id":0,"text":"Paris is the capital of France.","excerpt":"Paris is the capital"}]}"#;
    let provider = mock_provider(vec!["not json at all", valid_json]);
    let result = chat_json::<serde_json::Value>(&provider, "sys", "user", Duration::from_secs(5))
        .await
        .unwrap();
    // attempt=2 means retry happened
    assert_eq!(result.2, 2);
    assert!(result.0.get("assertions").is_some());
}

#[tokio::test]
async fn chat_json_fails_after_two_parse_attempts() {
    let provider = mock_provider(vec!["bad1", "bad2"]);
    let err = chat_json::<serde_json::Value>(&provider, "sys", "user", Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("failed to parse JSON"));
}

// ─── Proposer tests ───────────────────────────────────────────────────────────

#[tokio::test]
async fn proposer_clamps_assertions_to_max() {
    // 15 assertions returned, max is 5
    let mut assertions_raw: Vec<serde_json::Value> = (0..15)
        .map(|i| {
            serde_json::json!({
                "id": i,
                "text": format!("claim {i}"),
                "excerpt": format!("ex {i}")
            })
        })
        .collect();
    let payload = serde_json::json!({ "assertions": assertions_raw }).to_string();
    let provider = mock_provider(vec![&payload]);
    let (assertions, _, _, _) =
        run_proposer(&provider, "some response", 5, Duration::from_secs(5)).await;
    assert_eq!(assertions.len(), 5);
    // Suppress unused warning on assertions_raw
    let _ = assertions_raw.pop();
}

// ─── Pipeline tests ───────────────────────────────────────────────────────────

fn make_pipeline(cfg: &QualityConfig) -> std::sync::Arc<SelfCheckPipeline> {
    let provider = mock_provider(vec![]);
    SelfCheckPipeline::build(cfg, &provider).unwrap()
}

#[tokio::test]
async fn pipeline_skips_when_no_retrieved_context_has_retrieval_trigger() {
    let mut cfg = default_cfg();
    cfg.trigger = TriggerPolicy::HasRetrieval;
    cfg.self_check = true;
    let pipeline = make_pipeline(&cfg);
    let report = pipeline
        .run("response text", RetrievedContext::default(), "query", 1)
        .await;
    assert!(matches!(
        report.proposer_outcome,
        super::types::StageOutcome::Skipped(super::types::SkipReason::NoRetrievedContext)
    ));
}

#[tokio::test]
async fn pipeline_runs_without_retrieval_when_trigger_always() {
    let proposer_resp = r#"{"assertions":[{"id":0,"text":"Sky is blue","excerpt":"sky is blue"}]}"#;
    let checker_resp = r#"{"verdicts":[{"id":0,"status":"supported","evidence":0.9,"rationale":"evidence confirms"}]}"#;
    let provider = mock_provider(vec![proposer_resp, checker_resp]);
    let mut cfg = default_cfg();
    cfg.trigger = TriggerPolicy::Always;
    let pipeline = SelfCheckPipeline::build(&cfg, &provider).unwrap();
    let report = pipeline
        .run(
            "The sky is blue.",
            RetrievedContext::default(),
            "what color?",
            1,
        )
        .await;
    assert!(
        !report.assertions.is_empty(),
        "expected at least one assertion"
    );
    assert!(matches!(
        report.proposer_outcome,
        super::types::StageOutcome::Ok
    ));
}

#[tokio::test]
async fn pipeline_respects_outer_budget() {
    // Provider with 5000ms delay; budget is 300ms
    let mut provider = MockProvider::default();
    provider.delay_ms = 5_000;
    provider.default_response = r#"{"assertions":[]}"#.into();
    let provider = AnyProvider::Mock(provider);
    let mut cfg = default_cfg();
    cfg.trigger = TriggerPolicy::Always;
    cfg.latency_budget_ms = 300;
    cfg.per_call_timeout_ms = 150; // 150 * 2 = 300 = budget
    let pipeline = SelfCheckPipeline::build(&cfg, &provider).unwrap();

    let start = std::time::Instant::now();
    let report = pipeline
        .run("response", RetrievedContext::default(), "query", 1)
        .await;
    let elapsed = start.elapsed();

    // Should timeout well within 700ms total (budget + a bit of slack)
    assert!(
        elapsed.as_millis() < 700,
        "expected timeout < 700ms, got {}ms",
        elapsed.as_millis()
    );
    assert!(matches!(
        report.proposer_outcome,
        super::types::StageOutcome::Timeout { .. }
    ));
}

#[tokio::test]
async fn irrelevant_verdicts_not_flagged_by_low_evidence() {
    let proposer_resp =
        r#"{"assertions":[{"id":0,"text":"How are you?","excerpt":"how are you"}]}"#;
    let checker_resp = r#"{"verdicts":[{"id":0,"status":"irrelevant","evidence":0.0}]}"#;
    let provider = mock_provider(vec![proposer_resp, checker_resp]);
    let mut cfg = default_cfg();
    cfg.trigger = TriggerPolicy::Always;
    let pipeline = SelfCheckPipeline::build(&cfg, &provider).unwrap();
    let report = pipeline
        .run("text", RetrievedContext::default(), "q", 1)
        .await;
    assert!(
        report.flagged_ids.is_empty(),
        "irrelevant verdicts must not be flagged"
    );
}

#[tokio::test]
async fn flagged_when_contradicted_regardless_of_evidence() {
    let proposer_resp =
        r#"{"assertions":[{"id":0,"text":"Sky is green","excerpt":"sky is green"}]}"#;
    // High evidence (0.9) but status is contradicted
    let checker_resp = r#"{"verdicts":[{"id":0,"status":"contradicted","evidence":0.9,"rationale":"evidence shows blue"}]}"#;
    let provider = mock_provider(vec![proposer_resp, checker_resp]);
    let mut cfg = default_cfg();
    cfg.trigger = TriggerPolicy::Always;
    let pipeline = SelfCheckPipeline::build(&cfg, &provider).unwrap();
    let report = pipeline
        .run("text", RetrievedContext::default(), "q", 1)
        .await;
    assert_eq!(
        report.flagged_ids,
        vec![0],
        "contradicted assertions must always be flagged"
    );
}

// ─── Checker asymmetry test ───────────────────────────────────────────────────

#[test]
fn checker_prompt_does_not_accept_response_string() {
    // This test documents the compile-time invariant:
    // `checker_user(user_query, evidence, assertions)` has no `response: &str` parameter.
    // The function signature in checker.rs enforces this asymmetry property.
    use super::prompts::checker_user;
    let prompt = checker_user("what is x?", "evidence text", "[]");
    assert!(prompt.contains("<evidence>"));
    assert!(!prompt.contains("original assistant answer")); // not leaked
}

// ─── Flagging rule tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn unsupported_with_low_evidence_is_flagged() {
    let proposer_resp =
        r#"{"assertions":[{"id":0,"text":"Water is wet","excerpt":"water is wet"}]}"#;
    // evidence = 0.3 < default min_evidence 0.6
    let checker_resp = r#"{"verdicts":[{"id":0,"status":"unsupported","evidence":0.3}]}"#;
    let provider = mock_provider(vec![proposer_resp, checker_resp]);
    let mut cfg = default_cfg();
    cfg.trigger = TriggerPolicy::Always;
    let pipeline = SelfCheckPipeline::build(&cfg, &provider).unwrap();
    let report = pipeline
        .run("text", RetrievedContext::default(), "q", 1)
        .await;
    assert_eq!(report.flagged_ids, vec![0]);
}

#[tokio::test]
async fn supported_with_high_evidence_not_flagged() {
    let proposer_resp = r#"{"assertions":[{"id":0,"text":"Paris is capital","excerpt":"Paris"}]}"#;
    let checker_resp = r#"{"verdicts":[{"id":0,"status":"supported","evidence":0.9}]}"#;
    let provider = mock_provider(vec![proposer_resp, checker_resp]);
    let mut cfg = default_cfg();
    cfg.trigger = TriggerPolicy::Always;
    let pipeline = SelfCheckPipeline::build(&cfg, &provider).unwrap();
    let report = pipeline
        .run("text", RetrievedContext::default(), "q", 1)
        .await;
    assert!(report.flagged_ids.is_empty());
}

// ─── Config validation tests ──────────────────────────────────────────────────

#[test]
fn config_validation_rejects_bad_timeout_ratio() {
    let mut cfg = default_cfg();
    cfg.latency_budget_ms = 1_000;
    cfg.per_call_timeout_ms = 600; // 600 * 2 = 1200 > 1000
    assert!(cfg.validate().is_err());
}

#[test]
fn config_validation_accepts_valid_config() {
    assert!(default_cfg().validate().is_ok());
}

// ─── RetrievedContext tests ───────────────────────────────────────────────────

#[test]
fn retrieved_context_is_empty_by_default() {
    assert!(RetrievedContext::default().is_empty());
}

#[test]
fn retrieved_context_joined_concatenates_all_fields() {
    let ctx = RetrievedContext {
        recall: vec!["recall text"],
        graph_facts: vec!["fact"],
        ..RetrievedContext::default()
    };
    let joined = ctx.joined(" | ");
    assert!(joined.contains("recall text"));
    assert!(joined.contains("fact"));
}
