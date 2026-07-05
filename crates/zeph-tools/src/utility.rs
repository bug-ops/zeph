// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Utility-guided tool dispatch gate (arXiv:2603.19896).
//!
//! Computes a scalar utility score for each candidate tool call before execution.
//! Calls below the configured threshold are skipped (fail-closed on scoring errors).

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::LazyLock;

use regex::Regex;

use crate::config::UtilityScoringConfig;
use crate::executor::ToolCall;

/// Returns `true` when a user message explicitly requests tool invocation.
///
/// Patterns are matched case-insensitively against the user message text.
/// This is intentionally limited to unambiguous phrasings to avoid false positives
/// that would incorrectly bypass the utility gate.
///
/// Safe to call on user-supplied text — does NOT bypass prompt-injection defences
/// because those are enforced on tool OUTPUT paths, not on gate routing decisions.
#[must_use]
pub fn has_explicit_tool_request(user_message: &str) -> bool {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?xi)
            using\s+a\s+tool
            | call\s+(the\s+)?[a-z_]+\s+tool
            | use\s+(the\s+)?[a-z_]+\s+tool
            | run\s+(the\s+)?[a-z_]+\s+tool
            | invoke\s+(the\s+)?[a-z_]+\s+tool
            | execute\s+(the\s+)?[a-z_]+\s+tool
            | show\s+me\s+the\s+result\s+of\s*:
            | run\s*:
            | execute\s*:
            | what\s+(does|would|is\s+the\s+output\s+of)
            ",
        )
        .expect("static regex is valid")
    });
    // Inline code blocks with shell syntax are matched separately to avoid
    // making the extended-mode regex unwieldy with backticks.
    static RE_CODE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"`[^`]*[|><$;&][^`]*`").expect("static regex is valid"));
    RE.is_match(user_message) || RE_CODE.is_match(user_message)
}

/// Estimated gain for known tool categories.
///
/// Keys are exact tool name prefixes or names. Higher value = more expected gain.
/// Unknown tools default to 0.5 (neutral).
///
/// Tools scoring `>= 0.7` take the direct `ToolCall` branch in `recommend_action`
/// before the exploratory `Retrieve` rule (which requires `gain < 0.7`) is ever
/// considered. Deterministic, self-contained actions — a compiler diagnostics run,
/// a file edit or rename, a directory mutation — gain nothing from being routed
/// through "retrieve context first, then retry": there is no missing context a
/// memory search could supply, so forcing that detour only produces a stalled
/// retry that then gets vetoed again as a redundant duplicate call (#5650).
///
/// This table only covers built-in tool ids known at compile time. Dynamically
/// registered tools — most notably MCP tools, whose ids are `{server_id}_{name}`
/// (see `McpTool::sanitized_id`) — never match a hardcoded name here and fall to the
/// generic `0.5` bucket, exposing them to the same stall #5650 fixed for built-ins.
/// `UtilityScorer::score` checks `UtilityScoringConfig::high_gain_tools` before
/// calling this function so operators can opt individual MCP (or future built-in)
/// tool ids into the `0.75` tier without a code change (#5659).
fn default_gain(tool_name: &str) -> f32 {
    if tool_name.starts_with("memory") {
        return 0.8;
    }
    match tool_name {
        "bash" | "shell" => 0.6,
        "read" | "write" => 0.55,
        "search_code" | "grep" | "glob" | "find_path" | "list_directory" => 0.65,
        "diagnostics" | "edit" | "format" | "create_directory" | "delete_path" | "move_path"
        | "copy_path" => 0.75,
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
    /// True when the user explicitly requested tool invocation — either via a `/tool` slash
    /// command or when the user message contains an unambiguous tool-invocation phrase detected
    /// by [`has_explicit_tool_request`]. Must NOT be set from LLM call content or tool outputs.
    pub user_requested: bool,
    /// True when this exact call is the mandated retry the `Retrieve` rule itself asked for:
    /// the same `(tool_id, params)` call was vetoed by rule 8 earlier this turn, and the
    /// injected system hint explicitly instructed the LLM to call it again with the same
    /// arguments. Set via [`UtilityScorer::take_mandated_retry`].
    ///
    /// When `true`, `recommend_action` must not re-veto the retry through the redundancy
    /// (`Respond`) rule — doing so fabricates a rejection despite the model correctly
    /// complying with the gate's own hint, stalling tools like `find_path`/`list_directory`
    /// in a doomed Retrieve-then-redundant-Respond cycle (#5719).
    pub mandated_retry: bool,
}

#[non_exhaustive]
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
    /// Count of consecutive non-`ToolCall` recommendations since the last `ToolCall` or turn reset.
    consecutive_low: usize,
    /// Hashes of `(tool_name, params)` calls vetoed by the `Retrieve` rule this turn that are
    /// awaiting the mandated retry the injected hint requested (#5719). Entries are consumed by
    /// [`take_mandated_retry`](Self::take_mandated_retry) so the bypass applies exactly once per
    /// veto — a genuine subsequent duplicate call is scored normally.
    mandated_retries: HashSet<u64>,
}

