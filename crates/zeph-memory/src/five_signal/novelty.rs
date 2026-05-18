// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// Novelty signal computer.
///
/// Computes `novelty = exp(-λ × days_since_agent_init)` where
/// `days_since_agent_init = (fact.created_at - session_start) / 86400`.
///
/// **Direction:** higher novelty = fact was created close to session start; lower novelty =
/// fact was created late in a long session. This corrects stale-episodic interference —
/// facts that accumulated late in long sessions and pollute recall despite weak relevance.
///
/// Facts created before session start receive `days = 0.0` → novelty = 1.0.
pub struct NoveltyComputer {
    session_start: i64,
    /// Decay rate λ in `exp(-λ × days)`. Default: `0.1`.
    decay_rate: f64,
}

impl NoveltyComputer {
    /// Create a new `NoveltyComputer`.
    ///
    /// # Parameters
    ///
    /// - `session_start`: Unix timestamp (seconds) when the agent session began.
    /// - `decay_rate`: λ in `exp(-λ × days)`. Larger values penalize late-session facts more strongly.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_memory::five_signal::novelty::NoveltyComputer;
    ///
    /// let start = 1_700_000_000_i64;
    /// let computer = NoveltyComputer::new(start, 0.1);
    ///
    /// // Fact created at session start → novelty = 1.0
    /// assert!((computer.compute(start) - 1.0).abs() < 1e-9);
    ///
    /// // Fact created 10 days into the session → novelty ≈ exp(-1.0)
    /// let ten_days_later = start + 10 * 86400;
    /// let expected = (-1.0_f64).exp();
    /// assert!((computer.compute(ten_days_later) - expected).abs() < 1e-9);
    /// ```
    #[must_use]
    pub fn new(session_start: i64, decay_rate: f64) -> Self {
        Self {
            session_start,
            decay_rate,
        }
    }

    /// Compute the novelty score for a fact with the given `created_at` timestamp.
    ///
    /// Returns a value in `(0.0, 1.0]`. Facts created before or at session start return `1.0`.
    #[must_use]
    #[inline]
    pub fn compute(&self, fact_created_at: i64) -> f64 {
        let _span = tracing::info_span!("memory.five_signal.novelty.compute").entered();
        #[expect(clippy::cast_precision_loss)]
        let days = ((fact_created_at - self.session_start).max(0) as f64) / 86_400.0;
        (-self.decay_rate * days).exp()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION_START: i64 = 1_700_000_000;

    #[test]
    fn at_session_start_is_one() {
        let c = NoveltyComputer::new(SESSION_START, 0.1);
        assert!((c.compute(SESSION_START) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn before_session_start_is_one() {
        let c = NoveltyComputer::new(SESSION_START, 0.1);
        assert!((c.compute(SESSION_START - 86_400) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn ten_days_later() {
        let c = NoveltyComputer::new(SESSION_START, 0.1);
        let ten_days = SESSION_START + 10 * 86_400;
        let expected = (-1.0_f64).exp();
        assert!((c.compute(ten_days) - expected).abs() < 1e-9);
    }

    #[test]
    fn monotone_decreasing() {
        let c = NoveltyComputer::new(SESSION_START, 0.1);
        let early = c.compute(SESSION_START + 86_400);
        let late = c.compute(SESSION_START + 10 * 86_400);
        assert!(early > late, "novelty must decrease with time");
    }
}
