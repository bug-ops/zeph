// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Re-exports of [`zeph_core::metrics`] types used by the TUI dashboard.
//!
//! Import from this module instead of `zeph_core::metrics` directly so that
//! the TUI dependency surface on `zeph_core` stays centralised and refactors
//! only need to touch this file.
//!
//! # Examples
//!
//! ```rust
//! use zeph_tui::metrics::{MetricsCollector, MetricsSnapshot};
//!
//! let (collector, mut _rx) = MetricsCollector::new();
//! // collector.update(|m| { /* modify metrics */ });
//! ```

pub use zeph_common::SecurityEventCategory;
pub use zeph_core::goal::{GoalSnapshot, GoalStatus};
pub use zeph_core::metrics::{
    CategoryScore, ClassifierMetricsSnapshot, McpServerConnectionStatus, McpServerStatus,
    MetricsCollector, MetricsSnapshot, ProbeCategory, ProbeVerdict, SecurityEvent, SkillConfidence,
    SubAgentMetrics, TaskGraphSnapshot, TaskMetricsSnapshot, TaskSnapshotRow,
};
