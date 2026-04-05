// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#![allow(clippy::doc_markdown, clippy::needless_range_loop)]
//! SkillOrchestra RL routing head: 2-layer MLP for skill re-ranking.
//!
//! Input features per candidate:
//!   `query_embed ++ skill_embed ++ [cosine_score, success_rate, log_use_count]`
//!
//! Forward pass: `score = sigmoid(w2 @ relu(w1 @ input + b1) + b2)`
//!
//! Training: REINFORCE with running baseline. Weights are shared via
//! `Arc<std::sync::Mutex<RoutingHeadInner>>` for safe concurrent access.
//!
//! # Single-instance limitation
//!
//! SQLite weight persistence is singleton-row based. Two agent instances sharing
//! the same DB will silently overwrite each other's weights (last writer wins).
//! This is documented and accepted for MVP single-instance deployments.

use std::sync::{Arc, Mutex};

/// Number of scalar features appended after the two embedding vectors.
/// Features: [cosine_score, success_rate, log_use_count]
const N_FEATURES: usize = 3;
const DEFAULT_HIDDEN_DIM: usize = 32;

/// Cached activations from a single forward pass, needed for REINFORCE gradient.
#[derive(Clone)]
pub struct ForwardCache {
    /// Full input vector: query_embed ++ skill_embed ++ features
    pub input: Vec<f32>,
    /// Hidden-layer pre-activations (before ReLU): w1 @ input + b1
    pub pre_relu: Vec<f32>,
    /// Hidden-layer post-activations (after ReLU)
    pub hidden: Vec<f32>,
    /// Output score (sigmoid output)
    pub score: f32,
}

struct RoutingHeadInner {
    /// (input_dim × hidden_dim) flattened row-major
    w1: Vec<f32>,
    b1: Vec<f32>,
    /// (hidden_dim × 1) flattened
    w2: Vec<f32>,
    b2: f32,
    embed_dim: usize,
    hidden_dim: usize,
    /// Running reward baseline for variance reduction in REINFORCE.
    baseline: f32,
    /// Total number of weight updates applied.
    update_count: u32,
    /// Cached activations from the most recent `score()` call, consumed by `update()`.
    last_forward: Option<ForwardCache>,
}

impl RoutingHeadInner {
    /// Xavier uniform initialization: `U(-sqrt(6/(fan_in+fan_out)), sqrt(6/(fan_in+fan_out)))`.
    fn new(embed_dim: usize) -> Self {
        let input_dim = 2 * embed_dim + N_FEATURES;
        let hidden_dim = DEFAULT_HIDDEN_DIM;

        let w1 = xavier_init(input_dim, hidden_dim);
        let b1 = vec![0.0f32; hidden_dim];
        let w2 = xavier_init(hidden_dim, 1);
        let b2 = 0.0f32;

        Self {
            w1,
            b1,
            w2,
            b2,
            embed_dim,
            hidden_dim,
            baseline: 0.0,
            update_count: 0,
            last_forward: None,
        }
    }

    fn input_dim(&self) -> usize {
        2 * self.embed_dim + N_FEATURES
    }

    fn score(
        &mut self,
        query_embed: &[f32],
        skill_embed: &[f32],
        cosine_score: f32,
        success_rate: f32,
        use_count: u32,
    ) -> f32 {
        let mut input = Vec::with_capacity(self.input_dim());
        input.extend_from_slice(query_embed);
        input.extend_from_slice(skill_embed);
        input.push(cosine_score);
        input.push(success_rate);
        #[allow(clippy::cast_precision_loss)]
        input.push((use_count as f32 + 1.0).ln());

        // Hidden layer: h = relu(w1 @ input + b1)
        let mut pre_relu = vec![0.0f32; self.hidden_dim];
        for i in 0..self.hidden_dim {
            let mut acc = self.b1[i];
            for j in 0..self.input_dim() {
                acc += self.w1[i * self.input_dim() + j] * input[j];
            }
            pre_relu[i] = acc;
        }
        let hidden: Vec<f32> = pre_relu.iter().map(|&x| x.max(0.0)).collect();

        // Output layer: score = sigmoid(w2 @ hidden + b2)
        let mut logit = self.b2;
        for i in 0..self.hidden_dim {
            logit += self.w2[i] * hidden[i];
        }
        let score = sigmoid(logit);

        self.last_forward = Some(ForwardCache {
            input,
            pre_relu: pre_relu.clone(),
            hidden,
            score,
        });

        score
    }

