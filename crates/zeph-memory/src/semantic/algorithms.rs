// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::types::MessageId;
use zeph_common::math::cosine_similarity;

#[allow(clippy::implicit_hasher)]
pub fn apply_temporal_decay(
    ranked: &mut [(MessageId, f64)],
    timestamps: &std::collections::HashMap<MessageId, i64>,
    half_life_days: u32,
) {
    if half_life_days == 0 {
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .cast_signed();
    let lambda = std::f64::consts::LN_2 / f64::from(half_life_days);

    for (msg_id, score) in ranked.iter_mut() {
        if let Some(&ts) = timestamps.get(msg_id) {
            #[allow(clippy::cast_precision_loss)]
            let age_days = (now - ts).max(0) as f64 / 86400.0;
            *score *= (-lambda * age_days).exp();
        }
    }
}

#[allow(clippy::implicit_hasher)]
pub fn apply_mmr(
    ranked: &[(MessageId, f64)],
    vectors: &std::collections::HashMap<MessageId, Vec<f32>>,
    lambda: f32,
    limit: usize,
) -> Vec<(MessageId, f64)> {
    if ranked.is_empty() || limit == 0 {
        return Vec::new();
    }

    tracing::debug!(
        candidates = ranked.len(),
        limit,
        lambda = %lambda,
        "mmr: starting re-ranking"
    );

    let lambda = f64::from(lambda);
    let mut selected: Vec<(MessageId, f64)> = Vec::with_capacity(limit);
    let mut remaining: Vec<(MessageId, f64)> = ranked.to_vec();

    while selected.len() < limit && !remaining.is_empty() {
        let best_idx = if selected.is_empty() {
            // Pick highest relevance first
            0
        } else {
            let mut best = 0usize;
            let mut best_score = f64::NEG_INFINITY;

            for (i, &(cand_id, relevance)) in remaining.iter().enumerate() {
                let max_sim = if let Some(cand_vec) = vectors.get(&cand_id) {
                    selected
                        .iter()
                        .filter_map(|(sel_id, _)| vectors.get(sel_id))
                        .map(|sel_vec| f64::from(cosine_similarity(cand_vec, sel_vec)))
                        .fold(f64::NEG_INFINITY, f64::max)
                } else {
                    0.0
                };
                let max_sim = if max_sim == f64::NEG_INFINITY {
                    0.0
                } else {
                    max_sim
                };
                let mmr_score = lambda * relevance - (1.0 - lambda) * max_sim;
                if mmr_score > best_score {
                    best_score = mmr_score;
                    best = i;
                }
            }
            best
        };

        selected.push(remaining.remove(best_idx));
    }

    tracing::debug!(selected = selected.len(), "mmr: re-ranking complete");

    selected
}
