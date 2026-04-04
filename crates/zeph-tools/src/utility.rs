// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Utility-guided tool dispatch gate (arXiv:2603.19896).
//!
//! Computes a scalar utility score for each candidate tool call before execution.
//! Calls below the configured threshold are skipped (fail-closed on scoring errors).

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};

use crate::config::UtilityScoringConfig;
use crate::executor::ToolCall;

/// Estimated gain for known tool categories.
///
/// Keys are exact tool name prefixes or names. Higher value = more expected gain.
/// Unknown tools default to 0.5 (neutral).
fn default_gain(tool_name: &str) -> f32 {
    if tool_name.starts_with("memory") {
        return 0.8;
    }
    if tool_name.starts_with("mcp_") {
        return 0.5;
    }
    match tool_name {
        "bash" | "shell" => 0.6,
        "read" | "write" => 0.55,
        "search_code" | "grep" | "glob" => 0.65,
        _ => 0.5,
    }
}

/// Computed utility components for a candidate tool call.
#[derive(Debug, Clone)]
pub struct UtilityScore {
    /// Estimated information gain from executing the tool.
    pub gain: f32,
    /// Normalized token cost: `tokens_consumed / token_budget`.
    pub cost: f32,
    /// Redundancy penalty: 1.0 if identical `(tool_name, params_hash)` was seen this turn.
    pub redundancy: f32,
    /// Exploration bonus: decreases as turn progresses (`1 - tool_calls_this_turn / max_calls`).
    pub uncertainty: f32,
    /// Weighted aggregate.
    pub total: f32,
}

impl UtilityScore {
    /// Returns `true` when the score components are all finite.
    fn is_valid(&self) -> bool {
        self.gain.is_finite()
            && self.cost.is_finite()
            && self.redundancy.is_finite()
            && self.uncertainty.is_finite()
            && self.total.is_finite()
    }
}

/// Context required to compute utility — provided by the agent loop.
#[derive(Debug, Clone)]
pub struct UtilityContext {
    /// Number of tool calls already dispatched in the current LLM turn.
    pub tool_calls_this_turn: usize,
    /// Tokens consumed so far in this turn.
    pub tokens_consumed: usize,
    /// Token budget for the current turn. 0 = budget unknown (cost component treated as 0).
    pub token_budget: usize,
    /// True only when the tool was explicitly invoked via a `/tool` slash command.
    /// Must NOT be set based on tool names found inside user message text or tool outputs.
    pub user_requested: bool,
}

/// Recommended action from the utility policy (arXiv:2603.19896, §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UtilityAction {
    /// Generate a text response without executing the proposed tool.
    Respond,
    /// Retrieve additional context (memory search, RAG, graph recall) before responding.
    Retrieve,
    /// Execute the proposed tool call.
    ToolCall,
    /// Verify the previous tool result before proceeding.
    Verify,
    /// Stop the tool loop entirely (budget exhausted or loop limit).
    Stop,
}

/// Hashes `(tool_name, serialized_params)` pre-execution for redundancy detection.
fn call_hash(call: &ToolCall) -> u64 {
    let mut h = DefaultHasher::new();
    call.tool_id.hash(&mut h);
    // Stable iteration order is not guaranteed for serde_json::Map, but it is insertion-order
    // in practice for the same LLM output. Using the debug representation is simple and
    // deterministic within a session (no cross-session persistence of these hashes).
    format!("{:?}", call.params).hash(&mut h);
    h.finish()
}

/// Computes utility scores for tool calls before dispatch.
///
/// Not `Send + Sync` — lives on the agent's single-threaded tool loop (same lifecycle as
/// `ToolResultCache` and `recent_tool_calls`).
#[derive(Debug)]
pub struct UtilityScorer {
    config: UtilityScoringConfig,
    /// Hashes of `(tool_name, params)` seen in the current LLM turn for redundancy detection.
    recent_calls: HashMap<u64, u32>,
}

impl UtilityScorer {
    /// Create a new scorer from the given config.
    #[must_use]
    pub fn new(config: UtilityScoringConfig) -> Self {
        Self {
            config,
            recent_calls: HashMap::new(),
        }
    }