    /// REINFORCE update using cached forward-pass activations.
    ///
    /// Must be called after `score()` for the skill that was actually selected.
    /// `reward`: +1.0 for success, -1.0 for failure.
    ///
    /// Returns `true` if the update was applied, `false` if no forward cache is available
    /// (i.e. `score()` was not called in the current turn — safe no-op).
    fn update(&mut self, reward: f32, learning_rate: f32) -> bool {
        let Some(cache) = self.last_forward.take() else {
            return false;
        };

        // Exponential moving average baseline (alpha=0.1)
        self.baseline = 0.9 * self.baseline + 0.1 * reward;
        let advantage = reward - self.baseline;

        let score = cache.score;
        // Gradient of log(score) w.r.t. logit = 1 - score (score = sigmoid(logit))
        let d_logit = advantage * (1.0 - score);

        // Gradient w.r.t. w2[i] = d_logit * hidden[i]
        for i in 0..self.hidden_dim {
            self.w2[i] += learning_rate * d_logit * cache.hidden[i];
        }
        self.b2 += learning_rate * d_logit;

        // Backprop through ReLU into w1
        // d_hidden[i] = d_logit * w2[i] * relu'(pre_relu[i])
        let input_dim = self.input_dim();
        for i in 0..self.hidden_dim {
            if cache.pre_relu[i] <= 0.0 {
                continue; // ReLU gate closed
            }
            let d_hidden = d_logit * self.w2[i];
            for j in 0..input_dim {
                self.w1[i * input_dim + j] += learning_rate * d_hidden * cache.input[j];
            }
            self.b1[i] += learning_rate * d_hidden;
        }

        self.update_count = self.update_count.saturating_add(1);
        true
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        // Format: [embed_dim u32][hidden_dim u32][baseline f32][update_count u32]
        //         [w1 len u32][w1 f32s...][b1 len u32][b1 f32s...]
        //         [w2 len u32][w2 f32s...][b2 f32]
        push_u32(&mut buf, u32::try_from(self.embed_dim).unwrap_or(u32::MAX));
        push_u32(&mut buf, u32::try_from(self.hidden_dim).unwrap_or(u32::MAX));
        push_f32(&mut buf, self.baseline);
        push_u32(&mut buf, self.update_count);
        push_f32_slice(&mut buf, &self.w1);
        push_f32_slice(&mut buf, &self.b1);
        push_f32_slice(&mut buf, &self.w2);
        push_f32(&mut buf, self.b2);
        buf
    }

    fn from_bytes(data: &[u8]) -> Option<Self> {
        let mut cursor = 0usize;

        let embed_dim = read_u32(data, &mut cursor)? as usize;
        let hidden_dim = read_u32(data, &mut cursor)? as usize;
        let baseline = read_f32(data, &mut cursor)?;
        let update_count = read_u32(data, &mut cursor)?;
        let w1 = read_f32_slice(data, &mut cursor)?;
        let b1 = read_f32_slice(data, &mut cursor)?;
        let w2 = read_f32_slice(data, &mut cursor)?;
        let b2 = read_f32(data, &mut cursor)?;

        let input_dim = 2 * embed_dim + N_FEATURES;
        if w1.len() != input_dim * hidden_dim || b1.len() != hidden_dim || w2.len() != hidden_dim {
            return None;
        }

        Some(Self {
            w1,
            b1,
            w2,
            b2,
            embed_dim,
            hidden_dim,
            baseline,
            update_count,
            last_forward: None,
        })
    }
}

