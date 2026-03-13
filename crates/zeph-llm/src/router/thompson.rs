// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Thompson Sampling router state.
//!
//! Uses Beta distributions (via `rand_distr::Beta`) for exploration/exploitation.
//! State is persisted to `~/.zeph/router_thompson_state.json` using atomic
//! rename writes. Multiple concurrent agent instances will overwrite each
//! other's state on shutdown (known limitation, acceptable pre-1.0).

use std::collections::{HashMap, HashSet};
#[cfg(unix)]
use std::io::Write as _;
use std::path::{Path, PathBuf};

use rand::SeedableRng;
use rand_distr::{Beta, Distribution};
use serde::{Deserialize, Serialize};

/// Per-provider Beta distribution parameters.
///
/// Initialized with `alpha = 1.0, beta = 1.0` (uniform prior).
/// Updated on each response: success → alpha += 1, failure → beta += 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BetaDist {
    pub alpha: f64,
    pub beta: f64,
}

impl Default for BetaDist {
    fn default() -> Self {
        Self {
            alpha: 1.0,
            beta: 1.0,
        }
    }
}

impl BetaDist {
    /// Sample from this Beta distribution.
    ///
    /// Clamps parameters to `[1e-6, ∞)` before sampling to avoid numerical
    /// instability when either count is near zero.
    ///
    /// # Panics
    ///
    /// Does not panic: the clamped alpha/beta values are always valid for `Beta::new`.
    pub fn sample<R: rand::Rng>(&self, rng: &mut R) -> f64 {
        let alpha = self.alpha.max(1e-6);
        let beta = self.beta.max(1e-6);
        // rand_distr::Beta is validated and numerically stable.
        let dist = Beta::new(alpha, beta).unwrap_or_else(|_| Beta::new(1.0, 1.0).unwrap());
        dist.sample(rng)
    }
}

/// Result of a Thompson Sampling selection, carrying diagnostics for debug logging.
#[derive(Debug, Clone)]
pub struct ThompsonSelection {
    pub provider: String,
    pub alpha: f64,
    pub beta: f64,
    pub exploit: bool,
}

/// Thompson Sampling state for all providers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ThompsonState {
    distributions: HashMap<String, BetaDist>,
    /// Seeded once per state instance; not serialized.
    // NOTE: `#[serde(skip)]` fields are excluded from serialization/deserialization
    // and initialized via `Default::default()` (i.e., `None`) on deserialization.
    #[serde(skip)]
    rng: Option<rand::rngs::SmallRng>,
}

