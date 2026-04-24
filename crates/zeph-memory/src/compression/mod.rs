// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Experience compression spectrum (#3305).
//!
//! This module implements a three-tier memory retrieval policy and a background
//! promotion engine that converts recurring episodic patterns into generated SKILL.md files.
//!
//! # Tiers
//!
//! | Tier | Description |
//! |------|-------------|
//! | `Episodic` | Raw conversation snippets, lowest abstraction, highest token cost. |
//! | `Procedural` | Tool-use patterns and how-to knowledge. |
//! | `Declarative` | Stable facts and reference knowledge. |
//!
//! # Retrieval policy
//!
//! [`RetrievalPolicy`] maps the current remaining token-budget ratio to a subset of
//! tiers. When the budget is ample (> `mid_budget_ratio`) all three tiers are queried;
//! as the budget narrows, cheaper tiers are preferred.
//!
//! # Promotion engine
//!
//! [`promotion::PromotionEngine`] scans a window of recent episodic messages for
//! clustering patterns and promotes qualifying clusters to SKILL.md files.

pub mod promotion;

pub use promotion::{PromotionCandidate, PromotionConfig, PromotionEngine, PromotionInput};

/// The three abstraction levels in the compression spectrum.
///
/// Higher variants are cheaper to retrieve (fewer tokens) but represent a higher
/// abstraction over raw episodic experience.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CompressionLevel {
    /// Raw episodic messages — full fidelity, high token cost.
    Episodic,
    /// Abstracted procedural knowledge (how-to, tool patterns).
    Procedural,
    /// Stable declarative facts and reference material.
    Declarative,
}

impl CompressionLevel {
    /// A relative token-cost factor for budgeting purposes.
    ///
    /// `Episodic = 1.0` (baseline), `Procedural = 0.6`, `Declarative = 0.3`.
    #[must_use]
    pub fn cost_factor(self) -> f32 {
        match self {
            Self::Episodic => 1.0,
            Self::Procedural => 0.6,
            Self::Declarative => 0.3,
        }
    }
}

/// Token-budget-aware tier selector for context assembly.
///
/// Maps a `remaining_ratio` (0.0 = budget exhausted, 1.0 = budget fully available) to
/// the subset of [`CompressionLevel`]s to include in the context recall step.
///
/// # Examples
///
/// ```
/// use zeph_memory::compression::{RetrievalPolicy, CompressionLevel};
///
/// let policy = RetrievalPolicy::default();
/// // Full budget → all three tiers.
/// let levels = policy.select(0.80);
/// assert!(levels.contains(&CompressionLevel::Episodic));
/// assert!(levels.contains(&CompressionLevel::Declarative));
///
/// // Low budget → episodic only.
/// let levels = policy.select(0.10);
/// assert_eq!(levels, &[CompressionLevel::Episodic]);
/// ```
#[derive(Debug, Clone, Copy)]
pub struct RetrievalPolicy {
    /// Below this ratio only `Episodic` recall is attempted. Default: `0.20`.
    pub low_budget_ratio: f32,
    /// Below this ratio `Episodic + Procedural` recall is attempted. Default: `0.50`.
    pub mid_budget_ratio: f32,
}

impl Default for RetrievalPolicy {
    fn default() -> Self {
        Self {
            low_budget_ratio: 0.20,
            mid_budget_ratio: 0.50,
        }
    }
}

impl RetrievalPolicy {
    /// Select which compression levels should be included for `remaining_ratio`.
    ///
    /// | `remaining_ratio` | Levels returned |
    /// |-------------------|-----------------|
    /// | `< low_budget_ratio` | `[Episodic]` |
    /// | `< mid_budget_ratio` | `[Episodic, Procedural]` |
    /// | `≥ mid_budget_ratio` | `[Episodic, Procedural, Declarative]` |
    #[tracing::instrument(name = "memory.compression.select", skip_all, fields(remaining_ratio))]
    pub fn select(&self, remaining_ratio: f32) -> &'static [CompressionLevel] {
        if remaining_ratio < self.low_budget_ratio {
            &[CompressionLevel::Episodic]
        } else if remaining_ratio < self.mid_budget_ratio {
            &[CompressionLevel::Episodic, CompressionLevel::Procedural]
        } else {
            &[
                CompressionLevel::Episodic,
                CompressionLevel::Procedural,
                CompressionLevel::Declarative,
            ]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compression_level_cost_factors() {
        assert!((CompressionLevel::Episodic.cost_factor() - 1.0).abs() < 1e-6);
        assert!((CompressionLevel::Procedural.cost_factor() - 0.6).abs() < 1e-6);
        assert!((CompressionLevel::Declarative.cost_factor() - 0.3).abs() < 1e-6);
    }

    #[test]
    fn retrieval_policy_full_budget() {
        let policy = RetrievalPolicy::default();
        let levels = policy.select(0.80);
        assert!(levels.contains(&CompressionLevel::Episodic));
        assert!(levels.contains(&CompressionLevel::Procedural));
        assert!(levels.contains(&CompressionLevel::Declarative));
    }

    #[test]
    fn retrieval_policy_mid_budget() {
        let policy = RetrievalPolicy::default();
        let levels = policy.select(0.35);
        assert!(levels.contains(&CompressionLevel::Episodic));
        assert!(levels.contains(&CompressionLevel::Procedural));
        assert!(!levels.contains(&CompressionLevel::Declarative));
    }

    #[test]
    fn retrieval_policy_low_budget() {
        let policy = RetrievalPolicy::default();
        let levels = policy.select(0.10);
        assert_eq!(levels, &[CompressionLevel::Episodic]);
    }

    #[test]
    fn retrieval_policy_boundary_at_low() {
        let policy = RetrievalPolicy::default();
        // Exactly at low_budget_ratio — mid tier.
        let levels = policy.select(0.20);
        assert!(levels.contains(&CompressionLevel::Procedural));
    }

    #[test]
    fn retrieval_policy_boundary_at_mid() {
        let policy = RetrievalPolicy::default();
        // Exactly at mid_budget_ratio — all tiers.
        let levels = policy.select(0.50);
        assert!(levels.contains(&CompressionLevel::Declarative));
    }
}
