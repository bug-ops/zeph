// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Audit signal types emitted by sanitizer subsystems for trajectory-level accumulation.
//!
//! These types are consumed by `TrajectoryRiskAccumulator` in `zeph-memory` to maintain
//! a rolling per-session risk score without coupling the sanitizer to the memory crate.

/// Signal type emitted by a sanitizer subsystem.
///
/// Variants correspond to the four signal classes defined in spec 004-16, FR-007.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuditSignalType {
    /// A policy gate denied or flagged an operation.
    PolicyViolation,
    /// A prompt-injection pattern was detected in untrusted content.
    PromptInjectionPattern,
    /// An anomalous tool-call chain was observed (e.g., rapid multi-tool escalation).
    ToolChainAnomaly,
    /// LLM response confidence dropped significantly between turns.
    ConfidenceDrop,
}

/// Severity level for an [`AuditSignalType`].
///
/// Mapped to a numeric multiplier by `TrajectorySeverityMultipliers`:
/// `Low → 0.5`, `Medium → 1.0`, `High → 2.0` (defaults).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Severity {
    /// Minor or likely-benign signal.
    Low,
    /// Moderate concern; warrants accumulation.
    Medium,
    /// Strong indicator; highest multiplier.
    High,
}

/// A single audit event emitted by a sanitizer subsystem.
///
/// Carry the minimum information needed by `TrajectoryRiskAccumulator::ingest`.
/// No heap allocation — both fields are `Copy`.
#[derive(Debug, Clone, Copy)]
pub struct AuditSignal {
    /// Category of the detected signal.
    pub signal_type: AuditSignalType,
    /// Severity of the detected signal.
    pub severity: Severity,
}

impl AuditSignal {
    /// Construct a new audit signal.
    #[must_use]
    pub const fn new(signal_type: AuditSignalType, severity: Severity) -> Self {
        Self {
            signal_type,
            severity,
        }
    }
}