    /// Whether utility scoring is enabled.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Score a candidate tool call.
    ///
    /// Returns `None` when scoring is disabled. When scoring produces a non-finite
    /// result (misconfigured weights), returns `None` — the caller treats `None` as
    /// fail-closed (skip the tool call) unless `user_requested` is set.
    #[must_use]
    pub fn score(&self, call: &ToolCall, ctx: &UtilityContext) -> Option<UtilityScore> {
        if !self.config.enabled {
            return None;
        }

        let gain = default_gain(&call.tool_id);

        let cost = if ctx.token_budget > 0 {
            #[allow(clippy::cast_precision_loss)]
            (ctx.tokens_consumed as f32 / ctx.token_budget as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };

        let hash = call_hash(call);
        let redundancy = if self.recent_calls.contains_key(&hash) {
            1.0_f32
        } else {
            0.0_f32
        };

        // Uncertainty decreases as turn progresses. At tool call 0 it equals 1.0;
        // at tool_calls_this_turn >= 10 it saturates to 0.0.
        #[allow(clippy::cast_precision_loss)]
        let uncertainty = (1.0_f32 - ctx.tool_calls_this_turn as f32 / 10.0).clamp(0.0, 1.0);

        let total = self.config.gain_weight * gain
            - self.config.cost_weight * cost
            - self.config.redundancy_weight * redundancy
            + self.config.uncertainty_bonus * uncertainty;

        let score = UtilityScore {
            gain,
            cost,
            redundancy,
            uncertainty,
            total,
        };

        if score.is_valid() { Some(score) } else { None }
    }

    /// Recommend an action based on the utility score and turn context.
    ///
    /// Decision tree (thresholds from arXiv:2603.19896):
    /// 1. `user_requested` → always `ToolCall` (bypass policy).
    /// 2. Scoring disabled → always `ToolCall`.
    /// 3. `score` is `None` (invalid score, scoring enabled) → `Stop` (fail-closed).
    /// 4. `cost > 0.9` (budget nearly exhausted) → `Stop`.
    /// 5. `redundancy == 1.0` (duplicate call) → `Respond`.
    /// 6. `gain >= 0.7 && total >= threshold` → `ToolCall`.
    /// 7. `gain >= 0.5 && uncertainty > 0.5` → `Retrieve`.
    /// 8. `total < threshold && tool_calls_this_turn > 0` → `Verify`.
    /// 9. `total >= threshold` → `ToolCall`.
    /// 10. Default → `Respond`.
    #[must_use]
    pub fn recommend_action(
        &self,
        score: Option<&UtilityScore>,
        ctx: &UtilityContext,
    ) -> UtilityAction {
        // Bypass: user-requested tools are never gated.
        if ctx.user_requested {
            return UtilityAction::ToolCall;
        }
        // Pass-through: scoring disabled → always execute.
        if !self.config.enabled {
            return UtilityAction::ToolCall;
        }
        let Some(s) = score else {
            // Invalid score with scoring enabled → fail-closed.
            return UtilityAction::Stop;
        };

        // Budget nearly exhausted.
        if s.cost > 0.9 {
            return UtilityAction::Stop;
        }
        // Duplicate call — skip tool.
        if s.redundancy >= 1.0 {
            return UtilityAction::Respond;
        }
        // High-gain tool call above threshold.
        if s.gain >= 0.7 && s.total >= self.config.threshold {
            return UtilityAction::ToolCall;
        }
        // Uncertain — gather more context first.
        if s.gain >= 0.5 && s.uncertainty > 0.5 {
            return UtilityAction::Retrieve;
        }
        // Below threshold but prior results exist — verify before proceeding.
        if s.total < self.config.threshold && ctx.tool_calls_this_turn > 0 {
            return UtilityAction::Verify;
        }
        // Above threshold (low-gain but low-cost / low-redundancy).
        if s.total >= self.config.threshold {
            return UtilityAction::ToolCall;
        }
        UtilityAction::Respond
    }

    /// Record a call as executed for redundancy tracking.
    ///
    /// Must be called after `score()` and before the next call to `score()` for the
    /// same tool in the same turn.
    pub fn record_call(&mut self, call: &ToolCall) {
        let hash = call_hash(call);
        *self.recent_calls.entry(hash).or_insert(0) += 1;
    }

    /// Reset per-turn state. Call at the start of each LLM tool round.
    pub fn clear(&mut self) {
        self.recent_calls.clear();
    }

    /// The configured threshold.
    #[must_use]
    pub fn threshold(&self) -> f32 {
        self.config.threshold
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_call(name: &str, params: serde_json::Value) -> ToolCall {
        ToolCall {
            tool_id: name.to_owned(),
            params: if let serde_json::Value::Object(m) = params {
                m
            } else {
                serde_json::Map::new()
            },
        }
    }

    fn default_ctx() -> UtilityContext {
        UtilityContext {
            tool_calls_this_turn: 0,
            tokens_consumed: 0,
            token_budget: 1000,
            user_requested: false,
        }
    }

    fn default_config() -> UtilityScoringConfig {
        UtilityScoringConfig {
            enabled: true,
            ..UtilityScoringConfig::default()
        }
    }

    #[test]
    fn disabled_returns_none() {
        let scorer = UtilityScorer::new(UtilityScoringConfig::default());
        assert!(!scorer.is_enabled());
        let call = make_call("bash", json!({}));
        let score = scorer.score(&call, &default_ctx());
        assert!(score.is_none());
        // When disabled, recommend_action always returns ToolCall (never gated).
        assert_eq!(
            scorer.recommend_action(score.as_ref(), &default_ctx()),
            UtilityAction::ToolCall
        );
    }

    #[test]
    fn first_call_passes_default_threshold() {
        let scorer = UtilityScorer::new(default_config());
        let call = make_call("bash", json!({"cmd": "ls"}));
        let score = scorer.score(&call, &default_ctx());
        assert!(score.is_some());
        let s = score.unwrap();
        assert!(
            s.total >= 0.1,
            "first call should exceed threshold: {}",
            s.total
        );
        // First call with high uncertainty may trigger Retrieve (gather context) — that is also
        // a non-blocking outcome. Only Stop/Respond are considered failures here.
        let action = scorer.recommend_action(Some(&s), &default_ctx());
        assert!(
            action == UtilityAction::ToolCall || action == UtilityAction::Retrieve,
            "first call should not be blocked, got {action:?}",
        );
    }

    #[test]
    fn redundant_call_penalized() {
        let mut scorer = UtilityScorer::new(default_config());
        let call = make_call("bash", json!({"cmd": "ls"}));
        scorer.record_call(&call);
        let score = scorer.score(&call, &default_ctx()).unwrap();
        assert!((score.redundancy - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn clear_resets_redundancy() {
        let mut scorer = UtilityScorer::new(default_config());
        let call = make_call("bash", json!({"cmd": "ls"}));
        scorer.record_call(&call);
        scorer.clear();
        let score = scorer.score(&call, &default_ctx()).unwrap();
        assert!(score.redundancy.abs() < f32::EPSILON);
    }

    #[test]
    fn user_requested_always_executes() {
        let scorer = UtilityScorer::new(default_config());
        // Simulate a call that would score very low.
        let score = UtilityScore {
            gain: 0.0,
            cost: 1.0,
            redundancy: 1.0,
            uncertainty: 0.0,
            total: -100.0,
        };
        let ctx = UtilityContext {
            user_requested: true,
            ..default_ctx()
        };
        assert_eq!(
            scorer.recommend_action(Some(&score), &ctx),
            UtilityAction::ToolCall
        );
    }

    #[test]
    fn none_score_fail_closed_when_enabled() {
        let scorer = UtilityScorer::new(default_config());
        // Scoring failure (None with scoring enabled) → Stop (fail-closed).
        assert_eq!(
            scorer.recommend_action(None, &default_ctx()),
            UtilityAction::Stop
        );
    }

    #[test]
    fn none_score_executes_when_disabled() {
        let scorer = UtilityScorer::new(UtilityScoringConfig::default()); // disabled
        assert_eq!(
            scorer.recommend_action(None, &default_ctx()),
            UtilityAction::ToolCall
        );
    }

    #[test]
    fn cost_increases_with_token_consumption() {
        let scorer = UtilityScorer::new(default_config());
        let call = make_call("bash", json!({}));
        let ctx_low = UtilityContext {
            tokens_consumed: 100,
            token_budget: 1000,
            ..default_ctx()
        };
        let ctx_high = UtilityContext {
            tokens_consumed: 900,
            token_budget: 1000,
            ..default_ctx()
        };
        let s_low = scorer.score(&call, &ctx_low).unwrap();
        let s_high = scorer.score(&call, &ctx_high).unwrap();
        assert!(s_low.cost < s_high.cost);
        assert!(s_low.total > s_high.total);
    }

    #[test]
    fn uncertainty_decreases_with_call_count() {
        let scorer = UtilityScorer::new(default_config());
        let call = make_call("bash", json!({}));
        let ctx_early = UtilityContext {
            tool_calls_this_turn: 0,
            ..default_ctx()
        };
        let ctx_late = UtilityContext {
            tool_calls_this_turn: 9,
            ..default_ctx()
        };
        let s_early = scorer.score(&call, &ctx_early).unwrap();
        let s_late = scorer.score(&call, &ctx_late).unwrap();
        assert!(s_early.uncertainty > s_late.uncertainty);
    }

    #[test]
    fn memory_tool_has_higher_gain_than_scrape() {
        let scorer = UtilityScorer::new(default_config());
        let mem_call = make_call("memory_search", json!({}));
        let web_call = make_call("scrape", json!({}));
        let s_mem = scorer.score(&mem_call, &default_ctx()).unwrap();
        let s_web = scorer.score(&web_call, &default_ctx()).unwrap();
        assert!(s_mem.gain > s_web.gain);
    }

    #[test]
    fn zero_token_budget_zeroes_cost() {
        let scorer = UtilityScorer::new(default_config());
        let call = make_call("bash", json!({}));
        let ctx = UtilityContext {
            tokens_consumed: 500,
            token_budget: 0,
            ..default_ctx()
        };
        let s = scorer.score(&call, &ctx).unwrap();
        assert!(s.cost.abs() < f32::EPSILON);
    }

    #[test]
    fn validate_rejects_negative_weights() {
        let cfg = UtilityScoringConfig {
            enabled: true,
            gain_weight: -1.0,
            ..UtilityScoringConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_nan_weights() {
        let cfg = UtilityScoringConfig {
            enabled: true,
            threshold: f32::NAN,
            ..UtilityScoringConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_accepts_default() {
        assert!(UtilityScoringConfig::default().validate().is_ok());
    }

    #[test]
    fn threshold_zero_all_calls_pass() {
        // threshold=0.0: every call with a non-negative total should execute.
        let scorer = UtilityScorer::new(UtilityScoringConfig {
            enabled: true,
            threshold: 0.0,
            ..UtilityScoringConfig::default()
        });
        let call = make_call("bash", json!({}));
        let score = scorer.score(&call, &default_ctx()).unwrap();
        // total must be >= 0.0 for a fresh call with default weights.
        assert!(
            score.total >= 0.0,
            "total should be non-negative: {}",
            score.total
        );
        // With threshold=0 any non-blocking action (ToolCall or Retrieve) is acceptable.
        let action = scorer.recommend_action(Some(&score), &default_ctx());
        assert!(
            action == UtilityAction::ToolCall || action == UtilityAction::Retrieve,
            "threshold=0 should not block calls, got {action:?}",
        );
    }

    #[test]
    fn threshold_one_blocks_all_calls() {
        // threshold=1.0: realistic scores never reach 1.0, so every call is blocked.
        let scorer = UtilityScorer::new(UtilityScoringConfig {
            enabled: true,
            threshold: 1.0,
            ..UtilityScoringConfig::default()
        });
        let call = make_call("bash", json!({}));
        let score = scorer.score(&call, &default_ctx()).unwrap();
        assert!(
            score.total < 1.0,
            "realistic score should be below 1.0: {}",
            score.total
        );
        // Below threshold, no prior calls → Respond.
        assert_ne!(
            scorer.recommend_action(Some(&score), &default_ctx()),
            UtilityAction::ToolCall
        );
    }

    // ── recommend_action tests ────────────────────────────────────────────────

    #[test]
    fn recommend_action_user_requested_always_tool_call() {
        let scorer = UtilityScorer::new(default_config());
        let score = UtilityScore {
            gain: 0.0,
            cost: 1.0,
            redundancy: 1.0,
            uncertainty: 0.0,
            total: -100.0,
        };
        let ctx = UtilityContext {
            user_requested: true,
            ..default_ctx()
        };
        assert_eq!(
            scorer.recommend_action(Some(&score), &ctx),
            UtilityAction::ToolCall
        );
    }

    #[test]
    fn recommend_action_disabled_scorer_always_tool_call() {
        let scorer = UtilityScorer::new(UtilityScoringConfig::default()); // disabled
        let ctx = default_ctx();
        assert_eq!(scorer.recommend_action(None, &ctx), UtilityAction::ToolCall);
    }

    #[test]
    fn recommend_action_none_score_enabled_stops() {
        let scorer = UtilityScorer::new(default_config());
        let ctx = default_ctx();
        assert_eq!(scorer.recommend_action(None, &ctx), UtilityAction::Stop);
    }

    #[test]
    fn recommend_action_budget_exhausted_stops() {
        let scorer = UtilityScorer::new(default_config());
        let score = UtilityScore {
            gain: 0.8,
            cost: 0.95,
            redundancy: 0.0,
            uncertainty: 0.5,
            total: 0.5,
        };
        assert_eq!(
            scorer.recommend_action(Some(&score), &default_ctx()),
            UtilityAction::Stop
        );
    }

    #[test]
    fn recommend_action_redundant_responds() {
        let scorer = UtilityScorer::new(default_config());
        let score = UtilityScore {
            gain: 0.8,
            cost: 0.1,
            redundancy: 1.0,
            uncertainty: 0.5,
            total: 0.5,
        };
        assert_eq!(
            scorer.recommend_action(Some(&score), &default_ctx()),
            UtilityAction::Respond
        );
    }

    #[test]
    fn recommend_action_high_gain_above_threshold_tool_call() {
        let scorer = UtilityScorer::new(default_config());
        let score = UtilityScore {
            gain: 0.8,
            cost: 0.1,
            redundancy: 0.0,
            uncertainty: 0.4,
            total: 0.6,
        };
        assert_eq!(
            scorer.recommend_action(Some(&score), &default_ctx()),
            UtilityAction::ToolCall
        );
    }

    #[test]
    fn recommend_action_uncertain_retrieves() {
        let scorer = UtilityScorer::new(default_config());
        // gain >= 0.5, uncertainty > 0.5, but gain < 0.7 so rule 3 not triggered
        let score = UtilityScore {
            gain: 0.6,
            cost: 0.1,
            redundancy: 0.0,
            uncertainty: 0.8,
            total: 0.4,
        };
        assert_eq!(
            scorer.recommend_action(Some(&score), &default_ctx()),
            UtilityAction::Retrieve
        );
    }

    #[test]
    fn recommend_action_below_threshold_with_prior_calls_verifies() {
        let scorer = UtilityScorer::new(default_config());
        let score = UtilityScore {
            gain: 0.3,
            cost: 0.1,
            redundancy: 0.0,
            uncertainty: 0.2,
            total: 0.05, // below default threshold 0.1
        };
        let ctx = UtilityContext {
            tool_calls_this_turn: 1,
            ..default_ctx()
        };
        assert_eq!(
            scorer.recommend_action(Some(&score), &ctx),
            UtilityAction::Verify
        );
    }

    #[test]
    fn recommend_action_default_responds() {
        let scorer = UtilityScorer::new(default_config());
        let score = UtilityScore {
            gain: 0.3,
            cost: 0.1,
            redundancy: 0.0,
            uncertainty: 0.2,
            total: 0.05, // below threshold, no prior calls
        };
        let ctx = UtilityContext {
            tool_calls_this_turn: 0,
            ..default_ctx()
        };
        assert_eq!(
            scorer.recommend_action(Some(&score), &ctx),
            UtilityAction::Respond
        );
    }
}
