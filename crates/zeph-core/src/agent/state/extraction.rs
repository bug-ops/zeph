// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Knowledge graph extraction and semantic labeling state for the agent's memory subsystem.
//!
//! [`MemoryExtractionState`] groups fields that control how the agent extracts structured
//! knowledge: graph extraction configuration, the RPE (Retrieval-Path Expansion) router,
//! document config, and semantic classification configs (persona, trajectory, category).

/// Graph extraction config, RPE router, document config, and semantic classification configs.
///
/// These fields are primarily accessed together in graph extraction passes and
/// semantic labeling paths. Isolating them reduces cognitive load when reasoning about
/// knowledge graph and classification logic.
#[derive(Default)]
pub(crate) struct MemoryExtractionState {
    /// Document indexing and chunking configuration.
    pub(crate) document_config: crate::config::DocumentConfig,
    /// Knowledge graph extraction configuration.
    pub(crate) graph_config: crate::config::GraphConfig,
    /// D-MEM RPE router. `Some` when `graph_config.rpe.enabled = true`.
    ///
    /// Protected by `std::sync::Mutex` for non-async access from `maybe_spawn_graph_extraction`.
    pub(crate) rpe_router: Option<std::sync::Mutex<zeph_memory::RpeRouter>>,
    /// Goal text for the current user turn, derived from raw user input (#2483).
    ///
    /// Passed to A-MAC admission control to enable goal-conditioned write gating.
    /// Reset at the start of each user turn. `None` only before the first user message.
    pub(crate) goal_text: Option<String>,
    /// Persona memory configuration (#2461).
    pub(crate) persona_config: zeph_config::PersonaConfig,
    /// Trajectory-informed memory configuration (#2498).
    pub(crate) trajectory_config: zeph_config::TrajectoryConfig,
    /// Category-aware memory configuration (#2428).
    pub(crate) category_config: zeph_config::CategoryConfig,
}

impl MemoryExtractionState {
    /// Apply an updated graph configuration, reinitializing the RPE router if needed.
    ///
    /// Rebuilds the RPE router when `config.rpe.enabled = true`, clears it otherwise.
    /// Emits a warning when graph-memory is enabled because PII redaction is not yet implemented.
    pub(crate) fn apply_graph_config(&mut self, config: crate::config::GraphConfig) {
        if config.enabled {
            tracing::warn!(
                "graph-memory is enabled: extracted entities are stored without PII redaction. \
                 Do not use with sensitive personal data until redaction is implemented."
            );
        }
        if config.rpe.enabled {
            self.rpe_router = Some(std::sync::Mutex::new(zeph_memory::RpeRouter::new(
                config.rpe.threshold,
                config.rpe.max_skip_turns,
            )));
        } else {
            self.rpe_router = None;
        }
        self.graph_config = config;
    }
}
