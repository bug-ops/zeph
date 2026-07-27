// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared audit signal types used by both the sanitizer and memory subsystems.
//!
//! Defined here in `zeph-common` to eliminate the duplicate definitions that previously
//! existed in `zeph-sanitizer::audit` and `zeph-memory::shadow`. Both crates re-export
//! from this module.

/// Signal type emitted by a sanitizer subsystem.
///
/// Variants correspond to the four signal classes defined in spec 004-19, FR-007.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
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
#[non_exhaustive]
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
/// Carries the minimum information needed by `TrajectoryRiskAccumulator::ingest`.
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
