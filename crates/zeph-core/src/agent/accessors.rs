// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! State-group accessor methods for `Agent<C>`.
//!
//! These methods return shared or mutable references to entire state sub-structs,
//! providing encapsulation and a stable API boundary for internal agent state.
//! Mutable access requires an exclusive borrow of `Agent<C>` — simultaneous
//! mutable + immutable access to different sub-structs must use direct field
//! access or sequence the borrows explicitly.
//!
//! Migration pattern: `self.memory_state.field` → `self.memory_state().field`
use super::state::CompressionState;
use super::{
    Agent,
    focus::FocusState,
    learning_engine::LearningEngine,
    sidequest::SidequestState,
    state::{
        DebugState, ExperimentState, FeedbackState, IndexState, InstructionState, LifecycleState,
        McpState, MemoryState, MessageState, MetricsState, OrchestrationState, ProviderState,
        RuntimeConfig, SecurityState, SessionState, SkillState,
    },
    tool_orchestrator::ToolOrchestrator,
};
use crate::channel::Channel;

// Migration is incremental: accessors are defined here and callers are migrated file-by-file.
// During the transition period, not all accessors may be called yet.
#[allow(dead_code)]
impl<C: Channel> Agent<C> {
    #[must_use]
    pub(super) fn msg(&self) -> &MessageState {
        &self.msg
    }

    #[must_use]
    pub(super) fn msg_mut(&mut self) -> &mut MessageState {
        &mut self.msg
    }

    #[must_use]
    pub(super) fn memory_state(&self) -> &MemoryState {
        &self.memory_state
    }

    #[must_use]
    pub(super) fn memory_state_mut(&mut self) -> &mut MemoryState {
        &mut self.memory_state
    }

    #[must_use]
    pub(super) fn skill_state(&self) -> &SkillState {
        &self.skill_state
    }

    #[must_use]
    pub(super) fn skill_state_mut(&mut self) -> &mut SkillState {
        &mut self.skill_state
    }

    #[must_use]
    pub(super) fn runtime(&self) -> &RuntimeConfig {
        &self.runtime
    }

    #[must_use]
    pub(super) fn runtime_mut(&mut self) -> &mut RuntimeConfig {
        &mut self.runtime
    }

    #[must_use]
    pub(super) fn session(&self) -> &SessionState {
        &self.session
    }

    #[must_use]
    pub(super) fn session_mut(&mut self) -> &mut SessionState {
        &mut self.session
    }

    #[must_use]
    pub(super) fn debug_state(&self) -> &DebugState {
        &self.debug_state
    }

    #[must_use]
    pub(super) fn debug_state_mut(&mut self) -> &mut DebugState {
        &mut self.debug_state
    }

    #[must_use]
    pub(super) fn security(&self) -> &SecurityState {
        &self.security
    }

    #[must_use]
    pub(super) fn security_mut(&mut self) -> &mut SecurityState {
        &mut self.security
    }

    #[must_use]
    pub(super) fn mcp(&self) -> &McpState {
        &self.mcp
    }

    #[must_use]
    pub(super) fn mcp_mut(&mut self) -> &mut McpState {
        &mut self.mcp
    }

    #[must_use]
    pub(super) fn index(&self) -> &IndexState {
        &self.index
    }

    #[must_use]
    pub(super) fn index_mut(&mut self) -> &mut IndexState {
        &mut self.index
    }

    #[must_use]
    pub(super) fn feedback(&self) -> &FeedbackState {
        &self.feedback
    }

    #[must_use]
    pub(super) fn feedback_mut(&mut self) -> &mut FeedbackState {
        &mut self.feedback
    }

    #[must_use]
    pub(super) fn instructions(&self) -> &InstructionState {
        &self.instructions
    }

    #[must_use]
    pub(super) fn instructions_mut(&mut self) -> &mut InstructionState {
        &mut self.instructions
    }

    #[must_use]
    pub(super) fn lifecycle(&self) -> &LifecycleState {
        &self.lifecycle
    }

    #[must_use]
    pub(super) fn lifecycle_mut(&mut self) -> &mut LifecycleState {
        &mut self.lifecycle
    }

    #[must_use]
    pub(super) fn providers(&self) -> &ProviderState {
        &self.providers
    }

    #[must_use]
    pub(super) fn providers_mut(&mut self) -> &mut ProviderState {
        &mut self.providers
    }

    #[must_use]
    pub(super) fn metrics(&self) -> &MetricsState {
        &self.metrics
    }

    #[must_use]
    pub(super) fn metrics_mut(&mut self) -> &mut MetricsState {
        &mut self.metrics
    }

    #[must_use]
    pub(super) fn orchestration(&self) -> &OrchestrationState {
        &self.orchestration
    }

    #[must_use]
    pub(super) fn orchestration_mut(&mut self) -> &mut OrchestrationState {
        &mut self.orchestration
    }

    #[must_use]
    pub(super) fn experiments(&self) -> &ExperimentState {
        &self.experiments
    }

    #[must_use]
    pub(super) fn experiments_mut(&mut self) -> &mut ExperimentState {
        &mut self.experiments
    }

    #[must_use]
    pub(super) fn focus(&self) -> &FocusState {
        &self.focus
    }

    #[must_use]
    pub(super) fn focus_mut(&mut self) -> &mut FocusState {
        &mut self.focus
    }

    #[must_use]
    pub(super) fn sidequest(&self) -> &SidequestState {
        &self.sidequest
    }

    #[must_use]
    pub(super) fn sidequest_mut(&mut self) -> &mut SidequestState {
        &mut self.sidequest
    }

    #[must_use]
    pub(super) fn tool_orchestrator(&self) -> &ToolOrchestrator {
        &self.tool_orchestrator
    }

    #[must_use]
    pub(super) fn tool_orchestrator_mut(&mut self) -> &mut ToolOrchestrator {
        &mut self.tool_orchestrator
    }

    #[must_use]
    pub(super) fn learning_engine(&self) -> &LearningEngine {
        &self.learning_engine
    }

    #[must_use]
    pub(super) fn learning_engine_mut(&mut self) -> &mut LearningEngine {
        &mut self.learning_engine
    }
    #[must_use]
    pub(super) fn compression(&self) -> &CompressionState {
        &self.compression
    }
    #[must_use]
    pub(super) fn compression_mut(&mut self) -> &mut CompressionState {
        &mut self.compression
    }
}
