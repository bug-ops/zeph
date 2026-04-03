// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! REINFORCE-based RL head for skill re-ranking (`SkillOrchestra`).
//!
//! Maintains a weight vector over skill slots and uses the REINFORCE policy gradient
//! to shift weights toward skills that receive positive reward.

/// Read a length-prefixed `f32` slice from a raw byte blob.
///
/// Format: 4-byte little-endian `u32` length, followed by `len * 4` bytes of `f32` data.
/// Returns `None` if the blob is malformed or the length exceeds the OOM cap.
#[must_use]
pub fn read_f32_slice(blob: &[u8]) -> Option<Vec<f32>> {
    if blob.len() < 4 {
        return None;
    }
    let len = u32::from_le_bytes(blob[..4].try_into().ok()?) as usize;
    if len > 1_000_000 {
        return None;
    }
    let data = blob.get(4..4 + len * 4)?;
    let mut out = Vec::with_capacity(len);
    for chunk in data.chunks_exact(4) {
        out.push(f32::from_le_bytes(chunk.try_into().ok()?));
    }
    Some(out)
}

/// REINFORCE RL head: maintains per-skill log-weights and updates them via policy gradient.
#[derive(Debug, Clone)]
pub struct SkillOrchestra {
    /// Log-scale weights for each skill slot (index-aligned with the registry).
    weights: Vec<f32>,
    /// Number of `update()` calls received so far.
    update_count: u64,
    /// Minimum updates before `rerank()` blends RL weights instead of returning cosine scores.
    warmup_updates: u64,
    /// Learning rate for REINFORCE gradient steps.
    learning_rate: f32,
}

impl SkillOrchestra {
    /// Create a new `SkillOrchestra` with `n` skill slots.
    #[must_use]
    pub fn new(n: usize, warmup_updates: u64, learning_rate: f32) -> Self {
        Self {
            weights: vec![0.0_f32; n],
            update_count: 0,
            warmup_updates,
            learning_rate,
        }
    }

    /// Number of skill slots tracked.
    #[must_use]
    pub fn len(&self) -> usize {
        self.weights.len()
    }

    /// Returns `true` if there are no skill slots.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.weights.is_empty()
    }

    /// Apply a REINFORCE gradient update.
    ///
    /// `skill_index` is the index of the selected skill.
    /// `embedding` is the query embedding used when the skill was selected (policy input).
    /// `reward` is the scalar reward signal (positive = good, negative = bad).
    ///
    /// The update follows REINFORCE: `w[i] += lr * reward * grad_log_pi`,
    /// where `grad_log_pi` for slot `i` is `(i == selected) - pi[i]`.
    pub fn update(&mut self, skill_index: usize, embedding: &[f32], reward: f32) {
        if skill_index >= self.weights.len() {
            return;
        }
        // Compute softmax probabilities from current weights scaled by embedding norm.
        let scale = embedding_norm(embedding).max(1e-8);
        let logits: Vec<f32> = self
            .weights
            .iter()
            .enumerate()
            .map(|(i, &w)| {
                let e = embedding.get(i % embedding.len()).copied().unwrap_or(0.0);
                w + e / scale
            })
            .collect();
        let probs = softmax(&logits);

        // REINFORCE gradient: grad_log_pi[i] = (i == selected) - pi[i]
        for (i, p) in probs.iter().enumerate() {
            let indicator = if i == skill_index { 1.0_f32 } else { 0.0_f32 };
            self.weights[i] += self.learning_rate * reward * (indicator - p);
        }
        self.update_count += 1;
    }

    /// Re-rank cosine scores by blending with the RL weight policy.
    ///
    /// During warmup (`update_count < warmup_updates`) returns `cosine_scores` unchanged.
    /// After warmup, blends: `score[i] = 0.5 * cosine[i] + 0.5 * softmax(weights)[i]`.
    ///
    /// Returns `None` if `cosine_scores.len() != self.weights.len()`.
    #[must_use]
    pub fn rerank(&self, cosine_scores: &[f32]) -> Option<Vec<f32>> {
        if cosine_scores.len() != self.weights.len() {
            return None;
        }
        if self.update_count < self.warmup_updates {
            return Some(cosine_scores.to_vec());
        }
        let rl_probs = softmax(&self.weights);
        let blended: Vec<f32> = cosine_scores
            .iter()
            .zip(rl_probs.iter())
            .map(|(&c, &r)| 0.5 * c + 0.5 * r)
            .collect();
        Some(blended)
    }

    /// Number of updates received so far.
    #[must_use]
    pub fn update_count(&self) -> u64 {
        self.update_count
    }
}

