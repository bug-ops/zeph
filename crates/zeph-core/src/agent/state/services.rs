// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `Services` aggregator struct — background subsystems borrowable independently of
//! `AgentRuntime` and conversation core.

use super::{
    CompressionState, ExperimentState, FeedbackState, IndexState, McpState, MemoryState,
    OrchestrationState, SecurityState, SessionState, SkillState, ToolState,
};
use crate::agent::{focus, learning_engine, sidequest};

/// Aggregator for background subsystems borrowable independently of [`AgentRuntime`] and
/// conversation core.
///
/// All fields are `pub(crate)` so existing call-site patterns inside `agent/*.rs` keep
/// compiling after the mechanical field-path rewrite. Field order matches the prior
/// `Agent<C>` declaration order so drop order is preserved.
///
/// [`AgentRuntime`]: super::runtime::AgentRuntime
pub(crate) struct Services {
    pub(crate) memory: MemoryState,
    pub(crate) skill: SkillState,
    pub(crate) learning_engine: learning_engine::LearningEngine,
    pub(crate) feedback: FeedbackState,
    pub(crate) mcp: McpState,
    pub(crate) index: IndexState,
    pub(crate) session: SessionState,
    pub(crate) security: SecurityState,
    pub(crate) experiments: ExperimentState,
    pub(crate) compression: CompressionState,
    pub(crate) orchestration: OrchestrationState,
    pub(crate) focus: focus::FocusState,
    pub(crate) sidequest: sidequest::SidequestState,
    pub(crate) tool_state: ToolState,

    // Optional service singletons
    /// Goal lifecycle store and accounting, initialised at startup when `[goals] enabled = true`.
    pub(crate) goal_accounting: Option<std::sync::Arc<crate::goal::GoalAccounting>>,

    /// MARCH self-check pipeline, built at startup and rebuilt on provider swap.
    pub(crate) quality: Option<std::sync::Arc<crate::quality::SelfCheckPipeline>>,
    /// Proactive world-knowledge explorer (#3320).
    ///
    /// `Some` when `config.skills.proactive_exploration.enabled = true`.
    pub(crate) proactive_explorer:
        Option<std::sync::Arc<zeph_skills::proactive::ProactiveExplorer>>,
    /// Experience compression spectrum promotion engine (#3305).
    ///
    /// `Some` when `config.memory.compression_spectrum.enabled = true`.
    pub(crate) promotion_engine:
        Option<std::sync::Arc<zeph_memory::compression::promotion::PromotionEngine>>,

    /// TACO rule-based compressor, kept alive for hit-count flushing during `maybe_autodream`.
    ///
    /// `Some` when `config.tools.compression.enabled = true` and a DB pool is available.
    pub(crate) taco_compressor: Option<std::sync::Arc<zeph_tools::RuleBasedCompressor>>,

    /// Speculative tool execution engine (#3636).
    ///
    /// `Some` when `config.tools.speculative.mode != Off` and not in bare mode.
    pub(crate) speculation_engine:
        Option<std::sync::Arc<crate::agent::speculative::SpeculationEngine>>,
}
