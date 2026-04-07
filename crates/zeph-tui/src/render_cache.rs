// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use ratatui::text::Line;

use crate::widgets::chat::MdLink;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderCacheKey {
    pub content_hash: u64,
    pub terminal_width: u16,
    pub tool_expanded: bool,
    pub compact_tools: bool,
    pub show_labels: bool,
}

pub struct RenderCacheEntry {
    pub key: RenderCacheKey,
    pub lines: Vec<Line<'static>>,
    pub md_links: Vec<MdLink>,
}

#[derive(Default)]
pub struct RenderCache {
    entries: Vec<Option<RenderCacheEntry>>,
}

impl RenderCache {
    pub fn get(&self, idx: usize, key: &RenderCacheKey) -> Option<(&[Line<'static>], &[MdLink])> {
        self.entries
            .get(idx)
            .and_then(Option::as_ref)
            .filter(|e| &e.key == key)
            .map(|e| (e.lines.as_slice(), e.md_links.as_slice()))
    }

    pub fn put(
        &mut self,
        idx: usize,
        key: RenderCacheKey,
        lines: Vec<Line<'static>>,
        md_links: Vec<MdLink>,
    ) {
        if idx >= self.entries.len() {
            self.entries.resize_with(idx + 1, || None);
        }
        self.entries[idx] = Some(RenderCacheEntry {
            key,
            lines,
            md_links,
        });
    }

    pub fn invalidate(&mut self, idx: usize) {
        if let Some(entry) = self.entries.get_mut(idx) {
            *entry = None;
        }
    }

    pub fn clear(&mut self) {
        self.entries = Vec::new();
    }

    pub fn shift(&mut self, count: usize) {
        if count >= self.entries.len() {
            self.entries = Vec::new();
        } else {
            self.entries.drain(0..count);
        }
    }
}

#[must_use]
pub fn content_hash(s: &str) -> u64 {
    zeph_common::hash::fast_hash(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_key(hash: u64) -> RenderCacheKey {
        RenderCacheKey {
            content_hash: hash,
            terminal_width: 80,
            tool_expanded: false,
            compact_tools: false,
            show_labels: false,
        }
    }

    fn populated_cache(count: usize) -> RenderCache {
        let mut cache = RenderCache::default();
        for i in 0..count {
            cache.put(i, make_key(i as u64), vec![], vec![]);
        }
        cache
    }

    #[test]
    fn shift_zero_is_noop() {
        let mut cache = populated_cache(3);
        cache.shift(0);
        assert!(cache.get(0, &make_key(0)).is_some());
        assert!(cache.get(1, &make_key(1)).is_some());
        assert!(cache.get(2, &make_key(2)).is_some());
    }

    #[test]
    fn shift_count_equals_len_empties_cache() {
        let mut cache = populated_cache(3);
        cache.shift(3);
        assert!(cache.get(0, &make_key(0)).is_none());
        assert!(cache.get(1, &make_key(1)).is_none());
    }

    #[test]
    fn shift_count_greater_than_len_empties_cache() {
        let mut cache = populated_cache(3);
        cache.shift(10);
        assert!(cache.get(0, &make_key(0)).is_none());
    }

    #[test]
    fn shift_partial_preserves_remaining_entries() {
        let mut cache = populated_cache(5);
        // entries at indices 0,1,2,3,4 have keys with hash 0,1,2,3,4
        cache.shift(2);
        // after shift: old index 2 → new index 0, old index 3 → new index 1, etc.
        assert!(cache.get(0, &make_key(2)).is_some());
        assert!(cache.get(1, &make_key(3)).is_some());
        assert!(cache.get(2, &make_key(4)).is_some());
        assert!(cache.get(3, &make_key(0)).is_none()); // out of bounds or wrong key
    }
}