impl ThompsonState {
    /// Sample all providers and return the selection with diagnostics.
    ///
    /// Returns `None` if `providers` is empty.
    /// Providers without prior observations get the uniform Beta(1,1) prior.
    #[must_use]
    pub fn select(&mut self, providers: &[String]) -> Option<ThompsonSelection> {
        if providers.is_empty() {
            return None;
        }
        let rng = self
            .rng
            .get_or_insert_with(rand::rngs::SmallRng::from_os_rng);
        let (best, _) = providers
            .iter()
            .map(|name| {
                let dist = self.distributions.get(name).cloned().unwrap_or_default();
                let sample = dist.sample(rng);
                (name.clone(), sample)
            })
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))?;
        let best_dist = self.distributions.get(&best).cloned().unwrap_or_default();
        let best_mean = best_dist.alpha / (best_dist.alpha + best_dist.beta);
        let exploit = providers.iter().all(|name| {
            let dist = self.distributions.get(name).cloned().unwrap_or_default();
            best_mean >= dist.alpha / (dist.alpha + dist.beta)
        });
        Some(ThompsonSelection {
            provider: best,
            alpha: best_dist.alpha,
            beta: best_dist.beta,
            exploit,
        })
    }

    /// Update the Beta distribution for `provider` based on the outcome.
    pub fn update(&mut self, provider: &str, success: bool) {
        let dist = self.distributions.entry(provider.to_owned()).or_default();
        if success {
            dist.alpha += 1.0;
        } else {
            dist.beta += 1.0;
        }
    }

    /// Returns sorted `(provider_name, alpha, beta)` tuples for all tracked providers.
    ///
    /// Useful for CLI inspection (`zeph router stats`).
    #[must_use]
    pub fn provider_stats(&self) -> Vec<(String, f64, f64)> {
        let mut stats: Vec<(String, f64, f64)> = self
            .distributions
            .iter()
            .map(|(name, dist)| (name.clone(), dist.alpha, dist.beta))
            .collect();
        stats.sort_by(|a, b| a.0.cmp(&b.0));
        stats
    }

    /// Default state file path: `~/.zeph/router_thompson_state.json`.
    #[must_use]
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".zeph")
            .join("router_thompson_state.json")
    }

    /// Remove distribution entries for providers not in `known`.
    ///
    /// Prevents unbounded growth when providers are renamed or removed from config.
    pub fn prune(&mut self, known: &HashSet<String>) {
        self.distributions.retain(|k, _| known.contains(k));
    }

    /// Load state from `path`. Falls back to uniform prior on any error.
    ///
    /// Logs a warning if the file exists but cannot be parsed (corrupt file).
    /// Missing file is normal on first run and logged only at debug level.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Self::default();
            }
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "Thompson state file unreadable, using uniform priors");
                return Self::default();
            }
        };
        match serde_json::from_slice::<Self>(&bytes) {
            Ok(mut state) => {
                // Clamp alpha/beta to a valid finite range to reject corrupt or
                // adversarially-crafted state files that could skew routing.
                for dist in state.distributions.values_mut() {
                    dist.alpha = dist.alpha.clamp(0.5, 1e9);
                    dist.beta = dist.beta.clamp(0.5, 1e9);
                    if !dist.alpha.is_finite() {
                        dist.alpha = 1.0;
                    }
                    if !dist.beta.is_finite() {
                        dist.beta = 1.0;
                    }
                }
                state
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Thompson state file is corrupt; resetting to uniform priors"
                );
                Self::default()
            }
        }
    }

    /// Save state to `path` using an atomic write (tmp file + rename).
    ///
    /// On Unix the tmp file is created with mode `0o600` (owner read/write only)
    /// to prevent other users from reading sensitive routing state.
    ///
    /// # Errors
    ///
    /// Returns an `io::Error` if the write or rename fails.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_vec(self).map_err(|e| std::io::Error::other(e.to_string()))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // TODO: use a randomized suffix (e.g., via `tempfile::NamedTempFile`) to avoid
        // the predictable `.tmp` path being a symlink-race target on shared directories.
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

    #[test]
    fn select_empty_providers_returns_none() {
        let mut state = ThompsonState::default();
        assert!(state.select(&[]).is_none());
    }

    #[test]
    fn select_single_provider_returns_it() {
        let mut state = ThompsonState::default();
        let result = state.select(&["ollama".to_owned()]);
        assert_eq!(result.map(|s| s.provider).as_deref(), Some("ollama"));
    }

    #[test]
    fn select_returns_one_of_the_providers() {
        let mut state = ThompsonState::default();
        let providers = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let selected = state.select(&providers).unwrap().provider;
        assert!(providers.contains(&selected));
    }

    #[test]
    fn update_success_increases_alpha() {
        let mut state = ThompsonState::default();
        state.update("p", true);
        let dist = &state.distributions["p"];
        assert!((dist.alpha - 2.0).abs() < f64::EPSILON);
        assert!((dist.beta - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn update_failure_increases_beta() {
        let mut state = ThompsonState::default();
        state.update("p", false);
        let dist = &state.distributions["p"];
        assert!((dist.alpha - 1.0).abs() < f64::EPSILON);
        assert!((dist.beta - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn beta_dist_sample_in_range() {
        let dist = BetaDist::default();
        let mut rng = rand::rngs::SmallRng::seed_from_u64(42);
        for _ in 0..100 {
            let v = dist.sample(&mut rng);
            assert!((0.0..=1.0).contains(&v), "sample {v} out of [0,1]");
        }
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thompson.json");

        let mut state = ThompsonState::default();
        state.update("provider_a", true);
        state.update("provider_a", true);
        state.update("provider_b", false);
        state.save(&path).unwrap();

        let loaded = ThompsonState::load(&path);
        assert!((loaded.distributions["provider_a"].alpha - 3.0).abs() < f64::EPSILON);
        assert!((loaded.distributions["provider_a"].beta - 1.0).abs() < f64::EPSILON);
        assert!((loaded.distributions["provider_b"].beta - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn load_missing_file_returns_default() {
        let state = ThompsonState::load(Path::new("/tmp/does-not-exist-zeph-test.json"));
        assert!(state.distributions.is_empty());
    }

    #[test]
    fn load_corrupt_file_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.json");
        std::fs::write(&path, b"not valid json {{{{").unwrap();
        let state = ThompsonState::load(&path);
        assert!(state.distributions.is_empty());
    }

    #[test]
    fn prune_removes_stale_entries() {
        let mut state = ThompsonState::default();
        state.update("provider_a", true);
        state.update("provider_b", false);
        state.update("provider_c", true);

        let known: HashSet<String> = ["provider_a".to_owned(), "provider_c".to_owned()]
            .into_iter()
            .collect();
        state.prune(&known);

        assert!(state.distributions.contains_key("provider_a"));
        assert!(!state.distributions.contains_key("provider_b"));
        assert!(state.distributions.contains_key("provider_c"));
    }

    #[test]
    fn provider_stats_returns_sorted_entries() {
        let mut state = ThompsonState::default();
        state.update("z_provider", true);
        state.update("a_provider", false);
        state.update("m_provider", true);

        let provider_stats = state.provider_stats();
        assert_eq!(provider_stats.len(), 3);
        assert_eq!(provider_stats[0].0, "a_provider");
        assert_eq!(provider_stats[1].0, "m_provider");
        assert_eq!(provider_stats[2].0, "z_provider");
    }

    /// Statistical correctness test: a provider with high alpha should be selected
    /// disproportionately more often than one with high beta.
    ///
    /// After recording 50 successes for `provider_a` and 50 failures for `provider_b`,
    /// the Beta(51,1) vs Beta(1,51) difference is dramatic. Over 1000 trials,
    /// `provider_a` should be selected at least 90% of the time.
    #[test]
    fn high_alpha_provider_selected_disproportionately() {
        let mut state = ThompsonState::default();
        for _ in 0..50 {
            state.update("provider_a", true);
            state.update("provider_b", false);
        }

        let providers = vec!["provider_a".to_owned(), "provider_b".to_owned()];
        let trials = 1000usize;
        let mut a_wins = 0usize;
        for _ in 0..trials {
            if state.select(&providers).map(|s| s.provider).as_deref() == Some("provider_a") {
                a_wins += 1;
            }
        }

        // With Beta(51, 1) vs Beta(1, 51), provider_a wins >>99% of trials.
        // We use a conservative threshold of 90% to avoid flakiness.
        let Ok(wins) = u32::try_from(a_wins) else {
            panic!("a_wins overflowed u32");
        };
        let ratio = f64::from(wins) / 1000.0;
        assert!(
            ratio > 0.90,
            "provider_a should be selected >90% of the time, got {ratio:.2}"
        );
    }

    #[test]
    fn select_is_mut_compatible_with_repeated_calls() {
        let mut state = ThompsonState::default();
        state.update("a", true);
        state.update("b", false);
        let providers = vec!["a".to_owned(), "b".to_owned()];

        // Repeated calls should not panic or lock up.
        for _ in 0..10 {
            let result = state.select(&providers);
            assert!(result.is_some());
            assert!(!result.unwrap().provider.is_empty());
        }
    }

    #[test]
    fn save_leaves_no_tmp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let tmp = path.with_extension("tmp");

        let mut state = ThompsonState::default();
        state.update("p", true);
        state.save(&path).unwrap();

        assert!(path.exists(), "state file must exist after save");
        assert!(
            !tmp.exists(),
            "tmp file must be cleaned up after atomic rename"
        );
    }

    #[test]
    fn load_clamps_out_of_range_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        // Write a state with alpha = -1 and beta = 2e12 (both out of valid range).
        std::fs::write(
            &path,
            br#"{"distributions":{"p":{"alpha":-1.0,"beta":2000000000000.0}}}"#,
        )
        .unwrap();
        let state = ThompsonState::load(&path);
        let dist = &state.distributions["p"];
        assert!(dist.alpha >= 0.5, "alpha must be clamped to >= 0.5");
        assert!(dist.beta <= 1e9, "beta must be clamped to <= 1e9");
    }
}