impl UtilityScorer {
    /// Create a new scorer from the given config.
    #[must_use]
    pub fn new(config: UtilityScoringConfig) -> Self {
        Self {
            config,
            recent_calls: HashMap::new(),
            consecutive_low: 0,
            mandated_retries: HashSet::new(),
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

        let gain = if self.is_high_gain(call.tool_id.as_str()) {
            0.75
        } else {
            default_gain(call.tool_id.as_str())
        };

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
    /// 3. `mandated_retry` → always `ToolCall` — this is the retry the `Retrieve` rule (8)
    ///    itself demanded via the injected hint; the gate must honor its own contract instead
    ///    of re-vetoing compliance as a redundant duplicate (#5719).
    /// 4. `score` is `None` (invalid score, scoring enabled) → `Stop` (fail-closed).
    /// 5. `cost > 0.9` (budget nearly exhausted) → `Stop`.
    /// 6. `redundancy == 1.0` (duplicate call) → `Respond`.
    /// 7. `gain >= 0.7 && total >= threshold` → `ToolCall`.
    /// 8. `gain >= 0.5 && uncertainty > 0.5` → `Retrieve`.
    /// 9. `total < threshold && tool_calls_this_turn > 0` → `Verify`.
    /// 10. `total >= threshold` → `ToolCall`.
    /// 11. Default → `Respond`.
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
        // Bypass: this call is the mandated retry the Retrieve rule itself asked for (#5719).
        if ctx.mandated_retry {
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
        self.consecutive_low = 0;
        self.mandated_retries.clear();
    }

    /// Marks `call` as an in-flight mandated retry after the `Retrieve` rule instructs the LLM
    /// to call it again with the same arguments (#5719).
    ///
    /// Called by the tool loop when it injects the "you MUST call it again" hint. The next
    /// occurrence of the identical call this turn bypasses the redundancy veto — see
    /// [`take_mandated_retry`](Self::take_mandated_retry).
    pub fn mark_mandated_retry(&mut self, call: &ToolCall) {
        self.mandated_retries.insert(call_hash(call));
    }

    /// Returns `true` and consumes the pending mandated-retry marker for `call`, if any.
    ///
    /// Consuming rather than merely peeking ensures the bypass applies exactly once per
    /// `Retrieve` veto: a genuine third identical call afterward is scored normally instead of
    /// being exempted forever.
    pub fn take_mandated_retry(&mut self, call: &ToolCall) -> bool {
        self.mandated_retries.remove(&call_hash(call))
    }

    /// Record the recommended action and check whether the consecutive-low-utility window is
    /// exhausted.
    ///
    /// Returns `true` when `config.utility_window > 0` and `consecutive_low >= utility_window`,
    /// indicating that the current batch should be downgraded and the caller should signal
    /// a hard break of the outer iteration loop. Always returns `false` when
    /// `utility_window == 0` (disabled) so existing behaviour is fully preserved.
    ///
    /// Must be called only for calls that actually went through `recommend_action` — exempt and
    /// pre-exec-blocked calls bypass scoring and must NOT call this method.
    pub fn note_action(&mut self, action: &UtilityAction) -> bool {
        if *action == UtilityAction::ToolCall {
            self.consecutive_low = 0;
        } else {
            self.consecutive_low = self.consecutive_low.saturating_add(1);
        }
        self.config.utility_window > 0 && self.consecutive_low >= self.config.utility_window
    }

    /// Returns `true` when `tool_name` case-insensitively matches an entry in `list`.
    ///
    /// Shared lookup behind `is_exempt` and `is_high_gain` — both are "does a configured
    /// tool-name list contain this `tool_id`" checks and must use identical matching rules.
    ///
    /// Normalizes `:` to `_` on both sides before comparing (#5713): MCP tool ids are dispatched
    /// as `McpTool::sanitized_id()` ("`{server_id}_{name}`", underscore-separated), but the only
    /// runtime surface that lists them to an operator — the TUI `mcp:list` command — displays
    /// `McpTool::qualified_name()` ("`{server_id}:{name}`", colon-separated). Without this
    /// normalization, an id copied straight from `mcp:list` into config never matches the
    /// incoming `tool_id`.
    fn contains_tool_name(list: &[String], tool_name: &str) -> bool {
        let normalize = |s: &str| s.to_lowercase().replace(':', "_");
        let normalized = normalize(tool_name);
        list.iter().any(|e| normalize(e) == normalized)
    }

    /// Returns `true` when `tool_name` is in the exempt list (case-insensitive).
    ///
    /// Exempt tools bypass the utility gate unconditionally and always receive `ToolCall`.
    #[must_use]
    pub fn is_exempt(&self, tool_name: &str) -> bool {
        Self::contains_tool_name(&self.config.exempt_tools, tool_name)
    }

    /// Returns `true` when `tool_name` is in the `high_gain_tools` opt-in list (case-insensitive).
    ///
    /// Tools in this list receive the same `0.75` "direct action" gain as
    /// `diagnostics`/`edit`/etc, regardless of whether `default_gain` has a hardcoded entry
    /// for them. Intended for MCP-registered tools whose ids `default_gain` can never match
    /// (#5659).
    #[must_use]
    pub fn is_high_gain(&self, tool_name: &str) -> bool {
        Self::contains_tool_name(&self.config.high_gain_tools, tool_name)
    }

    /// The configured threshold.
    #[must_use]
    pub fn threshold(&self) -> f32 {
        self.config.threshold
    }

    /// The configured consecutive-low-utility window size. 0 means disabled.
    #[must_use]
    pub fn utility_window(&self) -> usize {
        self.config.utility_window
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolName;
    use serde_json::json;

    fn make_call(name: &str, params: serde_json::Value) -> ToolCall {
        ToolCall {
            tool_id: ToolName::new(name),
            params: if let serde_json::Value::Object(m) = params {
                m
            } else {
                serde_json::Map::new()
            },
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        }
    }

    fn default_ctx() -> UtilityContext {
        UtilityContext {
            tool_calls_this_turn: 0,
            tokens_consumed: 0,
            token_budget: 1000,
            user_requested: false,
            mandated_retry: false,
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

    // ── #5650 regression: direct-action tools bypass the Retrieve detour ────────

    #[test]
    fn default_gain_direct_action_tools_reach_tool_call_threshold() {
        for tool in [
            "diagnostics",
            "edit",
            "format",
            "create_directory",
            "delete_path",
            "move_path",
            "copy_path",
        ] {
            let gain = default_gain(tool);
            assert!(gain >= 0.7, "{tool} gain should be >= 0.7, got {gain}");
        }
    }

    #[test]
    fn default_gain_find_path_and_list_directory_match_grep_glob_tier() {
        for tool in ["find_path", "list_directory", "grep", "glob"] {
            let gain = default_gain(tool);
            assert!(
                (gain - 0.65).abs() < f32::EPSILON,
                "{tool} gain should be 0.65, got {gain}"
            );
        }
    }

    #[test]
    fn recommend_action_direct_tools_execute_on_first_call() {
        // Regression test for #5650: these tools previously fell through to the
        // generic 0.5 gain bucket, which routed a fresh first call (uncertainty ~1.0)
        // through rule 7 (Retrieve) instead of rule 6 (ToolCall), stalling the tool
        // behind a doomed Retrieve -> redundant-Respond cycle.
        let scorer = UtilityScorer::new(default_config());
        let ctx = default_ctx(); // tool_calls_this_turn: 0 -> uncertainty == 1.0
        for tool in [
            "diagnostics",
            "edit",
            "format",
            "create_directory",
            "delete_path",
            "move_path",
            "copy_path",
        ] {
            let call = make_call(tool, json!({}));
            let score = scorer.score(&call, &ctx).unwrap();
            assert!(
                score.gain >= 0.7,
                "{tool} gain should be >= 0.7, got {}",
                score.gain
            );
            assert_eq!(
                scorer.recommend_action(Some(&score), &ctx),
                UtilityAction::ToolCall,
                "{tool} should execute immediately on first call, not stall on Retrieve"
            );
        }
    }

    #[test]
    fn recommend_action_unclassified_tools_still_retrieve_on_first_call() {
        // Documents preserved, intentional behavior: tools that genuinely benefit
        // from a "retrieve context first" detour (or unknown tool ids) remain in the
        // 0.5 default-gain bucket and may still receive Retrieve on a fresh first
        // call. This is not the #5650 regression — it only affected tools that have
        // no exploratory value to gain from the detour.
        let scorer = UtilityScorer::new(default_config());
        let ctx = default_ctx();
        for tool in ["fetch", "totally_unrecognized_tool_xyz"] {
            let call = make_call(tool, json!({}));
            let score = scorer.score(&call, &ctx).unwrap();
            assert!((score.gain - 0.5).abs() < f32::EPSILON);
            assert_eq!(
                scorer.recommend_action(Some(&score), &ctx),
                UtilityAction::Retrieve,
                "{tool} should still be eligible for Retrieve on first call"
            );
        }
    }

    #[test]
    fn recommend_action_diagnostics_never_enters_the_retrieve_redundant_respond_stall() {
        // Contrasts the fixed diagnostics tool with the still-affected fetch tool:
        // fetch's first call recommends Retrieve, and once the identical retry is
        // recorded it becomes a redundant duplicate that resolves to Respond — the
        // exact two-step no-op #5650 reported. diagnostics's gain (0.75) means it
        // takes the ToolCall branch on the very first call, so it never enters that
        // stall in the first place.
        let mut scorer = UtilityScorer::new(default_config());
        let ctx = default_ctx();

        let fetch_call = make_call("fetch", json!({"url": "https://example.com"}));
        let fetch_score = scorer.score(&fetch_call, &ctx).unwrap();
        assert_eq!(
            scorer.recommend_action(Some(&fetch_score), &ctx),
            UtilityAction::Retrieve
        );
        scorer.record_call(&fetch_call);
        let fetch_retry_score = scorer.score(&fetch_call, &ctx).unwrap();
        assert_eq!(
            scorer.recommend_action(Some(&fetch_retry_score), &ctx),
            UtilityAction::Respond,
            "identical retry should be flagged as redundant, reproducing the stall"
        );

        let diagnostics_call = make_call("diagnostics", json!({}));
        let diagnostics_score = scorer.score(&diagnostics_call, &ctx).unwrap();
        assert_eq!(
            scorer.recommend_action(Some(&diagnostics_score), &ctx),
            UtilityAction::ToolCall,
            "diagnostics must execute on the first call, bypassing the stall entirely"
        );
    }

    // ── has_explicit_tool_request tests ──────────────────────────────────────

    #[test]
    fn explicit_request_using_a_tool() {
        assert!(has_explicit_tool_request(
            "Please list the files in the current directory using a tool"
        ));
    }

    #[test]
    fn explicit_request_call_the_tool() {
        assert!(has_explicit_tool_request("call the list_directory tool"));
    }

    #[test]
    fn explicit_request_use_the_tool() {
        assert!(has_explicit_tool_request("use the shell tool to run ls"));
    }

    #[test]
    fn explicit_request_run_the_tool() {
        assert!(has_explicit_tool_request("run the bash tool"));
    }

    #[test]
    fn explicit_request_invoke_the_tool() {
        assert!(has_explicit_tool_request("invoke the search_code tool"));
    }

    #[test]
    fn explicit_request_execute_the_tool() {
        assert!(has_explicit_tool_request("execute the grep tool for me"));
    }

    #[test]
    fn explicit_request_case_insensitive() {
        assert!(has_explicit_tool_request("USING A TOOL to find files"));
    }

    #[test]
    fn explicit_request_no_match_plain_message() {
        assert!(!has_explicit_tool_request("what is the weather today?"));
    }

    #[test]
    fn explicit_request_no_match_tool_mentioned_without_invocation() {
        assert!(!has_explicit_tool_request(
            "the shell tool is very useful in general"
        ));
    }

    #[test]
    fn explicit_request_show_me_result_of() {
        assert!(has_explicit_tool_request(
            "show me the result of: echo hello"
        ));
    }

    #[test]
    fn explicit_request_run_colon() {
        assert!(has_explicit_tool_request("run: echo hello"));
    }

    #[test]
    fn explicit_request_execute_colon() {
        assert!(has_explicit_tool_request("execute: ls -la"));
    }

    #[test]
    fn explicit_request_what_does() {
        assert!(has_explicit_tool_request("what does echo hello output?"));
    }

    #[test]
    fn explicit_request_what_would() {
        assert!(has_explicit_tool_request("what would cat /etc/hosts show?"));
    }

    #[test]
    fn explicit_request_what_is_the_output_of() {
        assert!(has_explicit_tool_request(
            "what is the output of ls | grep foo?"
        ));
    }

    #[test]
    fn explicit_request_inline_code_pipe() {
        assert!(has_explicit_tool_request("try running `ls | grep foo`"));
    }

    #[test]
    fn explicit_request_inline_code_redirect() {
        assert!(has_explicit_tool_request("run `echo hello > /tmp/out`"));
    }

    #[test]
    fn explicit_request_inline_code_dollar() {
        assert!(has_explicit_tool_request("check `$HOME/bin`"));
    }

    #[test]
    fn explicit_request_inline_code_and() {
        assert!(has_explicit_tool_request("try `git fetch && git rebase`"));
    }

    #[test]
    fn no_match_run_the_tests() {
        assert!(!has_explicit_tool_request("run the tests please"));
    }

    #[test]
    fn no_match_execute_the_plan() {
        assert!(!has_explicit_tool_request("execute the plan we discussed"));
    }

    #[test]
    fn no_match_inline_code_no_shell_syntax() {
        assert!(!has_explicit_tool_request(
            "the function `process_items` handles it"
        ));
    }

    // "what does this function do?" triggers the wide `what\s+(does|...)` pattern.
    // This is an acceptable false positive: users asking "what does X do?" in the
    // context of shell commands benefit from the gate bypass, and the cost of an
    // occasional extra tool call for a prose question is low.
    #[test]
    fn known_fp_what_does_function_do() {
        // Documents known false-positive: prose "what does X do?" also matches.
        assert!(has_explicit_tool_request("what does this function do?"));
    }

    #[test]
    fn no_match_show_me_result_without_colon() {
        // Without the trailing colon the phrase is ambiguous prose, should not match.
        assert!(!has_explicit_tool_request(
            "show me the result of running it"
        ));
    }

    #[test]
    fn is_exempt_matches_case_insensitively() {
        let scorer = UtilityScorer::new(UtilityScoringConfig {
            enabled: true,
            exempt_tools: vec!["Read".to_owned(), "file_read".to_owned()],
            ..UtilityScoringConfig::default()
        });
        assert!(scorer.is_exempt("read"));
        assert!(scorer.is_exempt("READ"));
        assert!(scorer.is_exempt("FILE_READ"));
        assert!(!scorer.is_exempt("write"));
        assert!(!scorer.is_exempt("bash"));
    }

    #[test]
    fn is_exempt_empty_list_returns_false() {
        let scorer = UtilityScorer::new(UtilityScoringConfig::default());
        assert!(!scorer.is_exempt("read"));
    }

    // ── high_gain_tools opt-in tests (#5659) ─────────────────────────────────

    #[test]
    fn is_high_gain_matches_case_insensitively() {
        let scorer = UtilityScorer::new(UtilityScoringConfig {
            enabled: true,
            high_gain_tools: vec!["Github_create_issue".to_owned()],
            ..UtilityScoringConfig::default()
        });
        assert!(scorer.is_high_gain("github_create_issue"));
        assert!(scorer.is_high_gain("GITHUB_CREATE_ISSUE"));
        assert!(!scorer.is_high_gain("bash"));
    }

    #[test]
    fn is_high_gain_empty_list_returns_false() {
        let scorer = UtilityScorer::new(UtilityScoringConfig::default());
        assert!(!scorer.is_high_gain("github_create_issue"));
    }

    #[test]
    fn default_gain_unconfigured_mcp_shaped_tool_id_stays_neutral() {
        // Real MCP tool ids are "{server_id}_{name}" (McpTool::sanitized_id), not literally
        // prefixed with "mcp_". Without an opt-in high_gain_tools entry, such an id has no
        // hardcoded match and stays in the generic 0.5 bucket.
        assert!((default_gain("github_create_issue") - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn score_high_gain_tools_overrides_default_gain_for_mcp_shaped_tool_id() {
        // Reproduces the #5659 gap: an MCP tool id ("github_create_issue", shaped like
        // McpTool::sanitized_id's "{server_id}_{name}") has no entry in default_gain's
        // hardcoded table and would default to 0.5. Opting it into high_gain_tools must
        // raise its gain to 0.75 and let it take the direct ToolCall branch on the first
        // call, exactly like the #5650 fix does for built-in direct-action tools.
        let scorer = UtilityScorer::new(UtilityScoringConfig {
            enabled: true,
            high_gain_tools: vec!["github_create_issue".to_owned()],
            ..UtilityScoringConfig::default()
        });
        let ctx = default_ctx(); // tool_calls_this_turn: 0 -> uncertainty == 1.0
        let call = make_call("github_create_issue", json!({}));
        let score = scorer.score(&call, &ctx).unwrap();
        assert!(
            (score.gain - 0.75).abs() < f32::EPSILON,
            "high_gain_tools entry should raise gain to 0.75, got {}",
            score.gain
        );
        assert_eq!(
            scorer.recommend_action(Some(&score), &ctx),
            UtilityAction::ToolCall,
            "high-gain MCP tool should execute immediately on first call, not stall on Retrieve"
        );
    }

    #[test]
    fn score_high_gain_tools_does_not_affect_unlisted_tools() {
        let scorer = UtilityScorer::new(UtilityScoringConfig {
            enabled: true,
            high_gain_tools: vec!["github_create_issue".to_owned()],
            ..UtilityScoringConfig::default()
        });
        let ctx = default_ctx();
        let call = make_call("fetch", json!({}));
        let score = scorer.score(&call, &ctx).unwrap();
        assert!(
            (score.gain - 0.5).abs() < f32::EPSILON,
            "unlisted tool must keep its default_gain value, got {}",
            score.gain
        );
    }

    // ── high_gain_tools colon/underscore dual-form matching (#5713) ─────────

    #[test]
    fn is_high_gain_matches_qualified_name_config_against_sanitized_id_call() {
        // Operator copies the colon-separated `McpTool::qualified_name()` form from `mcp:list`
        // into config, but the incoming tool_id is always the underscore-separated
        // `McpTool::sanitized_id()` dispatch form.
        let scorer = UtilityScorer::new(UtilityScoringConfig {
            enabled: true,
            high_gain_tools: vec!["myserver:mytool".to_owned()],
            ..UtilityScoringConfig::default()
        });
        assert!(scorer.is_high_gain("myserver_mytool"));
    }

    #[test]
    fn is_high_gain_matches_sanitized_id_config_against_qualified_name_call() {
        // Symmetric case: config already uses the underscore form, incoming id uses colons.
        let scorer = UtilityScorer::new(UtilityScoringConfig {
            enabled: true,
            high_gain_tools: vec!["myserver_mytool".to_owned()],
            ..UtilityScoringConfig::default()
        });
        assert!(scorer.is_high_gain("myserver:mytool"));
    }

    #[test]
    fn is_exempt_matches_qualified_name_config_against_sanitized_id_call() {
        // is_exempt shares contains_tool_name with is_high_gain and must get the same fix.
        let scorer = UtilityScorer::new(UtilityScoringConfig {
            enabled: true,
            exempt_tools: vec!["myserver:mytool".to_owned()],
            ..UtilityScoringConfig::default()
        });
        assert!(scorer.is_exempt("myserver_mytool"));
    }

    #[test]
    fn is_high_gain_dual_form_still_case_insensitive() {
        let scorer = UtilityScorer::new(UtilityScoringConfig {
            enabled: true,
            high_gain_tools: vec!["MyServer:MyTool".to_owned()],
            ..UtilityScoringConfig::default()
        });
        assert!(scorer.is_high_gain("myserver_mytool"));
        assert!(scorer.is_high_gain("MYSERVER_MYTOOL"));
    }

    #[test]
    fn is_high_gain_dual_form_does_not_match_unrelated_tool() {
        let scorer = UtilityScorer::new(UtilityScoringConfig {
            enabled: true,
            high_gain_tools: vec!["myserver:mytool".to_owned()],
            ..UtilityScoringConfig::default()
        });
        assert!(!scorer.is_high_gain("otherserver_othertool"));
    }

    // ── mandated-retry bypass (#5719) ────────────────────────────────────────
    //
    // Reproduces the stall reported in #5719: find_path/list_directory (default_gain 0.65)
    // trigger rule 8 (Retrieve) on a fresh first call. The injected hint tells the LLM to
    // retry with the same arguments, but record_call() already logged the call hash, so the
    // identical retry scores redundancy=1.0 and rule 6 (Respond) vetoes it a second time —
    // the tool never executes and the turn ends with a fabricated "restriction" apology.

    #[test]
    fn mark_and_take_mandated_retry_is_consumed_exactly_once() {
        let mut scorer = UtilityScorer::new(default_config());
        let call = make_call("find_path", json!({"pattern": "*.rs"}));

        assert!(
            !scorer.take_mandated_retry(&call),
            "no marker set yet — must not report a pending retry"
        );

        scorer.mark_mandated_retry(&call);
        assert!(
            scorer.take_mandated_retry(&call),
            "marker set — first take must report the pending retry"
        );
        assert!(
            !scorer.take_mandated_retry(&call),
            "marker consumed — second take must not report a pending retry again"
        );
    }

    #[test]
    fn recommend_action_mandated_retry_bypasses_redundancy_veto() {
        let scorer = UtilityScorer::new(default_config());
        // Simulate the exact retry scenario: identical call already recorded (redundancy=1.0),
        // which alone would trigger rule 6 (Respond).
        let score = UtilityScore {
            gain: 0.65,
            cost: 0.1,
            redundancy: 1.0,
            uncertainty: 0.7,
            total: 0.5,
        };
        let ctx = UtilityContext {
            mandated_retry: true,
            ..default_ctx()
        };
        assert_eq!(
            scorer.recommend_action(Some(&score), &ctx),
            UtilityAction::ToolCall,
            "mandated retry must bypass the redundancy veto and execute"
        );
    }

    #[test]
    fn find_path_retrieve_then_mandated_retry_executes_not_redundant_respond() {
        let mut scorer = UtilityScorer::new(default_config());
        let ctx = default_ctx(); // tool_calls_this_turn: 0 -> uncertainty == 1.0
        let call = make_call("find_path", json!({"pattern": "*.rs"}));

        // First attempt: gain 0.65 (>= 0.5) with high uncertainty -> Retrieve (rule 8).
        let first_score = scorer.score(&call, &ctx).unwrap();
        assert_eq!(
            scorer.recommend_action(Some(&first_score), &ctx),
            UtilityAction::Retrieve
        );
        // record_call() always runs regardless of the recommended action (mirrors
        // compute_utility_actions in tier_loop.rs), and the tool loop marks the call as an
        // in-flight mandated retry when it injects the "you MUST call it again" hint.
        scorer.record_call(&call);
        scorer.mark_mandated_retry(&call);

        // Without the fix: the retry's redundancy is 1.0 (same hash already recorded), which
        // would trigger rule 6 (Respond) — reproducing the fabricated-restriction stall.
        let retry_ctx = UtilityContext {
            mandated_retry: scorer.take_mandated_retry(&call),
            ..default_ctx()
        };
        assert!(
            retry_ctx.mandated_retry,
            "retry must be recognized as mandated"
        );
        let retry_score = scorer.score(&call, &retry_ctx).unwrap();
        assert!(
            (retry_score.redundancy - 1.0).abs() < f32::EPSILON,
            "retry is indeed flagged redundant by the raw score — the bypass must come from \
             recommend_action, not from suppressing the redundancy component"
        );
        assert_eq!(
            scorer.recommend_action(Some(&retry_score), &retry_ctx),
            UtilityAction::ToolCall,
            "mandated retry must execute instead of being re-vetoed as a redundant duplicate"
        );

        // A genuine third identical call afterward (not requested by any hint) is scored
        // normally again — the bypass must not persist beyond the one mandated retry.
        scorer.record_call(&call);
        let third_ctx = UtilityContext {
            mandated_retry: scorer.take_mandated_retry(&call),
            ..default_ctx()
        };
        assert!(
            !third_ctx.mandated_retry,
            "marker was consumed by the mandated retry — a third call is not exempted"
        );
        let third_score = scorer.score(&call, &third_ctx).unwrap();
        assert_eq!(
            scorer.recommend_action(Some(&third_score), &third_ctx),
            UtilityAction::Respond,
            "a genuine third identical call must be treated as a redundant duplicate"
        );
    }

    #[test]
    fn clear_resets_mandated_retries() {
        let mut scorer = UtilityScorer::new(default_config());
        let call = make_call("find_path", json!({}));
        scorer.mark_mandated_retry(&call);
        scorer.clear();
        assert!(
            !scorer.take_mandated_retry(&call),
            "clear() must reset mandated-retry state at turn start"
        );
    }

    #[test]
    fn note_action_window_zero_never_fires() {
        let mut scorer = UtilityScorer::new(UtilityScoringConfig {
            enabled: true,
            utility_window: 0,
            ..UtilityScoringConfig::default()
        });
        // Any number of non-ToolCall actions must not trigger early-stop when window=0.
        for _ in 0..100 {
            assert!(!scorer.note_action(&UtilityAction::Stop));
        }
    }

    #[test]
    fn note_action_window_three_fires_on_third() {
        let mut scorer = UtilityScorer::new(UtilityScoringConfig {
            enabled: true,
            utility_window: 3,
            ..UtilityScoringConfig::default()
        });
        assert!(!scorer.note_action(&UtilityAction::Stop));
        assert!(!scorer.note_action(&UtilityAction::Respond));
        assert!(scorer.note_action(&UtilityAction::Stop));
    }

    #[test]
    fn note_action_tool_call_resets_counter() {
        let mut scorer = UtilityScorer::new(UtilityScoringConfig {
            enabled: true,
            utility_window: 2,
            ..UtilityScoringConfig::default()
        });
        assert!(!scorer.note_action(&UtilityAction::Stop));
        // ToolCall resets the counter.
        assert!(!scorer.note_action(&UtilityAction::ToolCall));
        // One more non-ToolCall does not fire — counter was reset.
        assert!(!scorer.note_action(&UtilityAction::Stop));
    }

    #[test]
    fn note_action_clear_resets_counter() {
        let mut scorer = UtilityScorer::new(UtilityScoringConfig {
            enabled: true,
            utility_window: 1,
            ..UtilityScoringConfig::default()
        });
        // First Stop would fire (window=1)...
        assert!(scorer.note_action(&UtilityAction::Stop));
        // ...but after clear() the counter is reset so it fires again from scratch.
        scorer.clear();
        assert!(scorer.note_action(&UtilityAction::Stop));
    }
}
