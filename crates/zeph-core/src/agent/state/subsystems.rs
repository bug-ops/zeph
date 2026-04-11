// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Advanced memory subsystem runtime state and configuration.
//!
//! [`MemorySubsystemState`] groups configuration and runtime state for three advanced memory
//! subsystems: `TiMem` (temporal-hierarchical memory tree), `autoDream` (background consolidation),
//! and `MagicDocs` (document context injection). Also tracks the background tree consolidation
//! task handle.

/// `TiMem` tree config, `autoDream`, `MagicDocs`, and microcompact subsystem state.
///
/// These subsystems are initialized together during agent construction and managed as a group
/// across the agent lifetime. Isolating them in their own struct makes it clear that they are
/// advanced features separate from core persistence and compaction.
pub(crate) struct MemorySubsystemState {
    /// `TiMem` temporal-hierarchical memory tree configuration (#2262).
    pub(crate) tree_config: zeph_config::TreeConfig,
    /// Background tree consolidation loop handle — kept alive for the agent's lifetime (#2262).
    ///
    /// `None` when tree consolidation is disabled or memory is not initialized.
    pub(crate) tree_consolidation_handle: Option<tokio::task::JoinHandle<()>>,
    /// Time-based microcompact configuration (#2699).
    pub(crate) microcompact_config: zeph_config::MicrocompactConfig,
    /// autoDream configuration (#2697).
    pub(crate) autodream_config: zeph_config::AutoDreamConfig,
    /// autoDream session state (#2697). Tracks session count and last consolidation time.
    pub(crate) autodream: super::super::autodream::AutoDreamState,
    /// `MagicDocs` configuration (#2702).
    pub(crate) magic_docs_config: zeph_config::MagicDocsConfig,
    /// `MagicDocs` session state (#2702). Tracks registered doc paths and last update turn.
    pub(crate) magic_docs: super::super::magic_docs::MagicDocsState,
}

impl Default for MemorySubsystemState {
    fn default() -> Self {
        Self {
            tree_config: zeph_config::TreeConfig::default(),
            tree_consolidation_handle: None,
            microcompact_config: zeph_config::MicrocompactConfig::default(),
            autodream_config: zeph_config::AutoDreamConfig::default(),
            autodream: super::super::autodream::AutoDreamState::new(),
            magic_docs_config: zeph_config::MagicDocsConfig::default(),
            magic_docs: super::super::magic_docs::MagicDocsState::new(),
        }
    }
}
