// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

// LinUCB and PCA/OpenAI are external proper nouns; skip backtick lint for identifiers in doc comments.
#![allow(clippy::doc_markdown)]

//! PILOT (Provider Intelligence via Learned Online Tuning) — LinUCB contextual bandit routing.
//!
//! Each provider has a per-arm `(A_k, b_k)` pair updated online:
//!   - `A_k += x * x^T` on every selection of arm k
//!   - `b_k += reward * x` on every selection of arm k
//!   - `theta_k = A_k^{-1} * b_k`
//!   - UCB score = `theta_k^T * x + alpha * sqrt(x^T * A_k^{-1} * x)`
//!
//! Feature vector `x` is the first `dim` components of the query embedding (simple truncation),
//! L2-normalised. Truncation is a cheap approximation — it is NOT equivalent to PCA. The first
//! raw embedding dimensions do not necessarily capture the most variance. This is an acceptable
//! pre-1.0 trade-off; Matryoshka embeddings (OpenAI `text-embedding-3-*` with `dimensions`
//! parameter) and random projection are better alternatives documented in #2230.
//!
//! During the cold-start period (`total_updates < warmup_queries`), selection falls back to
//! Thompson sampling (via caller) or uniform selection within this module. Once warmup is
//! complete, LinUCB takes over.
//!
//! The decay mechanism (`A_k = I + decay * (A_k - I)`, `b_k = decay * b_k`) prevents stale
//! observations from dominating when provider quality changes. With `decay < 1.0` the bandit
//! never truly converges — it perpetually re-explores. This is intentional for non-stationary
//! provider quality. Use `decay_factor = 1.0` (default) to disable decay.
//!
//! # Security
//!
//! `BanditState` is loaded from a user-controlled path. Values are validated and clamped on
//! load. Do not place the state file in world-writable directories.

use std::collections::{HashMap, HashSet};
#[cfg(unix)]
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Per-provider LinUCB state.
/// `a_matrix`: d×d matrix (row-major). `b_vector`: d-vector.
///
/// `a_matrix`: d×d matrix (row-major, stored as `Vec<f32>` of length `d*d`).
/// Initialised to identity I_d. Remains positive semi-definite by construction
/// (only rank-1 PSD updates `x*x^T` are ever added).
///
/// `b_vector`: d-vector, initialised to zero.
///
/// All arithmetic uses `f32`. Embeddings from `LlmProvider::embed()` are already `Vec<f32>`,
/// so no precision is lost in conversion. Accumulating outer products in f32 is sufficient
/// for d=32; float32 has ~7 decimal digits of precision and matrices stay well-conditioned
/// with the regularising identity initialisation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinUcbArm {
    /// A matrix (d×d), row-major.
    pub a_matrix: Vec<f32>,
    /// b vector (d×1).
    pub b_vector: Vec<f32>,
    /// Number of times this arm has been updated (pulled and rewarded).
    pub n: u64,
    /// Cumulative reward sum (diagnostics only).
    pub total_reward: f32,
}

impl LinUcbArm {
    /// Create a fresh arm: A = I_d, b = 0.
    #[must_use]
    pub fn new(dim: usize) -> Self {
        let mut a = vec![0.0f32; dim * dim];
        for i in 0..dim {
            a[i * dim + i] = 1.0;
        }
        Self {
            a_matrix: a,
            b_vector: vec![0.0f32; dim],
            n: 0,
            total_reward: 0.0,
        }
    }

    /// Compute `theta = A^{-1} * b` and `p = theta^T * x + alpha * sqrt(x^T * A^{-1} * x)`.
    ///
    /// Uses Gaussian elimination with partial pivoting to solve the linear system.
    /// At d=32 this is ~32^3 / 3 ≈ 11k floating-point ops — negligible.
    ///
    /// Returns `None` if the matrix is (near-)singular or if `x` is all-zero.
    #[must_use]
    pub fn ucb_score(&self, features: &[f32], alpha: f32) -> Option<f32> {
        let dim = self.b_vector.len();
        debug_assert_eq!(features.len(), dim, "feature vector dim mismatch");
        debug_assert_eq!(self.a_matrix.len(), dim * dim, "A matrix dim mismatch");

        // Solve A * theta = b  (theta = A^{-1} * b).
        let theta = solve_linear(dim, &self.a_matrix, &self.b_vector)?;

        // Solve A * v = features  (v = A^{-1} * features), then uncertainty = features^T * v.
        let inv_v = solve_linear(dim, &self.a_matrix, features)?;
        let uncertainty: f32 = features
            .iter()
            .zip(inv_v.iter())
            .map(|(feat, inv)| feat * inv)
            .sum();
        if !uncertainty.is_finite() || uncertainty < 0.0 {
            return None;
        }

        let exploit: f32 = theta
            .iter()
            .zip(features.iter())
            .map(|(th, feat)| th * feat)
            .sum();
        let ucb = exploit + alpha * uncertainty.sqrt();
        if ucb.is_finite() { Some(ucb) } else { None }
    }

