// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Unit tests for agent state sub-structs.
//!
//! These tests verify construction and field access for each sub-struct defined
//! in `agent/state/mod.rs`. They act as change-detectors: if a struct is
//! restructured (fields renamed, types changed), the compilation errors pinpoint
//! which sub-struct changed.

use std::collections::VecDeque;

use crate::agent::rate_limiter::{RateLimitConfig, ToolRateLimiter};
use crate::agent::state::{
    ExperimentState, FeedbackState, HooksConfigSnapshot, InstructionState, MessageState,
    RuntimeConfig, SessionState,
};
use crate::config::{SecurityConfig, TimeoutConfig};
use crate::context::EnvironmentContext;
use zeph_agent_feedback::FeedbackDetector;

fn make_instruction_state() -> InstructionState {
    InstructionState {
        blocks: Vec::new(),
        reload_rx: None,
        reload_state: None,
    }
}

fn make_experiment_state() -> ExperimentState {
    let (notify_tx, notify_rx) = tokio::sync::mpsc::channel::<String>(4);

    ExperimentState {
        config: crate::config::ExperimentConfig::default(),
        cancel: None,
        baseline: zeph_experiments::ConfigSnapshot::default(),
        eval_provider: None,
        notify_rx: Some(notify_rx),
        notify_tx,
    }
}

fn make_message_state() -> MessageState {
    use zeph_llm::provider::{Message, MessageMetadata, Role};
    MessageState {
        messages: vec![Message {
            role: Role::System,
            content: String::from("system"),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }],
        message_queue: VecDeque::new(),
        pending_image_parts: Vec::new(),
        last_persisted_message_id: None,
        deferred_db_hide_ids: Vec::new(),
        deferred_db_summaries: Vec::new(),
    }
}

fn make_session_state() -> SessionState {
    SessionState {
        env_context: EnvironmentContext::gather(""),
        response_cache: None,
        parent_tool_use_id: None,
        current_turn_intent: None,
        status_tx: None,
        lsp_hooks: None,
        policy_config: None,
        hooks_config: HooksConfigSnapshot::default(),
        last_assistant_at: None,
    }
}

fn make_runtime_config() -> RuntimeConfig {
    RuntimeConfig {
        security: SecurityConfig::default(),
        timeouts: TimeoutConfig::default(),
        model_name: String::new(),
        active_provider_name: String::new(),
        permission_policy: zeph_tools::PermissionPolicy::default(),
        redact_credentials: true,
        rate_limiter: ToolRateLimiter::new(RateLimitConfig::default()),
        semantic_cache_enabled: false,
        semantic_cache_threshold: 0.95,
        semantic_cache_max_candidates: 10,
        dependency_config: zeph_tools::DependencyConfig::default(),
        adversarial_policy_info: None,
        spawn_depth: 0,
        budget_hint_enabled: true,
        channel_skills: zeph_config::ChannelSkillsConfig::default(),
        loop_min_interval_secs: 5,
        layers: Vec::new(),
        supervisor_config: crate::config::TaskSupervisorConfig::default(),
        recap_config: zeph_config::RecapConfig::default(),
        acp_config: zeph_config::AcpConfig::default(),
        auto_recap_shown: false,
        msg_count_at_resume: 0,
        acp_subagent_spawn_fn: None,
        channel_type: String::new(),
        provider_persistence_enabled: true,
    }
}

fn make_feedback_state() -> FeedbackState {
    FeedbackState {
        detector: FeedbackDetector::new(0.6),
        judge: None,
        llm_classifier: None,
    }
}

// ------------------------------------------------------------------
// InstructionState
// ------------------------------------------------------------------

#[test]
fn instruction_state_construction() {
    let state = make_instruction_state();
    assert!(state.blocks.is_empty());
    assert!(state.reload_rx.is_none());
    assert!(state.reload_state.is_none());
}

// ------------------------------------------------------------------
// ExperimentState
// ------------------------------------------------------------------

#[test]
fn experiment_state_notify_rx_always_present() {
    // notify_rx is unconditional regardless of the `experiments` feature.
    let state = make_experiment_state();
    assert!(state.notify_rx.is_some());
}
#[test]
fn experiment_state_cfg_fields_present_with_feature() {
    let state = make_experiment_state();
    // `config`, `cancel`, `baseline`, and `notify_tx` are only present with the feature.
    let _ = &state.config;
    let _ = &state.cancel;
    let _ = &state.baseline;
    let _ = &state.notify_tx;
}

