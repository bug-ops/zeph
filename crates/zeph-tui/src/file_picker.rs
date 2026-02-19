use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

const TTL: Duration = Duration::from_secs(30);
const MAX_RESULTS: usize = 10;
/// Hard cap on indexed paths to prevent unbounded memory usage on repos with
/// large unignored directories.
const MAX_INDEXED: usize = 50_000;

pub struct FileIndex {
    paths: Arc<Vec<String>>,
    built_at: Instant,
}

impl FileIndex {
    /// Builds the file index by walking `root` with `.gitignore` awareness.
    ///
    /// # Blocking I/O note
    ///
    /// This function performs synchronous directory traversal on the calling thread.
    /// For small to medium repos (< 5 000 files) the cost is negligible (< 20 ms).
    /// For large monorepos (50 000+ files) consider offloading via
    /// `tokio::task::spawn_blocking`. A full async build is deferred to a
    /// follow-up milestone once the UX for "Indexing…" feedback is designed.
    #[must_use]
    pub fn build(root: &Path) -> Self {
        let mut paths = Vec::new();
        let walker = ignore::WalkBuilder::new(root)
            .hidden(true) // exclude dotfiles (.env, .ssh/, etc.)
            .ignore(true)
            .git_ignore(true)
            .build();

        for entry in walker.flatten() {
            if entry.file_type().is_some_and(|ft| ft.is_file()) {
                let path = entry.path();
                let rel = path.strip_prefix(root).unwrap_or(path);
                if let Some(s) = rel.to_str() {
                    // Normalize Windows backslashes to forward slashes
                    paths.push(s.replace('\\', "/"));
                }
                if paths.len() >= MAX_INDEXED {
                    tracing::warn!(
                        max = MAX_INDEXED,
                        root = %root.display(),
                        "file index cap reached; some files will not be searchable"
                    );
                    break;
                }
            }
        }
        paths.sort_unstable();
        Self {
            paths: Arc::new(paths),
            built_at: Instant::now(),
        }
    }

    #[must_use]
    pub fn is_stale(&self) -> bool {
        self.built_at.elapsed() > TTL
    }

    #[must_use]
    pub fn paths(&self) -> &[String] {
        &self.paths
    }

    #[must_use]
    pub fn paths_arc(&self) -> Arc<Vec<String>> {
        Arc::clone(&self.paths)
    }
}

#[derive(Clone)]
pub struct PickerMatch {
    pub path: String,
    pub score: u32,
}

pub struct FilePickerState {
    pub query: String,
    pub selected: usize,
    matches: Vec<PickerMatch>,
    /// Shared ownership of the file index — no clone on picker open.
    index: Arc<Vec<String>>,
    /// Reused across `refilter` calls to avoid per-keystroke heap allocation.
    matcher: Matcher,
}

impl FilePickerState {
    #[must_use]
    pub fn new(index: &FileIndex) -> Self {
        let mut state = Self {
            query: String::new(),
            selected: 0,
            matches: Vec::new(),
            index: index.paths_arc(),
            matcher: Matcher::new(Config::DEFAULT),
        };
        state.refilter();
        state
    }

    pub fn update_query(&mut self, query: &str) {
        query.clone_into(&mut self.query);
        self.refilter();
    }

    /// Appends a character to the query and re-filters.
    pub fn push_char(&mut self, c: char) {
        self.query.push(c);
        self.refilter();
    }

    /// Removes the last character from the query and re-filters.
    /// Returns `true` if a character was removed, `false` if the query was already empty.
    pub fn pop_char(&mut self) -> bool {
        if self.query.pop().is_some() {
            self.refilter();
            true
        } else {
            false
        }
    }

    #[must_use]
    pub fn matches(&self) -> &[PickerMatch] {
        &self.matches
    }

    #[must_use]
    pub fn selected_path(&self) -> Option<&str> {
        self.matches.get(self.selected).map(|m| m.path.as_str())
    }

    pub fn move_selection(&mut self, delta: i32) {
        let len = self.matches.len();
        if len == 0 {
            return;
        }
        let len_i = i32::try_from(len).unwrap_or(i32::MAX);
        let cur_i = i32::try_from(self.selected).unwrap_or(0);
        let new_i = (cur_i + delta).rem_euclid(len_i);
        self.selected = usize::try_from(new_i).unwrap_or(0);
    }

