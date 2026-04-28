// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! File-system watcher for incremental re-indexing on save.
//!
//! [`IndexWatcher`] wraps `notify-debouncer-mini` and feeds file change events
//! through an async channel into a background Tokio task that calls
//! [`crate::indexer::CodeIndexer::reindex_file`].
//!
//! ## Debouncing
//!
//! Events pass through two debounce stages. The `notify-debouncer-mini` layer
//! coalesces OS-level inotify/kqueue/FSEvents bursts with a 1-second window.
//! A second 500 ms Tokio-side debounce batches any remaining rapid events into a
//! single reindex pass, further reducing redundant work on bursty saves.
//!
//! ## Gitignore filtering
//!
//! The watcher loads `.gitignore` from the project root and skips files matched by
//! it. This prevents spurious reindex calls for build artifacts in `target/` or
//! temporary files in `.local/`.
//!
//! ## TUI integration
//!
//! When `status_tx` is supplied, the watcher sends a short `"Re-indexing <file>..."`
//! message before each reindex and an empty string when it completes, so the TUI
//! status bar shows live feedback.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify_debouncer_mini::{DebouncedEventKind, new_debouncer};
use tokio::sync::mpsc;

use crate::error::Result;
use crate::indexer::CodeIndexer;
use crate::languages::is_indexable;

