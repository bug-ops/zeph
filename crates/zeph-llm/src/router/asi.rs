// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Agent Stability Index (ASI) — per-provider coherence tracking.
//!
//! Maintains a sliding window of response embeddings per provider and computes a coherence
//! score as the cosine similarity of the latest embedding vs. the window mean. Low coherence
//! signals that a provider's responses are drifting (e.g. degraded model quality, context
//! overflow, prompt sensitivity). The score is used by `RouterProvider` to penalize Thompson
//! beta priors and EMA scores for incoherent providers.
//!
//! # Design Notes
//!
//! - State is session-only: no persistence, no cross-session accumulation.
//! - `push_embedding` is called from a background `tokio::spawn` task (fire-and-forget).
//!   Under high request rates the coherence score used for routing may lag 1–2 responses
//!   behind. With `window = 5` this lag is acceptable — coherence is a slow-moving signal.
//! - `coherence()` returns `1.0` until at least 2 embeddings have been observed (no penalty
//!   during warm-up).

use std::collections::{HashMap, VecDeque};

use zeph_common::math::cosine_similarity;

/// Per-provider sliding window of response embeddings + derived coherence score.
#[derive(Debug, Clone)]
struct AsiWindow {
    embeddings: VecDeque<Vec<f32>>,
    coherence_score: f32,
}

impl AsiWindow {
    fn new(capacity: usize) -> Self {
        Self {
            embeddings: VecDeque::with_capacity(capacity),
            coherence_score: 1.0,
        }
    }
}

/// Per-provider ASI state. Shared via `Arc<Mutex<AsiState>>` in `RouterProvider`.
#[derive(Debug, Clone, Default)]
pub struct AsiState {
    windows: HashMap<String, AsiWindow>,
}

impl AsiState {
    /// Add a response embedding for `provider`. Evicts the oldest entry when the window
    /// exceeds `window_size`, then recomputes the coherence score.
    pub fn push_embedding(&mut self, provider: &str, embedding: Vec<f32>, window_size: usize) {
        let window = self
            .windows
            .entry(provider.to_owned())
            .or_insert_with(|| AsiWindow::new(window_size));

        if window.embeddings.len() >= window_size {
            window.embeddings.pop_front();
        }
        window.embeddings.push_back(embedding);
        window.coherence_score = compute_coherence(&window.embeddings);
    }

    /// Return the current coherence score for `provider`.
    ///
    /// Returns `1.0` when fewer than 2 embeddings have been observed (no penalty during
    /// warm-up) or when `provider` is unknown.
    #[must_use]
    pub fn coherence(&self, provider: &str) -> f32 {
        self.windows
            .get(provider)
            .map_or(1.0, |w| w.coherence_score)
    }
}

/// Cosine similarity of the latest embedding in `window` vs. the element-wise mean of all
/// embeddings in the window. Returns `1.0` when fewer than 2 embeddings are present.
fn compute_coherence(window: &VecDeque<Vec<f32>>) -> f32 {
    if window.len() < 2 {
        return 1.0;
    }
    let latest = window.back().expect("len >= 2 guarantees back exists");
    let mean = mean_embedding(window);
    if mean.is_empty() {
        return 1.0;
    }
    cosine_similarity(latest, &mean)
}

/// Element-wise mean of all embeddings in `window`. Returns an empty vec when `window` is
/// empty or embeddings have inconsistent dimensions.
fn mean_embedding(window: &VecDeque<Vec<f32>>) -> Vec<f32> {
    let Some(dim) = window.front().map(Vec::len) else {
        return vec![];
    };
    if dim == 0 || window.iter().any(|e| e.len() != dim) {
        return vec![];
    }
    #[allow(clippy::cast_precision_loss)]
    let n = window.len() as f32;
    let mut mean = vec![0.0_f32; dim];
    for emb in window {
        for (m, &v) in mean.iter_mut().zip(emb.iter()) {
            *m += v;
        }
    }
    for m in &mut mean {
        *m /= n;
    }
    mean
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asi_state_returns_one_before_warmup() {
        let state = AsiState::default();
        assert!((state.coherence("unknown-provider") - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn asi_state_high_coherence_for_identical_embeddings() {
        let mut state = AsiState::default();
        let emb = vec![1.0_f32, 0.0, 0.0];
        state.push_embedding("p1", emb.clone(), 5);
        state.push_embedding("p1", emb.clone(), 5);
        state.push_embedding("p1", emb, 5);
        let c = state.coherence("p1");
        assert!(
            c > 0.99,
            "coherence should be ~1.0 for identical embeddings, got {c}"
        );
    }

    #[test]
    fn asi_state_low_coherence_for_orthogonal_embeddings() {
        let mut state = AsiState::default();
        // Alternating orthogonal vectors → window mean ≈ [0.5, 0.5, 0], latest ≈ not aligned.
        state.push_embedding("p1", vec![1.0, 0.0, 0.0], 5);
        state.push_embedding("p1", vec![0.0, 1.0, 0.0], 5);
        state.push_embedding("p1", vec![1.0, 0.0, 0.0], 5);
        state.push_embedding("p1", vec![0.0, 1.0, 0.0], 5);
        let c = state.coherence("p1");
        // mean ≈ [0.5, 0.5, 0], latest = [0, 1, 0] → similarity < 1.0
        assert!(
            c < 0.95,
            "coherence should be below 0.95 for alternating vectors, got {c}"
        );
    }

    #[test]
    fn asi_state_window_evicts_oldest() {
        let mut state = AsiState::default();
        let window_size = 3;
        for _ in 0..5 {
            state.push_embedding("p1", vec![1.0, 0.0], window_size);
        }
        let window = &state.windows["p1"];
        assert_eq!(window.embeddings.len(), window_size);
    }
}