    /// Update arm: `A += features * features^T`, `b += reward * features`, `n += 1`.
    pub fn update(&mut self, features: &[f32], reward: f32) {
        let dim = self.b_vector.len();
        debug_assert_eq!(features.len(), dim);
        for row in 0..dim {
            for col in 0..dim {
                self.a_matrix[row * dim + col] += features[row] * features[col];
            }
            self.b_vector[row] += reward * features[row];
        }
        self.n += 1;
        self.total_reward += reward;
    }

    /// Apply session decay: `A = I + decay * (A - I)`, `b = decay * b`.
    ///
    /// Converges A toward I and b toward 0 as decay → 0. This causes re-exploration
    /// but prevents stale observations from locking in a suboptimal choice permanently.
    pub fn apply_decay(&mut self, decay: f32) {
        let dim = self.b_vector.len();
        for row in 0..dim {
            for col in 0..dim {
                let identity_val = if row == col { 1.0f32 } else { 0.0f32 };
                self.a_matrix[row * dim + col] =
                    identity_val + decay * (self.a_matrix[row * dim + col] - identity_val);
            }
            self.b_vector[row] *= decay;
        }
    }
}

/// Static cost estimate for a provider, normalised to [0.0, 1.0].
///
/// Checks both provider name and model identifier for known cost-tier patterns.
/// This is a coarse heuristic — the bandit learns actual quality/cost tradeoffs
/// online. The estimate only provides an initial directional bias.
///
/// Returns 0.3 for unknown providers (conservative mid-low fallback).
#[must_use]
pub fn provider_cost_estimate(provider_name: &str, model_id: &str) -> f32 {
    // Check model identifier first (more specific), then provider name as fallback.
    for s in [model_id, provider_name] {
        let s = s.to_ascii_lowercase();
        // Local / free tier: ollama, candle, local runners.
        if s.contains("ollama") || s.contains("candle") || s.contains("local") {
            return 0.1;
        }
        // Cheap cloud: mini, nano, small, haiku, flash, qwen, llama, phi, gemma.
        if s.contains("mini")
            || s.contains("nano")
            || s.contains("small")
            || s.contains("haiku")
            || s.contains("flash")
            || s.contains("qwen")
            || s.contains("llama")
            || s.contains("phi")
            || s.contains("gemma")
        {
            return 0.2;
        }
        // Mid tier: gpt-4o (not mini), sonnet, mistral, gemini-pro.
        if s.contains("sonnet")
            || s.contains("4o")
            || s.contains("gemini-pro")
            || s.contains("mistral")
            || s.contains("medium")
        {
            return 0.5;
        }
        // Expensive tier: opus, gpt-5, o1, o3, gpt-4 (not 4o), claude-3, claude-opus.
        if s.contains("opus")
            || s.contains("gpt-5")
            || s.contains("o1-")
            || s.contains("-o1")
            || s.contains("o3-")
            || s.contains("-o3")
            || s.contains("gpt-4-")
            || s.contains("-4-")
        {
            return 0.8;
        }
    }
    // Unknown provider/model: conservative mid-low default.
    0.3
}

/// PILOT bandit state for all providers.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BanditState {
    pub arms: HashMap<String, LinUcbArm>,
    pub dim: usize,
    /// Total number of updates across all arms. Used for warm-up detection.
    pub total_updates: u64,
}

impl BanditState {
    #[must_use]
    pub fn new(dim: usize) -> Self {
        Self {
            arms: HashMap::new(),
            dim,
            total_updates: 0,
        }
    }

    /// Return the arm for `provider`, initialising it fresh if absent.
    fn arm_mut(&mut self, provider: &str) -> &mut LinUcbArm {
        let dim = self.dim;
        self.arms
            .entry(provider.to_owned())
            .or_insert_with(|| LinUcbArm::new(dim))
    }

