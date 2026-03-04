// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Thompson Sampling router state.
//!
//! Uses Beta distributions (via `rand_distr::Beta`) for exploration/exploitation.
//! State is persisted to `~/.zeph/router_thompson_state.json` using atomic
//! rename writes. Multiple concurrent agent instances will overwrite each
//! other's state on shutdown (known limitation, acceptable pre-1.0).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rand::SeedableRng;
use rand_distr::{Beta, Distribution};
use serde::{Deserialize, Serialize};

/// Per-provider Beta distribution parameters.
///
/// Initialized with `alpha = 1.0, beta = 1.0` (uniform prior).
/// Updated on each response: success → alpha += 1, failure → beta += 1.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Thompson Sampling state for all providers.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ThompsonState {
    distributions: HashMap<String, BetaDist>,
}

impl ThompsonState {
    /// Sample all providers and return the name of the one with the highest sample.
    ///
    /// Returns `None` if `providers` is empty or no provider is in the state.
    /// Falls back to the first provider name if none match the state map.
    #[must_use]
    pub fn select(&self, providers: &[String]) -> Option<String> {
        if providers.is_empty() {
            return None;
        }
        let mut rng = rand::rngs::SmallRng::from_os_rng();
        let (best, _) = providers
            .iter()
            .map(|name| {
                let dist = self.distributions.get(name).cloned().unwrap_or_default();
                let sample = dist.sample(&mut rng);
                (name.clone(), sample)
            })
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))?;
        Some(best)
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

    /// Default state file path: `~/.zeph/router_thompson_state.json`.
    #[must_use]
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".zeph")
            .join("router_thompson_state.json")
    }

    /// Load state from `path`. Falls back to uniform prior on any error.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        let Ok(bytes) = std::fs::read(path) else {
            return Self::default();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    /// Save state to `path` using an atomic write (tmp file + rename).
    ///
    /// # Errors
    ///
    /// Returns an `io::Error` if the write or rename fails.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_vec(self).map_err(|e| std::io::Error::other(e.to_string()))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
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
        let state = ThompsonState::default();
        assert!(state.select(&[]).is_none());
    }

    #[test]
    fn select_single_provider_returns_it() {
        let state = ThompsonState::default();
        let result = state.select(&["ollama".to_owned()]);
        assert_eq!(result.as_deref(), Some("ollama"));
    }

    #[test]
    fn select_returns_one_of_the_providers() {
        let state = ThompsonState::default();
        let providers = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let selected = state.select(&providers).unwrap();
        assert!(providers.contains(&selected));
    }

    #[test]
    fn update_success_increases_alpha() {
        let mut state = ThompsonState::default();
        state.update("p", true);
        let dist = &state.distributions["p"];
        assert_eq!(dist.alpha, 2.0);
        assert_eq!(dist.beta, 1.0);
    }

    #[test]
    fn update_failure_increases_beta() {
        let mut state = ThompsonState::default();
        state.update("p", false);
        let dist = &state.distributions["p"];
        assert_eq!(dist.alpha, 1.0);
        assert_eq!(dist.beta, 2.0);
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
        assert_eq!(loaded.distributions["provider_a"].alpha, 3.0);
        assert_eq!(loaded.distributions["provider_a"].beta, 1.0);
        assert_eq!(loaded.distributions["provider_b"].beta, 2.0);
    }

    #[test]
    fn load_missing_file_returns_default() {
        let state = ThompsonState::load(Path::new("/tmp/does-not-exist-zeph-test.json"));
        assert!(state.distributions.is_empty());
    }
}