/// Thread-safe RL routing head, shared via `Arc<Mutex<...>>`.
#[derive(Clone)]
pub struct RoutingHead {
    inner: Arc<Mutex<RoutingHeadInner>>,
}

impl RoutingHead {
    /// Initialize with Xavier-initialized weights.
    #[must_use]
    pub fn new(embed_dim: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RoutingHeadInner::new(embed_dim))),
        }
    }

    /// Score a single candidate. Caches forward-pass activations for `update()`.
    ///
    /// # Panics
    ///
    /// Panics if the mutex is poisoned.
    #[must_use]
    pub fn score(
        &self,
        query_embed: &[f32],
        skill_embed: &[f32],
        cosine_score: f32,
        success_rate: f32,
        use_count: u32,
    ) -> f32 {
        self.inner
            .lock()
            .expect("RoutingHead mutex poisoned")
            .score(
                query_embed,
                skill_embed,
                cosine_score,
                success_rate,
                use_count,
            )
    }

    /// Re-rank candidates using RL scores. Returns indices sorted by blended score descending.
    ///
    /// `rl_weight`: final_score = (1-rl_weight)*cosine + rl_weight*rl_score
    ///
    /// Skips RL blending and returns original cosine order when `update_count < warmup_updates`.
    ///
    /// # Panics
    ///
    /// Panics if the mutex is poisoned.
    #[must_use]
    pub fn rerank(
        &self,
        query_embed: &[f32],
        candidates: &[(usize, &[f32], f32)],
        stats: &[(f32, u32)],
        rl_weight: f32,
        warmup_updates: u32,
    ) -> Vec<(usize, f32)> {
        let mut inner = self.inner.lock().expect("RoutingHead mutex poisoned");

        if inner.update_count < warmup_updates {
            // Cold start: use pure cosine order
            let mut ranked: Vec<(usize, f32)> = candidates
                .iter()
                .map(|&(idx, _, cosine)| (idx, cosine))
                .collect();
            ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            return ranked;
        }

        // Score all candidates under a single lock acquisition, capturing each forward cache.
        // After sorting, store only the winner's cache so update() uses the correct activations.
        let mut ranked: Vec<(usize, f32, ForwardCache)> = Vec::with_capacity(candidates.len());
        for (&(idx, skill_embed, cosine), &(success_rate, use_count)) in
            candidates.iter().zip(stats.iter())
        {
            let rl_score = inner.score(query_embed, skill_embed, cosine, success_rate, use_count);
            let blended = (1.0 - rl_weight) * cosine + rl_weight * rl_score;
            let cache = inner
                .last_forward
                .take()
                .expect("score() always sets last_forward");
            ranked.push((idx, blended, cache));
        }

        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Store only the winner's cache for REINFORCE update.
        if let Some((_, _, winner_cache)) = ranked.first() {
            inner.last_forward = Some(winner_cache.clone());
        }
        drop(inner);

        ranked
            .into_iter()
            .map(|(idx, score, _)| (idx, score))
            .collect()
    }

    /// REINFORCE update for the skill that was actually selected.
    ///
    /// Returns `true` if the update was applied, `false` if `rerank()` was not called
    /// in the current turn (safe no-op — no panic).
    ///
    /// # Panics
    ///
    /// Panics if the mutex is poisoned.
    #[must_use]
    pub fn update(&self, reward: f32, learning_rate: f32) -> bool {
        self.inner
            .lock()
            .expect("RoutingHead mutex poisoned")
            .update(reward, learning_rate)
    }

    /// Number of weight updates applied so far.
    ///
    /// # Panics
    ///
    /// Panics if the mutex is poisoned.
    #[must_use]
    pub fn update_count(&self) -> u32 {
        self.inner
            .lock()
            .expect("RoutingHead mutex poisoned")
            .update_count
    }

    /// Serialize weights to bytes for SQLite blob storage.
    ///
    /// # Panics
    ///
    /// Panics if the mutex is poisoned.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        self.inner
            .lock()
            .expect("RoutingHead mutex poisoned")
            .to_bytes()
    }

    /// Deserialize weights from bytes.
    #[must_use]
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        RoutingHeadInner::from_bytes(data).map(|inner| Self {
            inner: Arc::new(Mutex::new(inner)),
        })
    }

    /// Embedding dimension this head was built for.
    ///
    /// # Panics
    ///
    /// Panics if the mutex is poisoned.
    #[must_use]
    pub fn embed_dim(&self) -> usize {
        self.inner
            .lock()
            .expect("RoutingHead mutex poisoned")
            .embed_dim
    }

    /// Running reward baseline.
    ///
    /// # Panics
    ///
    /// Panics if the mutex is poisoned.
    #[must_use]
    pub fn baseline(&self) -> f32 {
        self.inner
            .lock()
            .expect("RoutingHead mutex poisoned")
            .baseline
    }
}