    /// Select the best provider using LinUCB UCB scores.
    ///
    /// `budget_filter(name) -> true` means the provider is available (within budget).
    /// When `total_updates < warmup_queries`, returns `None` (caller falls back to Thompson/uniform).
    /// When all available providers are filtered out, returns `None`.
    ///
    /// Ties broken by provider name (deterministic).
    ///
    /// `provider_models`: maps provider name to model identifier for cost estimation.
    /// `cost_weight`: BaRP dial clamped to [0.0, 1.0]. 0.0 disables cost influence entirely.
    /// `memory_hit_confidence`: MAR signal. When `>= memory_confidence_threshold`, cheap
    /// providers receive a boost proportional to `(1 - cost_estimate) * confidence * cost_weight`.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn select(
        &self,
        providers: &[String],
        features: &[f32],
        alpha: f32,
        warmup_queries: u64,
        budget_filter: &dyn Fn(&str) -> bool,
        cost_weight: f32,
        provider_models: &std::collections::HashMap<String, String>,
        memory_hit_confidence: Option<f32>,
        memory_confidence_threshold: f32,
    ) -> Option<String> {
        // Cold-start: defer to Thompson until we have enough observations.
        if self.total_updates < warmup_queries {
            return None;
        }

        let candidates: Vec<&String> = providers
            .iter()
            .filter(|name| budget_filter(name))
            .collect();

        if candidates.is_empty() {
            return None;
        }

        let mar_active = memory_hit_confidence.is_some_and(|c| c >= memory_confidence_threshold);

        let mut best_name: Option<&str> = None;
        let mut best_score = f32::NEG_INFINITY;

        for name in &candidates {
            let raw_ucb = if let Some(arm) = self.arms.get(name.as_str()) {
                arm.ucb_score(features, alpha).unwrap_or(0.0)
            } else {
                LinUcbArm::new(self.dim)
                    .ucb_score(features, alpha)
                    .unwrap_or(0.0)
            };

            let model_id = provider_models
                .get(name.as_str())
                .map_or("", String::as_str);
            let cost_est = provider_cost_estimate(name, model_id);

            // BaRP: penalise expensive providers when cost_weight > 0.
            let cost_penalty = cost_weight * cost_est;

            // MAR: when memory confidence is high, boost cheaper providers.
            // When cost_weight = 0.0, no boost is applied (operator disabled cost awareness).
            let memory_boost = if mar_active {
                let conf = memory_hit_confidence.unwrap_or(0.0);
                (1.0 - cost_est) * conf * cost_weight
            } else {
                0.0
            };

            let score = raw_ucb - cost_penalty + memory_boost;

            let is_better = score > best_score
                || (score.total_cmp(&best_score).is_eq()
                    && best_name.is_none_or(|prev: &str| name.as_str() < prev));
            if is_better {
                best_score = score;
                best_name = Some(name.as_str());
            }
        }

        best_name.map(str::to_owned)
    }

    /// Update the arm for `provider` with the observed reward and feature vector.
    pub fn update(&mut self, provider: &str, features: &[f32], reward: f32) {
        self.arm_mut(provider).update(features, reward);
        self.total_updates += 1;
    }

    /// Apply session-level decay to all arms.
    pub fn apply_decay(&mut self, decay_factor: f32) {
        for arm in self.arms.values_mut() {
            arm.apply_decay(decay_factor);
        }
    }

    /// Remove arms for providers not in `known`.
    pub fn prune(&mut self, known: &HashSet<String>) {
        self.arms.retain(|k, _| known.contains(k));
    }

    /// Return diagnostic stats: `(name, pulls, mean_reward)`.
    #[must_use]
    pub fn stats(&self) -> Vec<(String, u64, f32)> {
        let mut result: Vec<(String, u64, f32)> = self
            .arms
            .iter()
            .map(|(name, arm)| {
                let mean = if arm.n > 0 {
                    // n is bounded by total request count; precision loss acceptable here.
                    #[allow(clippy::cast_precision_loss)]
                    let n_f32 = arm.n as f32;
                    arm.total_reward / n_f32
                } else {
                    0.0
                };
                (name.clone(), arm.n, mean)
            })
            .collect();
        result.sort_by(|a, b| a.0.cmp(&b.0));
        result
    }

    /// Default state file: `~/.config/zeph/router_bandit_state.json`.
    #[must_use]
    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("zeph")
            .join("router_bandit_state.json")
    }

    /// Load state from `path`. Falls back to a fresh default on any error.
    ///
    /// Validates loaded values: clamps matrix entries to `[-1e9, 1e9]`, replaces
    /// non-finite values with identity/zero, replaces zero-length arms with fresh ones.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Self::default();
            }
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "bandit state file unreadable");
                return Self::default();
            }
        };
        match serde_json::from_slice::<Self>(&bytes) {
            Ok(mut state) => {
                // Sanitise loaded values to reject corrupt or adversarially-crafted state.
                let dim = state.dim;
                for arm in state.arms.values_mut() {
                    // Rebuild to correct dim if arm was saved with different dim (e.g. config change).
                    if arm.a_matrix.len() != dim * dim || arm.b_vector.len() != dim {
                        *arm = LinUcbArm::new(dim);
                        continue;
                    }
                    for v in &mut arm.a_matrix {
                        if !v.is_finite() {
                            *v = 0.0;
                        }
                        *v = v.clamp(-1e9, 1e9);
                    }
                    for v in &mut arm.b_vector {
                        if !v.is_finite() {
                            *v = 0.0;
                        }
                        *v = v.clamp(-1e9, 1e9);
                    }
                }
                state
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "bandit state file is corrupt; resetting"
                );
                Self::default()
            }
        }
    }

    /// Save state to `path` using an atomic write (write to `.tmp`, then rename).
    ///
    /// On Unix the `.tmp` file is created with mode `0o600`.
    ///
    /// # Errors
    ///
    /// Returns `io::Error` if serialization, write, or rename fails.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_vec(self).map_err(|e| std::io::Error::other(e.to_string()))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // TODO: use randomised suffix (e.g. `tempfile::NamedTempFile`) to avoid
        // predictable `.tmp` path being a symlink-race target on shared directories.
        let tmp = path.with_extension("tmp");
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
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

