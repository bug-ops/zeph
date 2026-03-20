// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Unit tests for agent state sub-structs.
//!
//! These tests verify construction and field access for each sub-struct defined
//! in `agent/state/mod.rs`. They act as change-detectors: if a struct is
//! restructured (fields renamed, types changed), the compilation errors pinpoint
//! which sub-struct changed.

use std::collections::VecDeque;

use crate::agent::feedback_detector::FeedbackDetector;
use crate::agent::rate_limiter::{RateLimitConfig, ToolRateLimiter};
use crate::agent::state::{
    ExperimentState, FeedbackState, InstructionState, MessageState, RuntimeConfig, SessionState,
};
use crate::config::{SecurityConfig, TimeoutConfig};
use crate::context::EnvironmentContext;

fn make_instruction_state() -> InstructionState {
    InstructionState {
        blocks: Vec::new(),
        reload_rx: None,
        reload_state: None,
    }
}

fn make_experiment_state() -> ExperimentState {
    #[cfg(feature = "experiments")]
    let (notify_tx, notify_rx) = tokio::sync::mpsc::channel::<String>(4);
    #[cfg(not(feature = "experiments"))]
    let (_tx, notify_rx) = tokio::sync::mpsc::channel::<String>(4);

    ExperimentState {
        #[cfg(feature = "experiments")]
        config: crate::config::ExperimentConfig::default(),
        #[cfg(feature = "experiments")]
        cancel: None,
        #[cfg(feature = "experiments")]
        baseline: crate::experiments::ConfigSnapshot::default(),
        notify_rx: Some(notify_rx),
        #[cfg(feature = "experiments")]
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
    }
}

fn make_session_state() -> SessionState {
    SessionState {
        env_context: EnvironmentContext::gather(""),
        response_cache: None,
        parent_tool_use_id: None,
        status_tx: None,
        #[cfg(feature = "lsp-context")]
        lsp_hooks: None,
        #[cfg(feature = "policy-enforcer")]
        policy_config: None,
    }
}

fn make_runtime_config() -> RuntimeConfig {
    RuntimeConfig {
        security: SecurityConfig::default(),
        timeouts: TimeoutConfig::default(),
        model_name: String::new(),
        permission_policy: zeph_tools::PermissionPolicy::default(),
        redact_credentials: true,
        rate_limiter: ToolRateLimiter::new(RateLimitConfig::default()),
        semantic_cache_enabled: false,
        semantic_cache_threshold: 0.95,
        semantic_cache_max_candidates: 10,
        dependency_config: zeph_tools::DependencyConfig::default(),
    }
}

fn make_feedback_state() -> FeedbackState {
    FeedbackState {
        detector: FeedbackDetector::new(0.6),
        judge: None,
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

#[cfg(feature = "experiments")]
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
// CompressionState (context-compression feature gate)
// ------------------------------------------------------------------

#[cfg(feature = "context-compression")]
#[test]
fn compression_state_construction() {
    use crate::agent::state::CompressionState;
    let state = CompressionState {
        current_task_goal: None,
        task_goal_user_msg_hash: None,
        pending_task_goal: None,
        pending_sidequest_result: None,
        subgoal_registry: crate::agent::compaction_strategy::SubgoalRegistry::default(),
        pending_subgoal: None,
        subgoal_user_msg_hash: None,
    };
    assert!(state.current_task_goal.is_none());
    assert!(state.task_goal_user_msg_hash.is_none());
}
