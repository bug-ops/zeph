// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Audit signal types emitted by sanitizer subsystems for trajectory-level accumulation.
//!
//! These types are consumed by `TrajectoryRiskAccumulator` in `zeph-memory` to maintain
//! a rolling per-session risk score without coupling the sanitizer to the memory crate.
//!
//! The canonical definitions live in [`zeph_common::audit`]; this module re-exports them
//! for backward compatibility with existing `zeph_sanitizer::audit` import paths.

pub use zeph_common::audit::{AuditSignal, AuditSignalType, Severity};
