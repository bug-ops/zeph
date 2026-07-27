// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const TTL: Duration = Duration::from_secs(30);
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
    fn unicode_paths_are_indexed_and_searchable() {
        let idx = make_index(&["src/данные.rs", "データ/main.rs", "normal.rs"]);
        assert!(idx.paths().iter().any(|p| p.contains("данные")));
        assert!(idx.paths().iter().any(|p| p.contains("main")));
    }

    #[test]
    fn arc_paths_shared_not_cloned() {
        let idx = make_index(&["a.rs", "b.rs"]);
        let arc1 = idx.paths_arc();
        let arc2 = idx.paths_arc();
        // Both should point to the same allocation
        assert!(Arc::ptr_eq(&arc1, &arc2));
    }
}