// ------------------------------------------------------------------
// MessageState
// ------------------------------------------------------------------

#[test]
fn message_state_construction() {
    let state = make_message_state();
    // messages contains the initial system prompt injected by Agent::new.
    assert!(!state.messages.is_empty());
    assert!(state.message_queue.is_empty());
    assert!(state.pending_image_parts.is_empty());
}

// ------------------------------------------------------------------
// SessionState
// ------------------------------------------------------------------

#[test]
fn session_state_optional_fields_default_none() {
    let state = make_session_state();
    assert!(state.response_cache.is_none());
    assert!(state.parent_tool_use_id.is_none());
    assert!(state.status_tx.is_none());
}

// ------------------------------------------------------------------
// RuntimeConfig (includes rate_limiter after #1971)
// ------------------------------------------------------------------

#[test]
fn runtime_config_construction() {
    let config = make_runtime_config();
    assert!(config.model_name.is_empty());
    assert!(config.redact_credentials);
}

#[test]
fn runtime_config_contains_rate_limiter() {
    // Verify rate_limiter is accessible as a field of RuntimeConfig.
    let mut config = make_runtime_config();
    let results = config.rate_limiter.check_batch(&["shell"]);
    // Default config allows at least one call — None means allowed.
    assert_eq!(results.len(), 1);
    assert!(results[0].is_none());
}

// ------------------------------------------------------------------
// FeedbackState (new in #1971)
// ------------------------------------------------------------------

#[test]
fn feedback_state_construction() {
    let state = make_feedback_state();
    // judge is None by default.
    assert!(state.judge.is_none());
}

#[test]
fn feedback_state_detector_returns_none_for_neutral_input() {
    let state = make_feedback_state();
    // Neutral phrasing produces no correction signal.
    let signal = state.detector.detect("please continue", &[]);
    assert!(signal.is_none());
}

// ------------------------------------------------------------------
// LifecycleState — no_providers backoff field added in #3357
// ------------------------------------------------------------------

#[test]
fn lifecycle_state_last_no_providers_at_starts_none() {
    use crate::agent::state::LifecycleState;
    let state = LifecycleState::new();
    assert!(
        state.last_no_providers_at.is_none(),
        "last_no_providers_at must be None at startup — backoff is inactive until a NoProviders error occurs (#3357)"
    );
}

#[test]
#[allow(clippy::unchecked_time_subtraction, clippy::nonminimal_bool)]
fn lifecycle_state_last_no_providers_at_elapsed_check() {
    use std::time::{Duration, Instant};
    // Simulate the backoff gate logic used in advance_context_lifecycle_guarded.
    // When last_no_providers_at was set very recently, providers_recently_failed must be true.
    let backoff_secs = 2u64;
    let last_no_providers_at: Option<Instant> = Some(Instant::now());
    let providers_recently_failed =
        last_no_providers_at.is_some_and(|t| t.elapsed().as_secs() < backoff_secs);
    assert!(
        providers_recently_failed,
        "a just-set last_no_providers_at must trigger the backoff gate"
    );

    // When last_no_providers_at is old enough, the gate must be open.
    let old_instant = Instant::now() - Duration::from_secs(backoff_secs + 1);
    let old_no_providers: Option<Instant> = Some(old_instant);
    let gate_open = !old_no_providers.is_some_and(|t| t.elapsed().as_secs() < backoff_secs);
    assert!(
        gate_open,
        "an expired last_no_providers_at must not block context preparation"
    );
}

#[test]
fn lifecycle_state_last_no_providers_at_none_means_gate_open() {
    let backoff_secs = 2u64;
    let last_no_providers_at: Option<std::time::Instant> = None;
    let providers_recently_failed =
        last_no_providers_at.is_some_and(|t| t.elapsed().as_secs() < backoff_secs);
    assert!(
        !providers_recently_failed,
        "None last_no_providers_at must not trigger the backoff gate"
    );
}

// ------------------------------------------------------------------
// CompressionState (context-compression feature gate)
// ------------------------------------------------------------------
#[test]
fn compression_state_construction() {
    use crate::agent::state::CompressionState;
    let state = CompressionState {
        current_task_goal: None,
        task_goal_user_msg_hash: None,
        pending_task_goal: None,
        pending_sidequest_result: None,
        subgoal_registry: zeph_agent_context::SubgoalRegistry::default(),
        pending_subgoal: None,
        subgoal_user_msg_hash: None,
    };
    assert!(state.current_task_goal.is_none());
    assert!(state.task_goal_user_msg_hash.is_none());
}
