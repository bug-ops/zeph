// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `AgentRuntime` aggregator struct — runtime configuration, lifecycle, providers,
//! metrics, debug instrumentation, and instruction hot-reload.

use super::{
    DebugState, InstructionState, LifecycleState, MetricsState, ProviderState, RuntimeConfig,
};

/// Aggregator for runtime configuration, lifecycle, providers, metrics, debug instrumentation,
/// and instruction hot-reload.
///
/// Borrowable independently of [`Services`](super::services::Services) and conversation core.
/// All fields are `pub(crate)`.
///
/// The inner `config` field holds the existing [`RuntimeConfig`] (formerly named `runtime` on
/// `Agent<C>`). Renaming it to `config` avoids the awkward `self.runtime.runtime` path.
pub(crate) struct AgentRuntime {
    /// Runtime configuration snapshot (formerly `Agent::runtime`).
    pub(crate) config: RuntimeConfig,
    pub(crate) lifecycle: LifecycleState,
    pub(crate) providers: ProviderState,
    pub(crate) metrics: MetricsState,
    pub(crate) debug: DebugState,
    pub(crate) instructions: InstructionState,
    /// Session-scoped ephemeral plugin directories loaded via `--plugin-url`.
    ///
    /// Holds the `TempDir` handles so extracted archives remain on disk for the session.
    /// Dropped automatically when the agent is dropped, cleaning up all ephemeral plugins.
    pub(crate) ephemeral_plugins: Vec<tempfile::TempDir>,
}