fn embedding_norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

fn softmax(logits: &[f32]) -> Vec<f32> {
    if logits.is_empty() {
        return vec![];
    }
    let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let exps: Vec<f32> = logits.iter().map(|&x| (x - max).exp()).collect();
    let sum: f32 = exps.iter().sum();
    exps.iter().map(|&e| e / sum).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_f32_slice_empty_blob() {
        assert!(read_f32_slice(&[]).is_none());
    }

    #[test]
    fn read_f32_slice_too_short() {
        assert!(read_f32_slice(&[0, 0, 0]).is_none());
    }

    #[test]
    fn read_f32_slice_zero_length() {
        let blob = [0u8; 4]; // len = 0
        let result = read_f32_slice(&blob).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn read_f32_slice_oom_cap() {
        // len = 1_000_001 encoded in 4 bytes LE
        let mut blob = (1_000_001u32).to_le_bytes().to_vec();
        // Pad enough bytes so the length field is parseable but cap kicks in first.
        blob.extend_from_slice(&[0u8; 8]);
        assert!(read_f32_slice(&blob).is_none());
    }

    #[test]
    fn read_f32_slice_at_cap_boundary() {
        // len = 1_000_000 should be rejected (> check is strictly greater).
        // The blob would need 4M bytes of data which we don't allocate; just check the guard.
        let len = 1_000_000u32;
        let mut blob = len.to_le_bytes().to_vec();
        // We only need enough to hit the cap check (which happens before with_capacity).
        // Since len == 1_000_000 is NOT > 1_000_000, the cap does not trigger.
        // The blob is truncated so the data read fails → None via get().
        blob.extend_from_slice(&[0u8; 4]); // only 1 float worth of data, not enough for 1M
        assert!(read_f32_slice(&blob).is_none());
    }

    #[test]
    fn read_f32_slice_valid_two_floats() {
        let values: [f32; 2] = [1.0, -2.5];
        let mut blob = 2u32.to_le_bytes().to_vec();
        for v in values {
            blob.extend_from_slice(&v.to_le_bytes());
        }
        let result = read_f32_slice(&blob).unwrap();
        assert_eq!(result.len(), 2);
        assert!((result[0] - 1.0).abs() < f32::EPSILON);
        assert!((result[1] - (-2.5)).abs() < f32::EPSILON);
    }

    #[test]
    fn warm_path_rerank_uses_blended_scores() {
        // 3 skill slots, warmup = 1 update, lr = 0.1
        let mut orchestra = SkillOrchestra::new(3, 1, 0.1);
        let embedding = vec![0.5, 0.5, 0.5];
        // Trigger warmup completion with one update (positive reward for slot 0).
        orchestra.update(0, &embedding, 1.0);
        assert!(orchestra.update_count() >= 1);

        let cosine = vec![0.8_f32, 0.5, 0.3];
        let blended = orchestra.rerank(&cosine).unwrap();

        // After warmup, blended scores must not equal cosine scores exactly.
        let is_pure_cosine = blended
            .iter()
            .zip(cosine.iter())
            .all(|(b, c)| (b - c).abs() < f32::EPSILON);
        assert!(
            !is_pure_cosine,
            "expected blended scores to differ from pure cosine after warmup"
        );
    }

    #[test]
    fn negative_reward_moves_weights_opposite_to_positive() {
        let mut pos_orch = SkillOrchestra::new(3, 10, 0.5);
        let mut neg_orch = SkillOrchestra::new(3, 10, 0.5);
        let embedding = vec![1.0, 0.0, 0.0];

        pos_orch.update(0, &embedding, 1.0);
        neg_orch.update(0, &embedding, -1.0);

        // Weight for selected slot should be higher after positive reward and lower after negative.
        assert!(
            pos_orch.weights[0] > neg_orch.weights[0],
            "positive reward should increase weight more than negative reward"
        );
    }

    #[test]
    fn rerank_before_warmup_returns_cosine_unchanged() {
        let orchestra = SkillOrchestra::new(3, 5, 0.1);
        let cosine = vec![0.9_f32, 0.4, 0.1];
        let result = orchestra.rerank(&cosine).unwrap();
        for (r, c) in result.iter().zip(cosine.iter()) {
            assert!((r - c).abs() < f32::EPSILON);
        }
    }

    #[test]
    fn rerank_length_mismatch_returns_none() {
        let orchestra = SkillOrchestra::new(3, 0, 0.1);
        assert!(orchestra.rerank(&[0.5, 0.5]).is_none());
    }
}
