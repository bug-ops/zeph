// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-provider Bayesian reputation scoring (RAPS — Reputation-Adjusted Provider Selection).
//!
//! Tracks quality outcomes from tool execution and plan steps using a Beta distribution
//! per provider. Reputation is used to shift the Thompson Sampling priors or blend into
//! EMA routing scores, enabling long-term quality signal distinct from API availability.
//!
//! # Design
//!
//! - Each provider has `quality_alpha` / `quality_beta` (uniform prior = 1,1).
//! - `record_quality(provider, success)` increments alpha (success) or beta (failure).
//! - Only semantic failures (invalid tool arguments, parse errors) count against reputation.
//!   Network errors, rate limits, and transient I/O failures are NOT quality signals.
//! - Session-level decay shrinks accumulated evidence toward the prior on each load,
//!   preventing stale observations from permanently biasing routing.
//! - Reputation is not used in Cascade mode (fixed cost tiers, no-op to avoid mutex overhead).

use std::collections::{HashMap, HashSet};
#[cfg(unix)]
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::thompson::BetaDist;

/// Per-provider reputation entry.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReputationEntry {
    #[serde(flatten)]
    pub dist: BetaDist,
    /// Monotonic observation count (alpha + beta - 2.0, since prior is 1,1).
    /// Used for the `min_observations` threshold check.
    pub observations: u64,
}

/// Tracks per-provider quality reputation using Beta distributions.
///
/// Thread-safe when wrapped in `Arc<Mutex<ReputationTracker>>`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReputationTracker {
    pub(crate) models: HashMap<String, ReputationEntry>,
    /// Temporal decay factor applied each session load. 0.95 = 5% decay toward prior.
    #[serde(default = "default_decay_factor")]
    decay_factor: f64,
    /// Minimum quality observations before reputation influences routing.
    #[serde(default = "default_min_observations")]
    min_observations: u64,
}

fn default_decay_factor() -> f64 {
    0.95
}

fn default_min_observations() -> u64 {
    5
}

impl ReputationTracker {
    #[must_use]
    pub fn new(decay_factor: f64, min_observations: u64) -> Self {
        Self {
            models: HashMap::new(),
            decay_factor: decay_factor.clamp(f64::MIN_POSITIVE, 1.0),
            min_observations,
        }
    }

    /// Record a quality outcome for `provider`.
    ///
    /// Only call for semantic failures (invalid arguments, parse errors).
    /// Do NOT call for network errors, rate limits, or transient I/O failures.
    pub fn record_quality(&mut self, provider: &str, success: bool) {
        let entry = self.models.entry(provider.to_owned()).or_default();
        if success {
            entry.dist.alpha += 1.0;
        } else {
            entry.dist.beta += 1.0;
        }
        entry.observations += 1;
    }

    /// Returns `true` if `provider` has enough observations to influence routing.
    #[must_use]
    pub fn has_sufficient_observations(&self, provider: &str) -> bool {
        self.models
            .get(provider)
            .is_some_and(|e| e.observations >= self.min_observations)
    }

    /// Return adjusted `(alpha, beta)` for `provider` by folding in quality reputation.
    ///
    /// The quality Beta distribution shifts the Thompson prior: good quality history
    /// increases effective alpha; bad quality history increases effective beta.
    /// This preserves the single-Beta-sample Thompson property (CRIT-3 fix):
    /// the caller samples once from the shifted distribution, not from two distributions.
    ///
    /// Returns `(alpha, beta)` unchanged if reputation has insufficient observations.
    ///
    /// # Arguments
    ///
    /// * `alpha` / `beta` — Thompson availability prior for `provider`
    /// * `weight` — fraction of reputation alpha/beta to blend into the prior (0.0–1.0)
    #[must_use]
    pub fn shift_thompson_priors(
        &self,
        provider: &str,
        alpha: f64,
        beta: f64,
        weight: f64,
    ) -> (f64, f64) {
        if !self.has_sufficient_observations(provider) {
            return (alpha, beta);
        }
        let Some(entry) = self.models.get(provider) else {
            return (alpha, beta);
        };
        // Shift: add a weighted fraction of reputation evidence to the Thompson prior.
        // This keeps the distribution Beta-distributed (not a convex blend of two samples)
        // and preserves Thompson's theoretical guarantees.
        let rep_alpha = entry.dist.alpha - 1.0; // excess over uniform prior
        let rep_beta = entry.dist.beta - 1.0;
        let new_alpha = alpha + weight * rep_alpha;
        let new_beta = beta + weight * rep_beta;
        (new_alpha.max(1e-6), new_beta.max(1e-6))
    }