/// Build a gitignore matcher for `root` by loading `.gitignore` from the root directory.
/// Returns an empty (pass-all) matcher on any error so the watcher degrades gracefully.
fn build_gitignore(root: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    let _ = builder.add(root.join(".gitignore"));
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

/// Returns `true` if `path` should be skipped because it (or one of its ancestors)
/// is matched by the project's `.gitignore`.
fn is_gitignored(gitignore: &Gitignore, root: &Path, path: &Path) -> bool {
    // matched_path_or_any_parents requires a path relative to the gitignore root.
    let Ok(relative) = path.strip_prefix(root) else {
        // Path outside root — let is_indexable decide.
        return false;
    };
    gitignore
        .matched_path_or_any_parents(relative, false)
        .is_ignore()
}

/// A running file-system watcher that triggers incremental re-indexing on file saves.
///
/// Created by [`IndexWatcher::start`]. Dropping the `IndexWatcher` aborts the
/// background Tokio task and the underlying `notify` watcher, stopping all
/// file-system monitoring.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
/// use std::path::Path;
/// use zeph_index::watcher::IndexWatcher;
/// # async fn example() -> zeph_index::Result<()> {
/// # let indexer: Arc<zeph_index::indexer::CodeIndexer> = panic!("placeholder");
///
/// // Start watching — the returned handle keeps the watcher alive.
/// let _watcher = IndexWatcher::start(Path::new("."), indexer, None)?;
/// # Ok(())
/// # }
/// ```
pub struct IndexWatcher {
    _handle: tokio::task::JoinHandle<()>,
}

impl IndexWatcher {
    /// Start the file-system watcher.
    ///
    /// When `status_tx` is `Some`, a short status message is sent to it whenever a file
    /// reindex begins, and an empty string is sent when it completes (clearing the TUI
    /// status bar). Pass `None` in non-TUI modes where no status indicator is needed.
    ///
    /// # Errors
    ///
    /// Returns an error if the filesystem watcher cannot be initialized.
    pub fn start(
        root: &Path,
        indexer: Arc<CodeIndexer>,
        status_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Result<Self> {
        const DEBOUNCE: Duration = Duration::from_millis(500);
        // Under sustained FS writes the deadline resets on every event. MAX_DEBOUNCE
        // caps the total wait so reindexing is never starved indefinitely.
        const MAX_DEBOUNCE: Duration = Duration::from_secs(5);

        let (notify_tx, mut notify_rx) = mpsc::channel::<PathBuf>(64);

        let mut debouncer = new_debouncer(
            Duration::from_secs(1),
            move |events: std::result::Result<
                Vec<notify_debouncer_mini::DebouncedEvent>,
                notify::Error,
            >| {
                let events = match events {
                    Ok(events) => events,
                    Err(e) => {
                        tracing::warn!("index watcher error: {e}");
                        return;
                    }
                };

                let paths: HashSet<PathBuf> = events
                    .into_iter()
                    .filter(|e| e.kind == DebouncedEventKind::Any && is_indexable(&e.path))
                    .map(|e| e.path)
                    .collect();

                for path in paths {
                    let _ = notify_tx.blocking_send(path);
                }
            },
        )?;

        debouncer
            .watcher()
            .watch(root, notify::RecursiveMode::Recursive)?;

        let root = root.to_path_buf();
        let gitignore = build_gitignore(&root);

        let handle = tokio::spawn(async move {
            let _debouncer = debouncer;
            let mut pending: HashSet<PathBuf> = HashSet::new();
            let mut deadline = tokio::time::Instant::now() + DEBOUNCE;
            let mut batch_start: Option<tokio::time::Instant> = None;

            loop {
                tokio::select! {
                    msg = notify_rx.recv() => {
                        let Some(path) = msg else { break };
                        if is_gitignored(&gitignore, &root, &path) {
                            tracing::trace!(path = %path.display(), "skipping gitignored path");
                            continue;
                        }
                        let now = tokio::time::Instant::now();
                        let start = *batch_start.get_or_insert(now);
                        pending.insert(path);
                        // Cap deadline so sustained writes cannot starve reindexing indefinitely.
                        deadline = (start + MAX_DEBOUNCE).min(now + DEBOUNCE);
                    }
                    () = tokio::time::sleep_until(deadline), if !pending.is_empty() => {
                        let paths: Vec<PathBuf> = pending.drain().collect();
                        batch_start = None;
                        tracing::trace!("debounce fired, reindexing {} paths", paths.len());
                        for path in paths {
                            if let Some(ref tx) = status_tx {
                                let name = path.file_name().map_or_else(
                                    || path.display().to_string(),
                                    |n| n.to_string_lossy().into_owned(),
                                );
                                let _ = tx.send(format!("Re-indexing {name}..."));
                            }
                            if let Err(e) = indexer.reindex_file(&root, &path).await {
                                tracing::warn!(path = %path.display(), "reindex failed: {e:#}");
                            }
                            if let Some(ref tx) = status_tx {
                                let _ = tx.send(String::new());
                            }
                        }
                    }
                }
            }
        });

        Ok(Self { _handle: handle })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use zeph_llm::any::AnyProvider;
    use zeph_llm::ollama::OllamaProvider;
    use zeph_memory::QdrantOps;

    async fn create_test_pool() -> zeph_db::DbPool {
        zeph_db::sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .unwrap()
    }

    async fn create_test_indexer() -> Arc<CodeIndexer> {
        let ops = QdrantOps::new("http://localhost:6334", None).unwrap();
        let store = crate::store::CodeStore::with_ops(ops, create_test_pool().await);
        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://127.0.0.1:1",
            "test".into(),
            "embed".into(),
        ));
        Arc::new(CodeIndexer::new(
            store,
            Arc::new(provider),
            crate::indexer::IndexerConfig::default(),
        ))
    }

    #[tokio::test]
    async fn start_with_valid_directory() {
        let dir = tempfile::tempdir().unwrap();
        let watcher = IndexWatcher::start(dir.path(), create_test_indexer().await, None);
        assert!(watcher.is_ok());
    }

    #[tokio::test]
    async fn start_with_nonexistent_directory_fails() {
        let result = IndexWatcher::start(
            Path::new("/nonexistent/path/xyz"),
            create_test_indexer().await,
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn gitignore_filters_target_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".gitignore"), "target/\n.local/\n").unwrap();

        let gitignore = build_gitignore(root);

        // Paths inside gitignored dirs must be filtered.
        assert!(is_gitignored(
            &gitignore,
            root,
            &root.join("target/debug/build")
        ));
        assert!(is_gitignored(
            &gitignore,
            root,
            &root.join(".local/testing/debug/dump.json")
        ));
        // Tracked source files must not be filtered.
        assert!(!is_gitignored(&gitignore, root, &root.join("src/main.rs")));
        assert!(!is_gitignored(
            &gitignore,
            root,
            &root.join("crates/zeph-core/src/lib.rs")
        ));
    }

    #[test]
    fn gitignore_passes_all_when_no_gitignore_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // No .gitignore — nothing should be filtered.
        let gitignore = build_gitignore(root);
        assert!(!is_gitignored(&gitignore, root, &root.join("src/lib.rs")));
        assert!(!is_gitignored(
            &gitignore,
            root,
            &root.join("target/debug/bin")
        ));
    }

    #[test]
    fn gitignore_ignores_path_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        let gitignore = build_gitignore(root);
        // Path outside root must not be filtered (strip_prefix fails → false).
        assert!(!is_gitignored(
            &gitignore,
            root,
            Path::new("/tmp/other/target/foo")
        ));
    }
}
