// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! ORCH-style deterministic multi-agent ensemble-merge (arXiv:2602.01797), scoped to
//! [`crate::verifier::PlanVerifier`] gap-severity verification.
//!
//! N configured provider members independently classify the same completed-task output in
//! parallel; a pure, deterministic binary-majority merge ([`merge::merge`]) produces the
//! single `VerificationResult` the existing `should_replan` gate already consumes. See
//! `specs/073-orch-ensemble-merge/spec.md` for the full design.
//!
//! Reuses the `Gap` type from [`crate::verifier`].

pub mod merge;
pub mod tracker;
pub mod verifier;

pub use merge::{Ballot, MergeOutcome, merge};
pub use tracker::EnsembleTracker;
pub use verifier::{EnsembleAttempt, EnsembleVerifier, MemberUsage};