/// Truncate embedding to `dim` dimensions and L2-normalise.
///
/// Returns `None` if `embedding` is shorter than `dim` or if the vector is all-zero
/// (normalisation would produce NaN / Inf).
///
/// Truncation is a cheap approximation (not PCA). For better results with OpenAI
/// embeddings, consider using the `dimensions` API parameter (Matryoshka).
#[must_use]
pub fn embedding_to_features(embedding: &[f32], dim: usize) -> Option<Vec<f32>> {
    if embedding.len() < dim {
        return None;
    }
    let truncated = &embedding[..dim];
    let norm: f32 = truncated.iter().map(|v| v * v).sum::<f32>().sqrt();
    if !norm.is_finite() || norm < 1e-9 {
        return None;
    }
    Some(truncated.iter().map(|v| v / norm).collect())
}

/// Solve `A * x = b` via Gaussian elimination with partial pivoting.
///
/// Returns `None` if the matrix is (near-)singular (pivot < 1e-9).
/// Operates on f32 copies; does not mutate the originals.
/// Used only at d=32; no external linear algebra dependency needed.
fn solve_linear(dim: usize, mat_a: &[f32], vec_b: &[f32]) -> Option<Vec<f32>> {
    debug_assert_eq!(mat_a.len(), dim * dim);
    debug_assert_eq!(vec_b.len(), dim);

    // Augmented matrix [A | b], row-major, dim rows × (dim+1) cols.
    let cols = dim + 1;
    let mut mat: Vec<f32> = Vec::with_capacity(dim * cols);
    for row in 0..dim {
        for col in 0..dim {
            mat.push(mat_a[row * dim + col]);
        }
        mat.push(vec_b[row]);
    }

    for col in 0..dim {
        // Find pivot (max absolute value in this column at or below current row).
        let mut max_row = col;
        let mut max_val = mat[col * cols + col].abs();
        for row in (col + 1)..dim {
            let val = mat[row * cols + col].abs();
            if val > max_val {
                max_val = val;
                max_row = row;
            }
        }
        if max_val < 1e-9 {
            return None; // Singular or near-singular
        }
        // Swap rows.
        if max_row != col {
            for cidx in 0..cols {
                mat.swap(col * cols + cidx, max_row * cols + cidx);
            }
        }
        // Eliminate below.
        let pivot = mat[col * cols + col];
        for row in (col + 1)..dim {
            let factor = mat[row * cols + col] / pivot;
            for cidx in col..cols {
                let v = mat[col * cols + cidx];
                mat[row * cols + cidx] -= factor * v;
            }
        }
    }

    // Back-substitution.
    let mut sol = vec![0.0f32; dim];
    for row in (0..dim).rev() {
        let mut s = mat[row * cols + dim];
        for cidx in (row + 1)..dim {
            s -= mat[row * cols + cidx] * sol[cidx];
        }
        let pivot = mat[row * cols + row];
        if pivot.abs() < 1e-9 {
            return None;
        }
        sol[row] = s / pivot;
    }
    Some(sol)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_arm(dim: usize) -> LinUcbArm {
        LinUcbArm::new(dim)
    }

    // ── Linear solver ────────────────────────────────────────────────────────

    #[test]
    fn solve_identity_returns_rhs() {
        let dim = 3;
        let mat_a = vec![
            1.0, 0.0, 0.0, // row 0
            0.0, 1.0, 0.0, // row 1
            0.0, 0.0, 1.0, // row 2
        ];
        let vec_b = vec![2.0f32, -1.0, 3.0];
        let sol = solve_linear(dim, &mat_a, &vec_b).unwrap();
        for (si, bi) in sol.iter().zip(vec_b.iter()) {
            assert!((si - bi).abs() < 1e-5, "expected {bi}, got {si}");
        }
    }

    #[test]
    fn solve_known_matrix() {
        // A = [[2, 1], [1, 3]], b = [5, 10]  → x = [1, 3]
        let mat_a = vec![2.0f32, 1.0, 1.0, 3.0];
        let vec_b = vec![5.0f32, 10.0];
        let sol = solve_linear(2, &mat_a, &vec_b).unwrap();
        assert!((sol[0] - 1.0).abs() < 1e-5);
        assert!((sol[1] - 3.0).abs() < 1e-5);
    }

    #[test]
    fn solve_singular_returns_none() {
        let mat_a = vec![1.0f32, 2.0, 2.0, 4.0]; // rank-1
        let vec_b = vec![1.0f32, 2.0];
        assert!(solve_linear(2, &mat_a, &vec_b).is_none());
    }

    // ── UCB score ────────────────────────────────────────────────────────────

    #[test]
    fn ucb_score_fresh_arm_equals_alpha_times_norm() {
        // For A = I, theta = 0, v = x, uncertainty = ||x||^2.
        // UCB = 0 + alpha * ||x||.
        // After L2 normalisation ||x|| = 1, so UCB = alpha.
        let dim = 4;
        let arm = identity_arm(dim);
        let x = [0.5f32, 0.5, 0.5, 0.5];
        let norm: f32 = x.iter().map(|v| v * v).sum::<f32>().sqrt();
        let x_norm: Vec<f32> = x.iter().map(|v| v / norm).collect();
        let alpha = 1.0f32;
        let score = arm.ucb_score(&x_norm, alpha).unwrap();
        // With A=I: x^T A^{-1} x = x^T x = 1 (normalised), sqrt = 1, UCB = alpha.
        assert!((score - alpha).abs() < 1e-4, "score={score}");
    }

    #[test]
    fn ucb_score_nan_feature_returns_none() {
        let dim = 4;
        let arm = identity_arm(dim);
        let x = [f32::NAN, 0.5, 0.5, 0.5];
        // NaN propagates through dot products; ucb_score must return None.
        let score = arm.ucb_score(&x, 1.0);
        // Either None (if detected) or Some(NaN-based), but we normalise so should be None or invalid.
        // Our solve_linear will likely return a non-finite result → ucb_score returns None.
        assert!(score.is_none_or(|s: f32| !s.is_finite()));
    }

    // ── Arm update ───────────────────────────────────────────────────────────

    #[test]
    fn update_modifies_a_and_b() {
        let dim = 2;
        let mut arm = identity_arm(dim);
        let x = vec![1.0f32, 0.0];
        let reward = 0.8f32;
        arm.update(&x, reward);
        // A[0][0] += x[0]*x[0] = 1 → A[0][0] = 2
        assert!((arm.a_matrix[0] - 2.0).abs() < 1e-6);
        // A[0][1] += x[0]*x[1] = 0 → unchanged
        assert!((arm.a_matrix[1] - 0.0).abs() < 1e-6);
        // b[0] += reward * x[0] = 0.8
        assert!((arm.b_vector[0] - 0.8).abs() < 1e-6);
        // b[1] unchanged
        assert!((arm.b_vector[1] - 0.0).abs() < 1e-6);
        assert_eq!(arm.n, 1);
    }

    // ── Decay ────────────────────────────────────────────────────────────────

    #[test]
    fn decay_converges_to_identity() {
        let dim = 2;
        let mut arm = identity_arm(dim);
        // Give it some off-diagonal mass.
        arm.a_matrix[1] = 5.0; // off-diag
        arm.b_vector[0] = 10.0;
        // Apply strong decay many times.
        for _ in 0..100 {
            arm.apply_decay(0.5);
        }
        // After many decays A ≈ I, b ≈ 0.
        assert!((arm.a_matrix[0] - 1.0).abs() < 0.01); // diag
        assert!(arm.a_matrix[1].abs() < 0.01); // off-diag → 0
        assert!(arm.b_vector[0].abs() < 0.01);
    }

    // ── Feature extraction ───────────────────────────────────────────────────

    #[test]
    fn embedding_to_features_normalises() {
        let emb = vec![3.0f32, 4.0, 0.0, 0.0]; // norm = 5
        let feat = embedding_to_features(&emb, 2).unwrap();
        assert!((feat[0] - 0.6).abs() < 1e-5);
        assert!((feat[1] - 0.8).abs() < 1e-5);
    }

    #[test]
    fn embedding_to_features_short_returns_none() {
        let emb = vec![1.0f32, 2.0];
        assert!(embedding_to_features(&emb, 4).is_none());
    }

    #[test]
    fn embedding_to_features_zero_vector_returns_none() {
        let emb = vec![0.0f32; 8];
        assert!(embedding_to_features(&emb, 4).is_none());
    }

    // ── BanditState selection ────────────────────────────────────────────────

    #[test]
    fn select_returns_none_during_warmup() {
        let state = BanditState::new(4);
        let providers = vec!["a".to_owned(), "b".to_owned()];
        let x = vec![1.0f32, 0.0, 0.0, 0.0];
        let result = state.select(
            &providers,
            &x,
            1.0,
            10,
            &|_| true,
            0.0,
            &Default::default(),
            None,
            0.9,
        );
        assert!(result.is_none(), "should fall back during warmup");
    }

    #[test]
    fn select_returns_none_when_all_filtered() {
        let mut state = BanditState::new(2);
        // Bypass warmup by setting total_updates.
        state.total_updates = 100;
        let providers = vec!["a".to_owned(), "b".to_owned()];
        let x = vec![1.0f32, 0.0];
        let result = state.select(
            &providers,
            &x,
            1.0,
            10,
            &|_| false,
            0.0,
            &Default::default(),
            None,
            0.9,
        );
        assert!(result.is_none());
    }

    #[test]
    fn select_after_warmup_returns_provider() {
        let mut state = BanditState::new(2);
        state.total_updates = 100;
        let providers = vec!["a".to_owned(), "b".to_owned()];
        let x = vec![1.0f32, 0.0];
        let result = state.select(
            &providers,
            &x,
            1.0,
            10,
            &|_| true,
            0.0,
            &Default::default(),
            None,
            0.9,
        );
        assert!(result.is_some());
        assert!(providers.contains(result.as_ref().unwrap()));
    }

    #[test]
    fn select_one_provider_over_budget_excluded() {
        let mut state = BanditState::new(2);
        state.total_updates = 100;
        let providers = vec!["a".to_owned(), "b".to_owned()];
        let x = vec![1.0f32, 0.0];
        // Provider "b" is over budget.
        let result = state.select(
            &providers,
            &x,
            1.0,
            10,
            &|name| name != "b",
            0.0,
            &Default::default(),
            None,
            0.9,
        );
        assert_eq!(result.as_deref(), Some("a"));
    }

    #[test]
    fn select_converges_to_best_arm() {
        // After many rewards for "a" and none for "b", bandit should prefer "a".
        let dim = 2;
        let mut state = BanditState::new(dim);
        let x = vec![1.0f32, 0.0];
        // Reward "a" consistently, punish "b".
        for _ in 0..50 {
            state.update("a", &x, 0.9);
            state.update("b", &x, 0.1);
        }
        let providers = vec!["a".to_owned(), "b".to_owned()];
        // Run many selections, count how often "a" wins.
        let mut a_wins = 0usize;
        for _ in 0..100 {
            if state
                .select(
                    &providers,
                    &x,
                    0.01,
                    0,
                    &|_| true,
                    0.0,
                    &Default::default(),
                    None,
                    0.9,
                )
                .as_deref()
                == Some("a")
            {
                a_wins += 1;
            }
        }
        assert!(a_wins > 80, "expected a to win >80%, got {a_wins}/100");
    }

    // ── Persistence ──────────────────────────────────────────────────────────

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bandit.json");

        let dim = 4;
        let mut state = BanditState::new(dim);
        let x = vec![1.0f32, 0.0, 0.0, 0.0];
        state.update("provider_a", &x, 0.8);
        state.update("provider_b", &x, 0.3);
        state.save(&path).unwrap();

        let loaded = BanditState::load(&path);
        assert_eq!(loaded.dim, dim);
        assert_eq!(loaded.total_updates, 2);
        assert!(loaded.arms.contains_key("provider_a"));
        assert_eq!(loaded.arms["provider_a"].n, 1);
    }

    #[test]
    fn load_missing_file_returns_default() {
        let state = BanditState::load(Path::new("/tmp/does-not-exist-bandit-zeph.json"));
        assert!(state.arms.is_empty());
    }

    #[test]
    fn load_corrupt_file_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.json");
        std::fs::write(&path, b"not valid json {{{{").unwrap();
        let state = BanditState::load(&path);
        assert!(state.arms.is_empty());
    }

    #[test]
    fn load_clamps_out_of_range_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bandit.json");
        // Write state with out-of-range matrix entry.
        std::fs::write(
            &path,
            r#"{"arms":{"p":{"a_matrix":[2e30,0.0,0.0,1.0],"b_vector":[1e30,0.0],"n":1,"total_reward":1.0}},"dim":2,"total_updates":1}"#,
        )
        .unwrap();
        let state = BanditState::load(&path);
        let arm = &state.arms["p"];
        assert!(arm.a_matrix[0] <= 1e9, "should be clamped");
        assert!(arm.b_vector[0] <= 1e9, "should be clamped");
    }

    #[test]
    fn save_leaves_no_tmp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let tmp = path.with_extension("tmp");
        BanditState::new(4).save(&path).unwrap();
        assert!(path.exists());
        assert!(!tmp.exists(), "tmp file must be removed after rename");
    }

    // ── Prune ────────────────────────────────────────────────────────────────

    #[test]
    fn prune_removes_stale_arms() {
        let dim = 2;
        let mut state = BanditState::new(dim);
        let x = vec![1.0f32, 0.0];
        state.update("a", &x, 1.0);
        state.update("b", &x, 1.0);
        state.update("c", &x, 1.0);

        let known: HashSet<String> = ["a".to_owned(), "c".to_owned()].into_iter().collect();
        state.prune(&known);
        assert!(state.arms.contains_key("a"));
        assert!(!state.arms.contains_key("b"));
        assert!(state.arms.contains_key("c"));
    }

    // ── Edge cases ───────────────────────────────────────────────────────────

    #[test]
    fn select_single_provider_returns_it() {
        let mut state = BanditState::new(2);
        state.total_updates = 100;
        let providers = vec!["only".to_owned()];
        let x = vec![1.0f32, 0.0];
        let result = state.select(
            &providers,
            &x,
            1.0,
            10,
            &|_| true,
            0.0,
            &Default::default(),
            None,
            0.9,
        );
        assert_eq!(result.as_deref(), Some("only"));
    }

    #[test]
    fn select_zero_providers_returns_none() {
        let mut state = BanditState::new(2);
        state.total_updates = 100;
        let providers: Vec<String> = vec![];
        let x = vec![1.0f32, 0.0];
        let result = state.select(
            &providers,
            &x,
            1.0,
            0,
            &|_| true,
            0.0,
            &Default::default(),
            None,
            0.9,
        );
        assert!(result.is_none());
    }

    #[test]
    fn dim_one_update_and_select() {
        // Verify that dim=1 works: A is 1×1, b is 1-vector.
        let mut state = BanditState::new(1);
        let x = vec![1.0f32];
        state.update("a", &x, 0.9);
        state.update("b", &x, 0.1);
        let providers = vec!["a".to_owned(), "b".to_owned()];
        // warmup_queries=0 skips cold-start; "a" should win after more reward.
        let result = state.select(
            &providers,
            &x,
            0.0,
            0,
            &|_| true,
            0.0,
            &Default::default(),
            None,
            0.9,
        );
        assert_eq!(
            result.as_deref(),
            Some("a"),
            "a has higher reward, alpha=0 → pure exploit"
        );
    }

    #[test]
    fn ucb_selects_arm_with_higher_score() {
        // After rewarding "high" and punishing "low", UCB with alpha=0 picks "high" (pure exploit).
        let dim = 2;
        let mut state = BanditState::new(dim);
        let x = vec![1.0f32, 0.0];
        for _ in 0..20 {
            state.update("high", &x, 1.0);
            state.update("low", &x, -1.0);
        }
        let providers = vec!["high".to_owned(), "low".to_owned()];
        let result = state.select(
            &providers,
            &x,
            0.0,
            0,
            &|_| true,
            0.0,
            &Default::default(),
            None,
            0.9,
        );
        assert_eq!(
            result.as_deref(),
            Some("high"),
            "pure exploit must pick highest reward arm"
        );
    }

    #[test]
    fn update_increments_total_updates() {
        let mut state = BanditState::new(2);
        assert_eq!(state.total_updates, 0);
        let x = vec![1.0f32, 0.0];
        state.update("a", &x, 0.5);
        assert_eq!(state.total_updates, 1);
        state.update("a", &x, 0.5);
        assert_eq!(state.total_updates, 2);
    }

    #[test]
    fn reward_clamping_via_stats() {
        // Reward is stored as total_reward; arm.update does NOT clamp — clamping is in mod.rs.
        // Verify that very large rewards accumulate correctly in the arm.
        let mut arm = LinUcbArm::new(2);
        let x = vec![1.0f32, 0.0];
        arm.update(&x, 999.0);
        assert!((arm.total_reward - 999.0).abs() < 1e-3);
        assert_eq!(arm.n, 1);
    }

    #[test]
    fn load_mismatched_dim_resets_arm() {
        // If a saved arm has wrong matrix size (e.g. config dim changed), it must be reset.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dim_mismatch.json");
        // Save state with dim=4 but arm with dim=2 data.
        std::fs::write(
            &path,
            r#"{"arms":{"p":{"a_matrix":[1.0,0.0,0.0,1.0],"b_vector":[0.5,0.5],"n":5,"total_reward":2.0}},"dim":4,"total_updates":5}"#,
        )
        .unwrap();
        let state = BanditState::load(&path);
        let arm = &state.arms["p"];
        // Arm must be reset to identity with dim=4.
        assert_eq!(arm.a_matrix.len(), 16, "A must be 4×4 after reset");
        assert_eq!(arm.b_vector.len(), 4, "b must be dim=4 after reset");
        assert_eq!(arm.n, 0, "n must be 0 after reset");
    }

    #[test]
    fn stats_returns_sorted_by_name() {
        let mut state = BanditState::new(2);
        let x = vec![1.0f32, 0.0];
        state.update("zebra", &x, 0.5);
        state.update("apple", &x, 0.9);
        state.update("mango", &x, 0.3);
        let arm_stats = state.stats();
        let names: Vec<&str> = arm_stats.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(names, vec!["apple", "mango", "zebra"]);
    }

    // ── BaRP cost_weight ────────────────────────────────────────────────────

    #[test]
    fn provider_cost_estimate_tiers() {
        // Local / free tier.
        assert!(provider_cost_estimate("my-ollama", "") < 0.3);
        assert!(provider_cost_estimate("provider", "ollama/llama3") < 0.3);
        // Cheap cloud tier.
        assert!(provider_cost_estimate("fast", "gpt-4o-mini") < 0.4);
        assert!(provider_cost_estimate("fast", "claude-haiku-3") < 0.4);
        // Mid tier.
        assert!(provider_cost_estimate("quality", "claude-sonnet-4-6") >= 0.4);
        assert!(provider_cost_estimate("quality", "gpt-4o-2024") >= 0.4);
        // Expensive tier.
        assert!(provider_cost_estimate("best", "claude-opus-4") >= 0.7);
        // Unknown: conservative mid-low default.
        assert_eq!(provider_cost_estimate("unknown-provider", ""), 0.3);
    }

    #[test]
    fn cost_weight_biases_toward_cheap_provider() {
        let dim = 2;
        let mut state = BanditState::new(dim);
        let x = vec![1.0f32, 0.0];
        // Give both providers equal rewards so pure quality is a tie.
        for _ in 0..20 {
            state.update("cheap", &x, 0.5);
            state.update("expensive", &x, 0.5);
        }
        let providers = vec!["cheap".to_owned(), "expensive".to_owned()];
        // Map "expensive" to an expensive model.
        let mut models = std::collections::HashMap::new();
        models.insert("expensive".to_owned(), "claude-opus-4".to_owned());
        models.insert("cheap".to_owned(), "gpt-4o-mini".to_owned());
        // With cost_weight=1.0, cheap provider should be preferred.
        let result = state.select(&providers, &x, 0.0, 0, &|_| true, 1.0, &models, None, 0.9);
        assert_eq!(
            result.as_deref(),
            Some("cheap"),
            "cost_weight=1.0 should prefer cheap provider"
        );
    }

    #[test]
    fn cost_weight_zero_no_bias() {
        let dim = 2;
        let mut state = BanditState::new(dim);
        let x = vec![1.0f32, 0.0];
        for _ in 0..20 {
            state.update("cheap", &x, 0.1);
            state.update("expensive", &x, 0.9);
        }
        let providers = vec!["cheap".to_owned(), "expensive".to_owned()];
        let mut models = std::collections::HashMap::new();
        models.insert("expensive".to_owned(), "claude-opus-4".to_owned());
        models.insert("cheap".to_owned(), "gpt-4o-mini".to_owned());
        // cost_weight=0.0 → pure quality, expensive wins.
        let result = state.select(&providers, &x, 0.0, 0, &|_| true, 0.0, &models, None, 0.9);
        assert_eq!(
            result.as_deref(),
            Some("expensive"),
            "cost_weight=0.0 should pick highest quality"
        );
    }

    // ── MAR memory_hit_confidence ────────────────────────────────────────────

    #[test]
    fn mar_high_confidence_boosts_cheap_provider() {
        let dim = 2;
        let mut state = BanditState::new(dim);
        let x = vec![1.0f32, 0.0];
        // Equal rewards.
        for _ in 0..20 {
            state.update("cheap", &x, 0.5);
            state.update("expensive", &x, 0.5);
        }
        let providers = vec!["cheap".to_owned(), "expensive".to_owned()];
        let mut models = std::collections::HashMap::new();
        models.insert("expensive".to_owned(), "claude-opus-4".to_owned());
        models.insert("cheap".to_owned(), "gpt-4o-mini".to_owned());
        // High memory confidence + cost_weight > 0 → cheap provider boosted.
        let result = state.select(
            &providers,
            &x,
            0.0,
            0,
            &|_| true,
            0.5,
            &models,
            Some(0.95),
            0.9,
        );
        assert_eq!(
            result.as_deref(),
            Some("cheap"),
            "MAR should boost cheap provider on high recall confidence"
        );
    }

    #[test]
    fn mar_low_confidence_no_boost() {
        let dim = 2;
        let mut state = BanditState::new(dim);
        let x = vec![1.0f32, 0.0];
        for _ in 0..20 {
            state.update("cheap", &x, 0.1);
            state.update("expensive", &x, 0.9);
        }
        let providers = vec!["cheap".to_owned(), "expensive".to_owned()];
        let mut models = std::collections::HashMap::new();
        models.insert("expensive".to_owned(), "claude-opus-4".to_owned());
        models.insert("cheap".to_owned(), "gpt-4o-mini".to_owned());
        // Confidence below threshold → no MAR boost, quality wins.
        let result = state.select(
            &providers,
            &x,
            0.0,
            0,
            &|_| true,
            0.5,
            &models,
            Some(0.5),
            0.9,
        );
        assert_eq!(
            result.as_deref(),
            Some("expensive"),
            "below threshold: no MAR boost"
        );
    }

    #[test]
    fn mar_cost_weight_zero_no_boost_even_high_confidence() {
        let dim = 2;
        let mut state = BanditState::new(dim);
        let x = vec![1.0f32, 0.0];
        for _ in 0..20 {
            state.update("cheap", &x, 0.1);
            state.update("expensive", &x, 0.9);
        }
        let providers = vec!["cheap".to_owned(), "expensive".to_owned()];
        let mut models = std::collections::HashMap::new();
        models.insert("expensive".to_owned(), "claude-opus-4".to_owned());
        models.insert("cheap".to_owned(), "gpt-4o-mini".to_owned());
        // cost_weight=0.0 → memory_boost=0 even with high confidence; quality wins.
        let result = state.select(
            &providers,
            &x,
            0.0,
            0,
            &|_| true,
            0.0,
            &models,
            Some(0.99),
            0.9,
        );
        assert_eq!(
            result.as_deref(),
            Some("expensive"),
            "cost_weight=0 → no MAR effect"
        );
    }
}
