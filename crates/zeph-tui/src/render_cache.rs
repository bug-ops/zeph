// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use ratatui::text::Line;

use crate::widgets::chat::MdLink;

/// Cache key for a single rendered chat message.
///
/// Two keys compare equal only when the content, terminal width, and all
/// display flags are identical. Any mismatch causes a cache miss and
/// re-render.
///
/// # Examples
///
/// ```rust
/// use zeph_tui::render_cache::RenderCacheKey;
///
/// let k1 = RenderCacheKey { content_hash: 1, terminal_width: 80, tool_expanded: false, compact_tools: false, show_labels: false };
/// let k2 = RenderCacheKey { content_hash: 1, terminal_width: 80, tool_expanded: false, compact_tools: false, show_labels: false };
/// assert_eq!(k1, k2);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderCacheKey {
    /// FNV/xxHash of the message content string.
    pub content_hash: u64,
    /// Terminal column width at the time of rendering.
    pub terminal_width: u16,
    /// Whether the tool-output section is expanded.
    pub tool_expanded: bool,
    /// Whether tool blocks use compact single-line display.
    pub compact_tools: bool,
    /// Whether source-label badges are shown on assistant messages.
    pub show_labels: bool,
}

/// A single cached render result for a chat message.
///
/// Stores the pre-rendered [`ratatui::text::Line`] vector and extracted
/// markdown link metadata. Both are reused verbatim on cache hits.
pub struct RenderCacheEntry {
    /// The key this entry was computed for.
    pub key: RenderCacheKey,
    /// Pre-rendered lines ready for the chat widget.
    pub lines: Vec<Line<'static>>,
    /// Markdown hyperlink spans extracted during rendering.
    pub md_links: Vec<MdLink>,
}

/// Per-message render cache keyed by message index.
///
/// The cache stores one optional entry per chat message, addressed by the
/// message's position in [`crate::App`]'s message buffer. On each frame the
/// chat widget calls [`get`](Self::get) with the current [`RenderCacheKey`];
/// on a hit it reuses the cached lines, skipping expensive markdown parsing
/// and word-wrapping.
///
/// When messages are evicted from the front of the buffer, call
/// [`shift`](Self::shift) to keep indices aligned.
///
/// # Examples
///
/// ```rust
/// use zeph_tui::render_cache::{RenderCache, RenderCacheKey};
///
/// let mut cache = RenderCache::default();
/// let key = RenderCacheKey { content_hash: 42, terminal_width: 80, tool_expanded: false, compact_tools: false, show_labels: false };
/// cache.put(0, key, vec![], vec![]);
/// assert!(cache.get(0, &key).is_some());
/// ```
#[derive(Default)]
pub struct RenderCache {
    entries: Vec<Option<RenderCacheEntry>>,
}

impl RenderCache {
    /// Look up cached lines for message at `idx` with the given `key`.
    ///
    /// Returns `Some((lines, md_links))` on a cache hit, `None` on a miss or
    /// key mismatch.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_tui::render_cache::{RenderCache, RenderCacheKey};
    ///
    /// let mut cache = RenderCache::default();
    /// let key = RenderCacheKey { content_hash: 1, terminal_width: 80, tool_expanded: false, compact_tools: false, show_labels: false };
    /// assert!(cache.get(0, &key).is_none()); // cold cache
    /// ```
    pub fn get(&self, idx: usize, key: &RenderCacheKey) -> Option<(&[Line<'static>], &[MdLink])> {
        self.entries
            .get(idx)
            .and_then(Option::as_ref)
            .filter(|e| &e.key == key)
            .map(|e| (e.lines.as_slice(), e.md_links.as_slice()))
    }

    /// Store a rendered entry for message at `idx`.
    ///
    /// Grows the internal storage as needed. An existing entry at `idx` is
    /// unconditionally replaced.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_tui::render_cache::{RenderCache, RenderCacheKey};
    ///
    /// let mut cache = RenderCache::default();
    /// let key = RenderCacheKey { content_hash: 7, terminal_width: 100, tool_expanded: true, compact_tools: false, show_labels: false };
    /// cache.put(0, key, vec![], vec![]);
    /// assert!(cache.get(0, &key).is_some());
    /// ```
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

    /// Invalidate the entry at `idx`, forcing a re-render on the next frame.
    ///
    /// A no-op if `idx` is out of range.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_tui::render_cache::{RenderCache, RenderCacheKey};
    ///
    /// let mut cache = RenderCache::default();
    /// let key = RenderCacheKey { content_hash: 1, terminal_width: 80, tool_expanded: false, compact_tools: false, show_labels: false };
    /// cache.put(0, key, vec![], vec![]);
    /// cache.invalidate(0);
    /// assert!(cache.get(0, &key).is_none());
    /// ```
    pub fn invalidate(&mut self, idx: usize) {
        if let Some(entry) = self.entries.get_mut(idx) {
            *entry = None;
        }
    }

    /// Remove all cached entries.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_tui::render_cache::{RenderCache, RenderCacheKey};
    ///
    /// let mut cache = RenderCache::default();
    /// let key = RenderCacheKey { content_hash: 1, terminal_width: 80, tool_expanded: false, compact_tools: false, show_labels: false };
    /// cache.put(0, key, vec![], vec![]);
    /// cache.clear();
    /// assert!(cache.get(0, &key).is_none());
    /// ```
    pub fn clear(&mut self) {
        self.entries = Vec::new();
    }

    /// Shift all entries left by `count` positions.
    ///
    /// Called when `count` messages are evicted from the front of the message
    /// buffer, so that cache index `N` continues to map to message index `N`.
    /// If `count` >= the current number of entries, the cache is emptied.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_tui::render_cache::{RenderCache, RenderCacheKey};
    ///
    /// let mut cache = RenderCache::default();
    /// for i in 0..3u64 {
    ///     let key = RenderCacheKey { content_hash: i, terminal_width: 80, tool_expanded: false, compact_tools: false, show_labels: false };
    ///     cache.put(i as usize, key, vec![], vec![]);
    /// }
    /// cache.shift(1);
    /// // Old index 1 is now at index 0.
    /// let key1 = RenderCacheKey { content_hash: 1, terminal_width: 80, tool_expanded: false, compact_tools: false, show_labels: false };
    /// assert!(cache.get(0, &key1).is_some());
    /// ```
    pub fn shift(&mut self, count: usize) {
        if count >= self.entries.len() {
            self.entries = Vec::new();
        } else {
            self.entries.drain(0..count);
        }
    }
}

/// Compute a fast, non-cryptographic hash of a string for cache keying.
///
/// The underlying algorithm is [`zeph_common::hash::fast_hash`] (xxHash or
/// similar). The result is stable within a process but should not be persisted.
///
/// # Examples
///
/// ```rust
/// use zeph_tui::render_cache::content_hash;
///
/// let h = content_hash("hello");
/// assert_eq!(h, content_hash("hello")); // deterministic
/// assert_ne!(h, content_hash("world")); // distinct inputs → distinct hashes
/// ```
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