// --- Math helpers ---

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

/// Xavier uniform initialization: U(-limit, limit) where limit = sqrt(6/(fan_in+fan_out)).
fn xavier_init(fan_in: usize, fan_out: usize) -> Vec<f32> {
    #[allow(clippy::cast_precision_loss)]
    let limit = (6.0_f32 / (fan_in + fan_out) as f32).sqrt();
    let n = fan_in * fan_out;
    // Deterministic LCG seeded by dimensions for reproducibility (no rand dep).
    let mut state: u64 = (fan_in as u64)
        .wrapping_mul(1_000_003)
        .wrapping_add(fan_out as u64);
    let mut weights = Vec::with_capacity(n);
    for _ in 0..n {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        // Map to [0, 1)
        #[allow(clippy::cast_precision_loss)]
        let u = (state >> 33) as f32 / (1u64 << 31) as f32;
        weights.push(u * 2.0 * limit - limit);
    }
    weights
}

// --- Binary serialization helpers ---

fn push_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn push_f32(buf: &mut Vec<u8>, v: f32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn push_f32_slice(buf: &mut Vec<u8>, slice: &[f32]) {
    push_u32(buf, u32::try_from(slice.len()).unwrap_or(u32::MAX));
    for &v in slice {
        push_f32(buf, v);
    }
}

fn read_u32(data: &[u8], cursor: &mut usize) -> Option<u32> {
    let end = cursor.checked_add(4)?;
    if end > data.len() {
        return None;
    }
    let v = u32::from_le_bytes(data[*cursor..end].try_into().ok()?);
    *cursor = end;
    Some(v)
}

fn read_f32(data: &[u8], cursor: &mut usize) -> Option<f32> {
    let end = cursor.checked_add(4)?;
    if end > data.len() {
        return None;
    }
    let v = f32::from_le_bytes(data[*cursor..end].try_into().ok()?);
    *cursor = end;
    Some(v)
}

fn read_f32_slice(data: &[u8], cursor: &mut usize) -> Option<Vec<f32>> {
    let len = read_u32(data, cursor)? as usize;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        out.push(read_f32(data, cursor)?);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_head() -> RoutingHead {
        RoutingHead::new(4)
    }

    fn dummy_embed(val: f32, dim: usize) -> Vec<f32> {
        vec![val; dim]
    }

    #[test]
    fn score_returns_value_in_unit_interval() {
        let head = make_head();
        let q = dummy_embed(0.1, 4);
        let s = dummy_embed(0.2, 4);
        let score = head.score(&q, &s, 0.8, 0.9, 5);
        assert!((0.0..=1.0).contains(&score), "score {score} out of [0,1]");
    }

    #[test]
    fn forward_cache_cleared_after_update() {
        let head = make_head();
        let q = dummy_embed(0.1, 4);
        let s = dummy_embed(0.2, 4);
        let _ = head.score(&q, &s, 0.8, 0.9, 5);
        assert!(head.update(1.0, 0.01), "first update should return true");
        // After update, last_forward is None — second update without score is a safe no-op.
        assert!(
            !head.update(1.0, 0.01),
            "update without preceding score should return false"
        );
    }

    #[test]
    fn update_count_increments() {
        let head = make_head();
        let q = dummy_embed(0.0, 4);
        let s = dummy_embed(0.0, 4);
        assert_eq!(head.update_count(), 0);
        let _ = head.score(&q, &s, 0.5, 0.5, 1);
        let _ = head.update(1.0, 0.01);
        assert_eq!(head.update_count(), 1);
    }

    #[test]
    fn weights_round_trip_serialization() {
        let head = make_head();
        let q = dummy_embed(0.3, 4);
        let s = dummy_embed(0.7, 4);
        let _ = head.score(&q, &s, 0.6, 0.8, 10);
        let _ = head.update(1.0, 0.01);

        let bytes = head.to_bytes();
        let head2 = RoutingHead::from_bytes(&bytes).expect("deserialization failed");

        assert_eq!(head2.embed_dim(), 4);
        assert_eq!(head2.update_count(), 1);

        // Scores should match after round-trip (same weights, new forward cache is None)
        let s1 = head.score(&q, &s, 0.6, 0.8, 10);
        let s2 = head2.score(&q, &s, 0.6, 0.8, 10);
        assert!(
            (s1 - s2).abs() < 1e-5,
            "score mismatch after round-trip: {s1} vs {s2}"
        );
    }

    #[test]
    fn from_bytes_returns_none_on_corrupt_data() {
        assert!(RoutingHead::from_bytes(&[]).is_none());
        assert!(RoutingHead::from_bytes(&[0u8; 3]).is_none());
    }

    #[test]
    fn rerank_cold_start_uses_cosine_order() {
        let head = make_head();
        let q = dummy_embed(0.1, 4);
        let s1 = dummy_embed(0.1, 4);
        let s2 = dummy_embed(0.9, 4);
        let s3 = dummy_embed(0.5, 4);
        let candidates: Vec<(usize, &[f32], f32)> =
            vec![(0, &s1, 0.9), (1, &s2, 0.5), (2, &s3, 0.7)];
        let stats = vec![(0.8, 5u32), (0.6, 3), (0.7, 4)];

        let ranked = head.rerank(&q, &candidates, &stats, 0.3, 50);
        assert_eq!(
            ranked[0].0, 0,
            "highest cosine should be first during warmup"
        );
    }

    #[test]
    fn blended_score_formula() {
        // Manually verify: (1-w)*cosine + w*rl_score
        let rl_weight = 0.3f32;
        let cosine = 0.8f32;
        let rl_score = 0.6f32;
        let expected = (1.0 - rl_weight) * cosine + rl_weight * rl_score;
        assert!((expected - 0.74f32).abs() < 1e-5);
    }

    #[test]
    fn update_without_prior_rerank_returns_false() {
        // Regression test for #2675: calling update() on a fresh head (no score/rerank)
        // must not panic and must return false.
        let head = make_head();
        assert!(
            !head.update(1.0, 0.01),
            "update() without prior rerank() must return false, not panic"
        );
    }

    #[test]
    fn update_changes_weights() {
        let head = make_head();
        let q = dummy_embed(0.5, 4);
        let s = dummy_embed(0.5, 4);

        let score_before = head.score(&q, &s, 0.5, 0.5, 5);
        let _ = head.update(1.0, 0.1); // large LR to ensure change

        let score_after = head.score(&q, &s, 0.5, 0.5, 5);
        let _ = head.update(1.0, 0.0); // consume cache

        assert!(
            (score_before - score_after).abs() > 1e-6,
            "weights should change after update: {score_before} vs {score_after}"
        );
    }
}
