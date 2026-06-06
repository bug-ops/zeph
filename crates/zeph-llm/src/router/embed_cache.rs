// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Bounded embedding caches used by the router.
//!
//! [`BanditEmbedCache`] is a process-lifetime FIFO cache for bandit feature vectors
//! keyed by query hash; [`TurnEmbedCache`] is a short-lived per-turn cache keyed by
//! the exact input string, created and dropped within a single `chat()` call.

use std::collections::HashMap;

/// Simple bounded embedding cache for bandit feature vectors.
///
/// Keyed by `u64` hash of query text (using `std::hash`). Eviction is FIFO on insertion
/// order (not LRU) — acceptable for a routing cache where hot queries repeat often.
/// The `lru` crate is not in the workspace; a `HashMap` + insertion-order Vec avoids a new dep.
#[derive(Debug)]
pub(crate) struct BanditEmbedCache {
    map: HashMap<u64, Vec<f32>>,
    order: std::collections::VecDeque<u64>,
    capacity: usize,
}

impl BanditEmbedCache {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::with_capacity(capacity),
            order: std::collections::VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub(crate) fn get(&self, key: u64) -> Option<&Vec<f32>> {
        self.map.get(&key)
    }

    pub(crate) fn insert(&mut self, key: u64, value: Vec<f32>) {
        if self.map.contains_key(&key) {
            return;
        }
        if self.map.len() >= self.capacity
            && let Some(evict) = self.order.pop_front()
        {
            self.map.remove(&evict);
        }
        self.map.insert(key, value);
        self.order.push_back(key);
    }
}

impl Default for BanditEmbedCache {
    fn default() -> Self {
        Self::new(512)
    }
}

/// Per-turn embedding cache keyed by the exact input string.
///
/// Created at the start of each `chat()` call and dropped at the end. With 2-4 entries
/// per turn, `String` keys have negligible overhead and eliminate the hash-collision risk
/// of `u64`-keyed caches.
#[derive(Debug, Default)]
pub(crate) struct TurnEmbedCache {
    entries: HashMap<String, Vec<f32>>,
}

impl TurnEmbedCache {
    pub(crate) fn get(&self, text: &str) -> Option<&Vec<f32>> {
        self.entries.get(text)
    }

    pub(crate) fn insert(&mut self, text: impl Into<String>, embedding: Vec<f32>) {
        self.entries.insert(text.into(), embedding);
    }
}
