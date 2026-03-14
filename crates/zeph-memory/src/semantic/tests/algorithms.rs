// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::types::MessageId;

use super::super::*;

#[test]
fn temporal_decay_disabled_leaves_scores_unchanged() {
    let mut ranked = vec![(MessageId(1), 1.0f64), (MessageId(2), 0.5f64)];
    let timestamps = std::collections::HashMap::new();
    apply_temporal_decay(&mut ranked, &timestamps, 30);
    assert!((ranked[0].1 - 1.0).abs() < f64::EPSILON);
    assert!((ranked[1].1 - 0.5).abs() < f64::EPSILON);
}

#[test]
fn temporal_decay_zero_age_preserves_score() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .cast_signed();
    let mut ranked = vec![(MessageId(1), 1.0f64)];
    let mut timestamps = std::collections::HashMap::new();
    timestamps.insert(MessageId(1), now);
    apply_temporal_decay(&mut ranked, &timestamps, 30);
    assert!((ranked[0].1 - 1.0).abs() < 0.01);
}

#[test]
fn temporal_decay_half_life_halves_score() {
    let half_life = 30u32;
    let age_secs = i64::from(half_life) * 86400;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .cast_signed();
    let ts = now - age_secs;
    let mut ranked = vec![(MessageId(1), 1.0f64)];
    let mut timestamps = std::collections::HashMap::new();
    timestamps.insert(MessageId(1), ts);
    apply_temporal_decay(&mut ranked, &timestamps, half_life);
    assert!(
        (ranked[0].1 - 0.5).abs() < 0.01,
        "score was {}",
        ranked[0].1
    );
}

#[test]
fn temporal_decay_half_life_zero_is_noop() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .cast_signed();
    let age_secs = 30i64 * 86400;
    let ts = now - age_secs;
    let mut ranked = vec![(MessageId(1), 1.0f64)];
    let mut timestamps = std::collections::HashMap::new();
    timestamps.insert(MessageId(1), ts);
    apply_temporal_decay(&mut ranked, &timestamps, 0);
    assert!(
        (ranked[0].1 - 1.0).abs() < f64::EPSILON,
        "score was {}",
        ranked[0].1
    );
}

#[test]
fn temporal_decay_huge_age_near_zero() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .cast_signed();
    let age_secs = 3650i64 * 86400;
    let ts = now - age_secs;
    let mut ranked = vec![(MessageId(1), 1.0f64)];
    let mut timestamps = std::collections::HashMap::new();
    timestamps.insert(MessageId(1), ts);
    apply_temporal_decay(&mut ranked, &timestamps, 30);
    assert!(ranked[0].1 < 0.001, "score was {}", ranked[0].1);
}

#[test]
fn temporal_decay_small_half_life() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .cast_signed();
    let ts = now - 7 * 86400i64;
    let mut ranked = vec![(MessageId(1), 1.0f64)];
    let mut timestamps = std::collections::HashMap::new();
    timestamps.insert(MessageId(1), ts);
    apply_temporal_decay(&mut ranked, &timestamps, 1);
    assert!(ranked[0].1 < 0.01, "score was {}", ranked[0].1);
}

#[test]
fn mmr_empty_input_returns_empty() {
    let ranked = vec![];
    let vectors = std::collections::HashMap::new();
    let result = apply_mmr(&ranked, &vectors, 0.7, 5);
    assert!(result.is_empty());
}

#[test]
fn mmr_returns_up_to_limit() {
    let ranked = vec![
        (MessageId(1), 1.0f64),
        (MessageId(2), 0.9f64),
        (MessageId(3), 0.8f64),
    ];
    let mut vectors = std::collections::HashMap::new();
    vectors.insert(MessageId(1), vec![1.0f32, 0.0]);
    vectors.insert(MessageId(2), vec![0.0f32, 1.0]);
    vectors.insert(MessageId(3), vec![1.0f32, 0.0]);
    let result = apply_mmr(&ranked, &vectors, 0.7, 2);
    assert_eq!(result.len(), 2);
}

#[test]
fn mmr_without_vectors_picks_by_relevance() {
    let ranked = vec![(MessageId(1), 1.0f64), (MessageId(2), 0.5f64)];
    let vectors = std::collections::HashMap::new();
    let result = apply_mmr(&ranked, &vectors, 0.7, 2);
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].0, MessageId(1));
}

#[test]
fn mmr_prefers_diverse_over_redundant() {
    let ranked = vec![
        (MessageId(1), 1.0f64),
        (MessageId(2), 0.9f64),
        (MessageId(3), 0.9f64),
    ];
    let mut vectors = std::collections::HashMap::new();
    vectors.insert(MessageId(1), vec![1.0f32, 0.0]);
    vectors.insert(MessageId(2), vec![0.0f32, 1.0]);
    vectors.insert(MessageId(3), vec![1.0f32, 0.0]);
    let result = apply_mmr(&ranked, &vectors, 0.5, 2);
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].0, MessageId(1));
    assert_eq!(result[1].0, MessageId(2));
}

#[test]
fn mmr_lambda_zero_max_diversity() {
    let ranked = vec![
        (MessageId(1), 1.0f64),
        (MessageId(2), 0.9f64),
        (MessageId(3), 0.85f64),
    ];
    let mut vectors = std::collections::HashMap::new();
    vectors.insert(MessageId(1), vec![1.0f32, 0.0]);
    vectors.insert(MessageId(2), vec![0.0f32, 1.0]);
    vectors.insert(MessageId(3), vec![1.0f32, 0.0]);
    let result = apply_mmr(&ranked, &vectors, 0.0, 3);
    assert_eq!(result.len(), 3);
    assert_eq!(result[1].0, MessageId(2));
}

#[test]
fn mmr_lambda_one_pure_relevance() {
    let ranked = vec![
        (MessageId(1), 1.0f64),
        (MessageId(2), 0.8f64),
        (MessageId(3), 0.6f64),
    ];
    let mut vectors = std::collections::HashMap::new();
    vectors.insert(MessageId(1), vec![1.0f32, 0.0]);
    vectors.insert(MessageId(2), vec![0.0f32, 1.0]);
    vectors.insert(MessageId(3), vec![0.5f32, 0.5]);
    let result = apply_mmr(&ranked, &vectors, 1.0, 3);
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].0, MessageId(1));
    assert_eq!(result[1].0, MessageId(2));
    assert_eq!(result[2].0, MessageId(3));
}

#[test]
fn mmr_limit_zero_returns_empty() {
    let ranked = vec![(MessageId(1), 1.0f64), (MessageId(2), 0.8f64)];
    let mut vectors = std::collections::HashMap::new();
    vectors.insert(MessageId(1), vec![1.0f32, 0.0]);
    vectors.insert(MessageId(2), vec![0.0f32, 1.0]);
    let result = apply_mmr(&ranked, &vectors, 0.7, 0);
    assert!(result.is_empty());
}

#[test]
fn mmr_duplicate_vectors_penalizes_second() {
    let ranked = vec![
        (MessageId(1), 1.0f64),
        (MessageId(2), 1.0f64),
        (MessageId(3), 0.9f64),
    ];
    let mut vectors = std::collections::HashMap::new();
    vectors.insert(MessageId(1), vec![1.0f32, 0.0]);
    vectors.insert(MessageId(2), vec![1.0f32, 0.0]);
    vectors.insert(MessageId(3), vec![0.0f32, 1.0]);
    let result = apply_mmr(&ranked, &vectors, 0.5, 3);
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].0, MessageId(1));
    assert_eq!(result[1].0, MessageId(3));
}