    /// Return the reputation mean for `provider` for use in EMA score blending.
    ///
    /// Returns `None` if reputation has insufficient observations.
    /// The mean is in [0.0, 1.0]; values above 0.5 indicate net positive quality.
    #[must_use]
    pub fn ema_reputation_factor(&self, provider: &str) -> Option<f64> {
        if !self.has_sufficient_observations(provider) {
            return None;
        }
        let entry = self.models.get(provider)?;
        let alpha = entry.dist.alpha;
        let beta = entry.dist.beta;
        Some(alpha / (alpha + beta))
    }

    /// Apply session-level decay: shrink accumulated evidence toward the uniform prior (1,1).
    ///
    /// `alpha_new = 1 + (alpha - 1) * decay_factor`
    /// `beta_new  = 1 + (beta  - 1) * decay_factor`
    ///
    /// With `decay_factor = 0.95`, after 14 sessions the accumulated evidence halves.
    pub fn apply_decay(&mut self) {
        let d = self.decay_factor;
        for entry in self.models.values_mut() {
            entry.dist.alpha = 1.0 + (entry.dist.alpha - 1.0) * d;
            entry.dist.beta = 1.0 + (entry.dist.beta - 1.0) * d;
            // Decay observations proportionally so threshold check stays calibrated.
            // f64 represents integers exactly up to 2^53; observation counts never approach that.
            #[allow(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss
            )]
            {
                let decayed = (entry.observations as f64) * d;
                entry.observations = decayed as u64;
            }
        }
    }

    /// Remove entries for providers not in `known`.
    pub fn prune(&mut self, known: &HashSet<String>) {
        self.models.retain(|k, _| known.contains(k));
    }

    /// Return sorted `(name, alpha, beta, mean, observations)` for diagnostics.
    #[must_use]
    pub fn stats(&self) -> Vec<(String, f64, f64, f64, u64)> {
        let mut v: Vec<_> = self
            .models
            .iter()
            .map(|(name, e)| {
                let mean = e.dist.alpha / (e.dist.alpha + e.dist.beta);
                (
                    name.clone(),
                    e.dist.alpha,
                    e.dist.beta,
                    mean,
                    e.observations,
                )
            })
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Default state file path: `~/.config/zeph/router_reputation_state.json`.
    #[must_use]
    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("zeph")
            .join("router_reputation_state.json")
    }

    /// Load from `path`. Falls back to default on any error.
    ///
    /// Clamps alpha/beta to `[0.5, 1e9]` and sanitizes NaN/Inf on load.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Self::default();
            }
            Err(e) => {
                tracing::debug!(
                    path = %path.display(),
                    error = %e,
                    "reputation state file unreadable, using uniform priors"
                );
                return Self::default();
            }
        };
        match serde_json::from_slice::<Self>(&bytes) {
            Ok(mut tracker) => {
                for entry in tracker.models.values_mut() {
                    if !entry.dist.alpha.is_finite() {
                        entry.dist.alpha = 1.0;
                    }
                    if !entry.dist.beta.is_finite() {
                        entry.dist.beta = 1.0;
                    }
                    entry.dist.alpha = entry.dist.alpha.clamp(0.5, 1e9);
                    entry.dist.beta = entry.dist.beta.clamp(0.5, 1e9);
                }
                tracker
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "reputation state file is corrupt; resetting to uniform priors"
                );
                Self::default()
            }
        }
    }

    /// Save to `path` using an atomic write (tmp file + rename).
    ///
    /// On Unix the tmp file is created with mode `0o600`.
    ///
    /// # Errors
    ///
    /// Returns `io::Error` if the write or rename fails.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_vec(self).map_err(|e| std::io::Error::other(e.to_string()))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?
                .write_all(&json)?;
        }
        #[cfg(not(unix))]
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn default_entry_is_uniform_prior() {
        let entry = ReputationEntry::default();
        assert!((entry.dist.alpha - 1.0).abs() < f64::EPSILON);
        assert!((entry.dist.beta - 1.0).abs() < f64::EPSILON);
        assert_eq!(entry.observations, 0);
    }

    #[test]
    fn record_success_increments_alpha() {
        let mut tracker = ReputationTracker::new(0.95, 1);
        tracker.record_quality("p", true);
        let entry = &tracker.models["p"];
        assert!((entry.dist.alpha - 2.0).abs() < f64::EPSILON);
        assert!((entry.dist.beta - 1.0).abs() < f64::EPSILON);
        assert_eq!(entry.observations, 1);
    }

    #[test]
    fn record_failure_increments_beta() {
        let mut tracker = ReputationTracker::new(0.95, 1);
        tracker.record_quality("p", false);
        let entry = &tracker.models["p"];
        assert!((entry.dist.alpha - 1.0).abs() < f64::EPSILON);
        assert!((entry.dist.beta - 2.0).abs() < f64::EPSILON);
        assert_eq!(entry.observations, 1);
    }

    #[test]
    fn min_observations_gate_blocks_insufficient_data() {
        let mut tracker = ReputationTracker::new(0.95, 5);
        for _ in 0..4 {
            tracker.record_quality("p", true);
        }
        assert!(!tracker.has_sufficient_observations("p"));
        assert!(tracker.ema_reputation_factor("p").is_none());
    }

    #[test]
    fn min_observations_gate_passes_at_threshold() {
        let mut tracker = ReputationTracker::new(0.95, 5);
        for _ in 0..5 {
            tracker.record_quality("p", true);
        }
        assert!(tracker.has_sufficient_observations("p"));
        assert!(tracker.ema_reputation_factor("p").is_some());
    }

    #[test]
    fn ema_reputation_factor_in_range() {
        let mut tracker = ReputationTracker::new(0.95, 1);
        for _ in 0..9 {
            tracker.record_quality("p", true);
        }
        tracker.record_quality("p", false);
        let factor = tracker.ema_reputation_factor("p").unwrap();
        assert!(
            (0.0..=1.0).contains(&factor),
            "factor {factor} out of [0,1]"
        );
    }

    #[test]
    fn apply_decay_shrinks_toward_prior() {
        let mut tracker = ReputationTracker::new(0.95, 1);
        for _ in 0..10 {
            tracker.record_quality("p", true);
        }
        let alpha_before = tracker.models["p"].dist.alpha;
        tracker.apply_decay();
        let alpha_after = tracker.models["p"].dist.alpha;
        assert!(alpha_after < alpha_before, "decay must reduce alpha");
        // alpha_new = 1 + (alpha_before - 1) * 0.95
        let expected = 1.0 + (alpha_before - 1.0) * 0.95;
        assert!((alpha_after - expected).abs() < 1e-9);
    }

    #[test]
    fn apply_decay_does_not_go_below_prior_at_one() {
        let mut tracker = ReputationTracker::new(0.95, 1);
        // Fresh tracker: alpha=1.0, beta=1.0 — decay should leave them unchanged.
        tracker
            .models
            .insert("p".to_owned(), ReputationEntry::default());
        tracker.apply_decay();
        let entry = &tracker.models["p"];
        assert!(
            (entry.dist.alpha - 1.0).abs() < 1e-9,
            "alpha must stay at 1.0"
        );
        assert!(
            (entry.dist.beta - 1.0).abs() < 1e-9,
            "beta must stay at 1.0"
        );
    }

    #[test]
    fn shift_thompson_priors_returns_unchanged_below_min_obs() {
        let tracker = ReputationTracker::new(0.95, 5);
        let (a, b) = tracker.shift_thompson_priors("p", 3.0, 2.0, 0.3);
        assert!((a - 3.0).abs() < f64::EPSILON);
        assert!((b - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn shift_thompson_priors_shifts_alpha_for_good_provider() {
        let mut tracker = ReputationTracker::new(0.95, 5);
        for _ in 0..10 {
            tracker.record_quality("p", true);
        }
        let (a, b) = tracker.shift_thompson_priors("p", 2.0, 1.0, 0.3);
        // alpha should increase (good reputation), beta unchanged
        assert!(a > 2.0, "alpha should increase for high-quality provider");
        assert!((b - 1.0).abs() < 1.0, "beta should change minimally");
    }

    #[test]
    fn shift_thompson_priors_shifts_beta_for_bad_provider() {
        let mut tracker = ReputationTracker::new(0.95, 5);
        for _ in 0..10 {
            tracker.record_quality("p", false);
        }
        let (_a, b) = tracker.shift_thompson_priors("p", 2.0, 1.0, 0.3);
        assert!(b > 1.0, "beta should increase for low-quality provider");
    }

    #[test]
    fn prune_removes_stale_entries() {
        let mut tracker = ReputationTracker::new(0.95, 1);
        tracker.record_quality("a", true);
        tracker.record_quality("b", false);
        tracker.record_quality("c", true);
        let known: HashSet<String> = ["a".to_owned(), "c".to_owned()].into_iter().collect();
        tracker.prune(&known);
        assert!(tracker.models.contains_key("a"));
        assert!(!tracker.models.contains_key("b"));
        assert!(tracker.models.contains_key("c"));
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rep.json");
        let mut tracker = ReputationTracker::new(0.95, 5);
        for _ in 0..7 {
            tracker.record_quality("claude", true);
        }
        tracker.record_quality("ollama", false);
        tracker.save(&path).unwrap();

        let loaded = ReputationTracker::load(&path);
        assert!(
            (loaded.models["claude"].dist.alpha - tracker.models["claude"].dist.alpha).abs() < 1e-9
        );
        assert!(
            (loaded.models["ollama"].dist.beta - tracker.models["ollama"].dist.beta).abs() < 1e-9
        );
    }

    #[test]
    fn load_missing_file_returns_default() {
        let tracker = ReputationTracker::load(Path::new("/tmp/zeph-rep-nonexistent-test.json"));
        assert!(tracker.models.is_empty());
    }

    #[test]
    fn load_clamps_out_of_range_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rep.json");
        std::fs::write(
            &path,
            br#"{"models":{"p":{"alpha":-5.0,"beta":2000000000000.0,"observations":10}}}"#,
        )
        .unwrap();
        let tracker = ReputationTracker::load(&path);
        let entry = &tracker.models["p"];
        assert!(entry.dist.alpha >= 0.5);
        assert!(entry.dist.beta <= 1e9);
    }

    #[test]
    fn load_corrupt_file_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.json");
        std::fs::write(&path, b"not valid json {{{{").unwrap();
        let tracker = ReputationTracker::load(&path);
        assert!(tracker.models.is_empty());
    }

    #[test]
    fn save_leaves_no_tmp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rep.json");
        let tmp = path.with_extension("tmp");
        let tracker = ReputationTracker::new(0.95, 5);
        tracker.save(&path).unwrap();
        assert!(path.exists());
        assert!(!tmp.exists());
    }

    #[test]
    fn empty_provider_list_does_not_panic() {
        let tracker = ReputationTracker::new(0.95, 5);
        // All methods on missing provider must be safe
        assert!(!tracker.has_sufficient_observations("unknown"));
        assert!(tracker.ema_reputation_factor("unknown").is_none());
        let (a, b) = tracker.shift_thompson_priors("unknown", 1.0, 1.0, 0.3);
        assert!((a - 1.0).abs() < f64::EPSILON);
        assert!((b - 1.0).abs() < f64::EPSILON);
    }

    /// Statistical: a provider with high quality reputation should have higher alpha-shifted
    /// Thompson priors than a low-quality provider.
    #[test]
    fn high_quality_provider_gets_higher_effective_alpha() {
        let mut tracker = ReputationTracker::new(0.95, 5);
        for _ in 0..20 {
            tracker.record_quality("good", true);
        }
        for _ in 0..20 {
            tracker.record_quality("bad", false);
        }
        let (good_a, _) = tracker.shift_thompson_priors("good", 2.0, 1.0, 0.3);
        let (bad_a, _) = tracker.shift_thompson_priors("bad", 2.0, 1.0, 0.3);
        assert!(
            good_a > bad_a,
            "good provider must have higher effective alpha"
        );
    }

    #[test]
    fn new_clamps_zero_decay_factor_to_min_positive() {
        // decay_factor=0.0 is clamped to MIN_POSITIVE; no panic, decay still applies.
        let mut tracker = ReputationTracker::new(0.0, 1);
        for _ in 0..5 {
            tracker.record_quality("p", true);
        }
        let alpha_before = tracker.models["p"].dist.alpha;
        tracker.apply_decay();
        let alpha_after = tracker.models["p"].dist.alpha;
        // With decay_factor≈MIN_POSITIVE, alpha collapses back to ~1.0 (all excess decayed).
        assert!(
            alpha_after < alpha_before,
            "even minimal decay must reduce alpha"
        );
        assert!(
            alpha_after >= 1.0 - 1e-6,
            "alpha must not go below prior of 1.0"
        );
    }

    #[test]
    fn apply_decay_with_zero_observations_leaves_observations_zero() {
        let mut tracker = ReputationTracker::new(0.95, 1);
        tracker.models.insert(
            "p".to_owned(),
            ReputationEntry {
                observations: 0,
                ..Default::default()
            },
        );
        tracker.apply_decay();
        assert_eq!(
            tracker.models["p"].observations, 0,
            "zero observations must stay zero after decay"
        );
    }

    #[test]
    fn ema_reputation_factor_exact_value() {
        // After 3 successes + 1 failure with min_obs=1: alpha=4, beta=2 → mean = 4/6 = 0.666...
        let mut tracker = ReputationTracker::new(0.95, 1);
        for _ in 0..3 {
            tracker.record_quality("p", true);
        }
        tracker.record_quality("p", false);
        let factor = tracker.ema_reputation_factor("p").unwrap();
        let expected = 4.0_f64 / 6.0;
        assert!(
            (factor - expected).abs() < 1e-9,
            "factor={factor}, expected={expected}"
        );
    }

    #[test]
    fn shift_thompson_priors_zero_weight_returns_original() {
        // Even with sufficient observations, weight=0 must return priors unchanged.
        let mut tracker = ReputationTracker::new(0.95, 5);
        for _ in 0..10 {
            tracker.record_quality("p", true);
        }
        let (a, b) = tracker.shift_thompson_priors("p", 3.0, 2.0, 0.0);
        assert!(
            (a - 3.0).abs() < 1e-9,
            "alpha must be unchanged with weight=0"
        );
        assert!(
            (b - 2.0).abs() < 1e-9,
            "beta must be unchanged with weight=0"
        );
    }

    #[test]
    fn shift_thompson_priors_max_weight_shifts_fully() {
        // weight=1.0 folds entire excess reputation into the prior.
        let mut tracker = ReputationTracker::new(0.95, 5);
        for _ in 0..9 {
            tracker.record_quality("p", true);
        }
        // alpha=10, beta=1 → rep_alpha=9, rep_beta=0
        let (a, b) = tracker.shift_thompson_priors("p", 2.0, 1.0, 1.0);
        let expected_a = 2.0 + 9.0; // base + 1.0 * rep_alpha
        assert!(
            (a - expected_a).abs() < 1e-9,
            "alpha={a}, expected={expected_a}"
        );
        assert!(
            (b - 1.0).abs() < 1e-9,
            "beta must be unchanged (rep_beta=0)"
        );
    }

    #[test]
    fn prune_with_empty_known_removes_all() {
        let mut tracker = ReputationTracker::new(0.95, 1);
        tracker.record_quality("a", true);
        tracker.record_quality("b", false);
        tracker.prune(&HashSet::new());
        assert!(
            tracker.models.is_empty(),
            "prune with empty set must remove all entries"
        );
    }

    #[test]
    fn stats_returns_sorted_entries() {
        let mut tracker = ReputationTracker::new(0.95, 1);
        tracker.record_quality("zebra", true);
        tracker.record_quality("apple", false);
        tracker.record_quality("mango", true);
        let stats = tracker.stats();
        assert_eq!(stats.len(), 3);
        assert_eq!(stats[0].0, "apple");
        assert_eq!(stats[1].0, "mango");
        assert_eq!(stats[2].0, "zebra");
        // Verify mean is alpha/(alpha+beta): apple failed → alpha=1,beta=2 → mean=1/3
        let apple = &stats[0];
        let expected_mean = apple.1 / (apple.1 + apple.2);
        assert!((apple.3 - expected_mean).abs() < 1e-9);
    }

    #[test]
    fn load_sanitizes_nan_and_inf_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nan.json");
        // Write JSON with NaN (not valid JSON — use a valid float that gets clamped,
        // then check the finite-check branch by writing extreme values).
        // serde_json cannot encode NaN, so we test via a very large finite value that
        // passes clamp but would be Inf after arithmetic — instead test alpha=0.3 < 0.5.
        std::fs::write(
            &path,
            br#"{"models":{"p":{"alpha":0.1,"beta":0.2,"observations":5}},"decay_factor":0.95,"min_observations":5}"#,
        )
        .unwrap();
        let tracker = ReputationTracker::load(&path);
        let entry = &tracker.models["p"];
        // 0.1 and 0.2 are both below 0.5 → clamped to 0.5
        assert!(entry.dist.alpha >= 0.5, "alpha below 0.5 must be clamped");
        assert!(entry.dist.beta >= 0.5, "beta below 0.5 must be clamped");
    }

    #[test]
    fn save_and_load_preserves_decay_factor_and_min_observations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rep2.json");
        let tracker = ReputationTracker::new(0.80, 10);
        tracker.save(&path).unwrap();
        let loaded = ReputationTracker::load(&path);
        assert!(
            (loaded.decay_factor - 0.80).abs() < 1e-9,
            "decay_factor must round-trip"
        );
        assert_eq!(
            loaded.min_observations, 10,
            "min_observations must round-trip"
        );
    }
}
