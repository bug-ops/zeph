// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Bounded LRU cache for LSP diagnostics pushed via `lsp/publishDiagnostics`.
//!
//! URI keys are compared as-is (the IDE is expected to use consistent URIs).
//! URI normalization (case, symlinks) is deferred to a follow-up if cache misses
//! are observed in practice.

use std::collections::{HashMap, VecDeque};

use super::types::LspDiagnostic;

/// Bounded per-session cache for pushed LSP diagnostics.
///
/// Holds diagnostics for at most `max_files` files with LRU eviction.
pub struct DiagnosticsCache {
    entries: HashMap<String, Vec<LspDiagnostic>>,
    order: VecDeque<String>,
    max_files: usize,
}

impl DiagnosticsCache {
    /// Create a new cache with the given file limit.
    #[must_use]
    pub fn new(max_files: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            max_files: max_files.max(1),
        }
    }

    /// Insert or replace diagnostics for a URI, evicting the oldest entry when at capacity.
    pub fn update(&mut self, uri: String, diagnostics: Vec<LspDiagnostic>) {
        if self.entries.contains_key(&uri) {
            // Move to back (most recently used).
            self.order.retain(|u| u != &uri);
        } else if self.entries.len() >= self.max_files {
            // Evict least recently used.
            if let Some(evicted) = self.order.pop_front() {
                self.entries.remove(&evicted);
            }
        }
        self.order.push_back(uri.clone());
        self.entries.insert(uri, diagnostics);
    }

    /// Peek at diagnostics for a URI without refreshing LRU order.
    ///
    /// Returns `None` if not cached. Reads do **not** affect eviction order — only
    /// `update()` refreshes recency. Rename from `get()` to make peek semantics explicit.
    #[must_use]
    pub fn peek(&self, uri: &str) -> Option<&[LspDiagnostic]> {
        self.entries.get(uri).map(Vec::as_slice)
    }

    /// Return all files that have at least one diagnostic.
    #[must_use]
    pub fn all_non_empty(&self) -> Vec<(&str, &[LspDiagnostic])> {
        self.entries
            .iter()
            .filter(|(_, diags)| !diags.is_empty())
            .map(|(uri, diags)| (uri.as_str(), diags.as_slice()))
            .collect()
    }

    /// Clear all cached diagnostics.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }

    /// Number of files currently in the cache.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if the cache has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::types::{LspDiagnosticSeverity, LspPosition, LspRange};

    fn make_diag(msg: &str) -> LspDiagnostic {
        LspDiagnostic {
            range: LspRange {
                start: LspPosition {
                    line: 1,
                    character: 0,
                },
                end: LspPosition {
                    line: 1,
                    character: 1,
                },
            },
            severity: Some(LspDiagnosticSeverity::Error),
            code: None,
            source: None,
            message: msg.to_owned(),
        }
    }

    #[test]
    fn insert_and_get() {
        let mut cache = DiagnosticsCache::new(5);
        cache.update("file:///a.rs".to_owned(), vec![make_diag("err1")]);
        let diags = cache.peek("file:///a.rs").unwrap();
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].message, "err1");
    }

    #[test]
    fn missing_uri_returns_none() {
        let cache = DiagnosticsCache::new(5);
        assert!(cache.peek("file:///missing.rs").is_none());
    }

    #[test]
    fn lru_eviction_removes_oldest() {
        let mut cache = DiagnosticsCache::new(2);
        cache.update("file:///a.rs".to_owned(), vec![make_diag("a")]);
        cache.update("file:///b.rs".to_owned(), vec![make_diag("b")]);
        // Insert third — should evict "a".
        cache.update("file:///c.rs".to_owned(), vec![make_diag("c")]);

        assert!(cache.peek("file:///a.rs").is_none(), "a should be evicted");
        assert!(cache.peek("file:///b.rs").is_some());
        assert!(cache.peek("file:///c.rs").is_some());
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn update_existing_uri_does_not_grow() {
        let mut cache = DiagnosticsCache::new(2);
        cache.update("file:///a.rs".to_owned(), vec![make_diag("v1")]);
        cache.update("file:///a.rs".to_owned(), vec![make_diag("v2")]);
        assert_eq!(cache.len(), 1);
        let diags = cache.peek("file:///a.rs").unwrap();
        assert_eq!(diags[0].message, "v2");
    }

    #[test]
    fn update_existing_moves_to_recent() {
        let mut cache = DiagnosticsCache::new(2);
        cache.update("file:///a.rs".to_owned(), vec![make_diag("a")]);
        cache.update("file:///b.rs".to_owned(), vec![make_diag("b")]);
        // Touch "a" — making it most recent.
        cache.update("file:///a.rs".to_owned(), vec![make_diag("a2")]);
        // Insert "c" — should evict "b" (least recently used).
        cache.update("file:///c.rs".to_owned(), vec![make_diag("c")]);

        assert!(cache.peek("file:///b.rs").is_none(), "b should be evicted");
        assert!(cache.peek("file:///a.rs").is_some());
        assert!(cache.peek("file:///c.rs").is_some());
    }

    #[test]
    fn all_non_empty_skips_empty_diags() {
        let mut cache = DiagnosticsCache::new(5);
        cache.update("file:///a.rs".to_owned(), vec![make_diag("err")]);
        cache.update("file:///b.rs".to_owned(), vec![]);
        let non_empty = cache.all_non_empty();
        assert_eq!(non_empty.len(), 1);
        assert_eq!(non_empty[0].0, "file:///a.rs");
    }

    #[test]
    fn clear_removes_all() {
        let mut cache = DiagnosticsCache::new(5);
        cache.update("file:///a.rs".to_owned(), vec![make_diag("err")]);
        cache.clear();
        assert!(cache.is_empty());
        assert!(cache.peek("file:///a.rs").is_none());
    }

    #[test]
    fn max_files_one_always_evicts() {
        let mut cache = DiagnosticsCache::new(1);
        cache.update("file:///a.rs".to_owned(), vec![make_diag("a")]);
        cache.update("file:///b.rs".to_owned(), vec![make_diag("b")]);
        assert!(cache.peek("file:///a.rs").is_none());
        assert!(cache.peek("file:///b.rs").is_some());
        assert_eq!(cache.len(), 1);
    }
}
