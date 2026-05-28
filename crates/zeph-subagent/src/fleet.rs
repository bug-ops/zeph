// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Fleet registry abstraction for sub-agent session tracking.
//!
//! [`FleetRegistry`] is a narrow trait that decouples `zeph-subagent` from the
//! `zeph-memory` `SqliteStore`. The concrete implementation lives in `zeph-core`
//! and is injected via `SubAgentManager::set_fleet_registry`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Terminal lifecycle status of an agent session visible in the fleet dashboard.
///
/// Only terminal states are represented because [`FleetRegistry::mark_terminal`] is only
/// called when a session ends. Active sessions are registered via
/// [`FleetRegistry::register_active`].
///
/// Mirrors the terminal variants of `zeph_memory::SessionStatus` without creating a
/// dependency on that crate.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FleetSessionStatus {
    /// Session ended normally.
    Completed,
    /// Session ended due to an unrecoverable error.
    Failed,
    /// Session was cancelled by the user.
    Cancelled,
}

/// Minimal data needed to register a sub-agent session in the fleet dashboard.
#[derive(Debug, Clone)]
pub struct FleetSessionInfo {
    /// Stable session ID (UUID string).
    pub id: String,
    /// Human-readable agent name (from the definition).
    pub agent_name: String,
    /// ISO-8601 UTC timestamp of session start.
    pub started_at: String,
}

/// Trait that abstracts fleet session persistence for `SubAgentManager`.
///
/// Implementors must be `Send + Sync` and handle their own internal error recovery;
/// the manager logs failures at `warn` level but never propagates them to the caller.
pub trait FleetRegistry: Send + Sync {
    /// Register or update a sub-agent session as active.
    ///
    /// Called once at sub-agent spawn time. Fire-and-forget: errors are logged
    /// by the manager and do not abort the spawn.
    fn register_active<'a>(
        &'a self,
        info: &'a FleetSessionInfo,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

    /// Mark a sub-agent session as terminal.
    ///
    /// Called when a sub-agent finishes (collect) or is cancelled.
    fn mark_terminal<'a>(
        &'a self,
        session_id: &'a str,
        status: FleetSessionStatus,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

/// Shared, type-erased fleet registry injected into the manager.
pub type SharedFleetRegistry = Arc<dyn FleetRegistry>;
