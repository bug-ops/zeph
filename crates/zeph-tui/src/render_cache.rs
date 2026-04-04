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
        for entry in &mut self.entries {
            *entry = None;
        }
    }
}

#[must_use]
pub fn content_hash(s: &str) -> u64 {
    zeph_common::hash::fast_hash(s)
}