    fn refilter(&mut self) {
        self.selected = 0;
        if self.query.is_empty() {
            self.matches = self
                .index
                .iter()
                .take(MAX_RESULTS)
                .map(|p| PickerMatch {
                    path: p.clone(),
                    score: 0,
                })
                .collect();
            return;
        }

        let pattern = Pattern::new(
            &self.query,
            CaseMatching::Smart,
            Normalization::Smart,
            AtomKind::Fuzzy,
        );

        let mut scored: Vec<PickerMatch> = self
            .index
            .iter()
            .filter_map(|p| {
                let mut buf = Vec::new();
                let haystack = Utf32Str::new(p, &mut buf);
                pattern
                    .score(haystack, &mut self.matcher)
                    .map(|score| PickerMatch {
                        path: p.clone(),
                        score,
                    })
            })
            .collect();

        scored.sort_unstable_by(|a, b| b.score.cmp(&a.score));
        scored.truncate(MAX_RESULTS);
        self.matches = scored;
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn make_index(files: &[&str]) -> FileIndex {
        let dir = tempfile::tempdir().unwrap();
        for &f in files {
            let path = dir.path().join(f);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, "").unwrap();
        }
        FileIndex::build(dir.path())
    }

    #[test]
    fn build_collects_files() {
        let idx = make_index(&["src/main.rs", "src/lib.rs", "README.md"]);
        assert_eq!(idx.paths().len(), 3);
        assert!(idx.paths().iter().any(|p| p.ends_with("main.rs")));
    }

    #[test]
    fn is_stale_false_when_fresh() {
        let idx = make_index(&["a.rs"]);
        assert!(!idx.is_stale());
    }

    #[test]
    fn empty_query_returns_up_to_10_files() {
        let files: Vec<String> = (0..15).map(|i| format!("file{i}.rs")).collect();
        let refs: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
        let idx = make_index(&refs);
        let state = FilePickerState::new(&idx);
        assert_eq!(state.matches().len(), 10);
    }

    #[test]
    fn fuzzy_query_filters_results() {
        let idx = make_index(&["src/main.rs", "src/lib.rs", "tests/foo.rs"]);
        let mut state = FilePickerState::new(&idx);
        state.update_query("main");
        assert!(!state.matches().is_empty());
        assert!(state.matches().iter().any(|m| m.path.contains("main")));
    }

    #[test]
    fn selected_path_returns_first_match() {
        let idx = make_index(&["alpha.rs", "beta.rs"]);
        let state = FilePickerState::new(&idx);
        assert!(state.selected_path().is_some());
    }

    #[test]
    fn move_selection_wraps_around() {
        let idx = make_index(&["a.rs", "b.rs", "c.rs"]);
        let mut state = FilePickerState::new(&idx);
        assert_eq!(state.selected, 0);
        state.move_selection(-1);
        assert_eq!(state.selected, state.matches().len() - 1);
    }

    #[test]
    fn move_selection_noop_when_empty() {
        let idx = make_index(&["a.rs"]);
        let mut state = FilePickerState::new(&idx);
        state.matches = vec![];
        state.move_selection(1);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn no_match_query_returns_empty_and_selected_path_none() {
        let idx = make_index(&["src/main.rs", "src/lib.rs"]);
        let mut state = FilePickerState::new(&idx);
        state.update_query("xyznotfound");
        assert!(state.matches().is_empty());
        assert!(state.selected_path().is_none());
    }

    #[test]
    fn unicode_paths_are_indexed_and_searchable() {
        let idx = make_index(&["src/данные.rs", "データ/main.rs", "normal.rs"]);
        assert!(idx.paths().iter().any(|p| p.contains("данные")));
        assert!(idx.paths().iter().any(|p| p.contains("main")));

        let mut state = FilePickerState::new(&idx);
        state.update_query("данные");
        assert!(
            !state.matches().is_empty(),
            "expected match for unicode query"
        );
    }

    #[test]
    fn push_char_appends_and_refilters() {
        let idx = make_index(&["src/main.rs", "src/lib.rs"]);
        let mut state = FilePickerState::new(&idx);
        state.push_char('m');
        state.push_char('a');
        assert!(state.matches().iter().any(|m| m.path.contains("main")));
    }

    #[test]
    fn pop_char_removes_last_and_refilters() {
        let idx = make_index(&["src/main.rs", "src/lib.rs"]);
        let mut state = FilePickerState::new(&idx);
        state.push_char('m');
        let removed = state.pop_char();
        assert!(removed);
        assert!(state.query.is_empty());
    }

    #[test]
    fn pop_char_on_empty_returns_false() {
        let idx = make_index(&["a.rs"]);
        let mut state = FilePickerState::new(&idx);
        assert!(!state.pop_char());
    }

    #[test]
    fn arc_index_shared_not_cloned() {
        let idx = make_index(&["a.rs", "b.rs"]);
        let arc1 = idx.paths_arc();
        let state = FilePickerState::new(&idx);
        // Both should point to the same allocation
        assert!(Arc::ptr_eq(&arc1, &state.index));
    }

    use proptest::prelude::*;

    proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(200))]

        #[test]
        fn move_selection_never_panics(
            n in 1usize..20,
            delta in -10i32..10,
        ) {
            let files: Vec<String> = (0..n).map(|i| format!("f{i}.rs")).collect();
            let refs: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
            let idx = make_index(&refs);
            let mut state = FilePickerState::new(&idx);
            state.move_selection(delta);
            prop_assert!(state.selected < state.matches().len().max(1));
        }
    }
}
