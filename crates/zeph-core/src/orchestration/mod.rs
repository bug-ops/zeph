// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Re-export shim: preserves `crate::orchestration::*` paths for internal consumers.
//!
//! All orchestration logic now lives in the `zeph-orchestration` crate.

pub use zeph_orchestration::*;
// Re-export submodules so `crate::orchestration::graph::*` qualified paths continue to work.
pub use zeph_orchestration::{aggregator, command, dag, error, graph, planner, router, scheduler};
// Re-export OrchestrationConfig to preserve the `crate::orchestration::OrchestrationConfig` path.
pub use crate::config::OrchestrationConfig;
