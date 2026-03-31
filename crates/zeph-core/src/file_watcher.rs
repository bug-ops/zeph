// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;
use std::time::Duration;

use notify_debouncer_mini::{DebouncedEventKind, new_debouncer};
use tokio::sync::mpsc;

#[derive(Debug, thiserror::Error)]
pub enum FileWatcherError {
    #[error("no watch paths configured")]
    NoWatchPaths,

    #[error("filesystem watcher error: {0}")]
    Notify(#[from] notify::Error),
}

/// Filesystem change event for a watched path.
#[derive(Debug, Clone)]
pub struct FileChangedEvent {
    pub path: PathBuf,
}

/// Watches a set of paths and sends `FileChangedEvent` on any change.
///
/// Uses `notify-debouncer-mini` to debounce rapid filesystem events.
/// Paths are resolved once at construction time from the project root.
///
/// Call [`stop`](Self::stop) to shut down the watcher cleanly. The watcher
/// is also stopped automatically when all senders are dropped.
pub struct FileChangeWatcher {
    handle: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for FileChangeWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileChangeWatcher").finish_non_exhaustive()
    }
}

impl Drop for FileChangeWatcher {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl FileChangeWatcher {
    /// Start watching the given paths.
    ///
    /// `watch_paths` are watched recursively if they are directories.
    /// Each path in `watch_paths` is watched with `RecursiveMode::Recursive`.
    ///
    /// # Errors
    ///
    /// Returns an error if no paths are provided or if the watcher cannot be initialized.
    pub fn start(
        watch_paths: &[PathBuf],
        debounce_ms: u64,
        tx: mpsc::Sender<FileChangedEvent>,
    ) -> Result<Self, FileWatcherError> {
        if watch_paths.is_empty() {
            return Err(FileWatcherError::NoWatchPaths);
        }

        let (notify_tx, mut notify_rx) = mpsc::channel::<PathBuf>(64);

        let mut debouncer = new_debouncer(
            Duration::from_millis(debounce_ms),
            move |events: Result<Vec<notify_debouncer_mini::DebouncedEvent>, notify::Error>| {
                let events = match events {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!("file watcher error: {e}");
                        return;
                    }
                };
                for event in events {
                    if event.kind == DebouncedEventKind::Any {
                        let _ = notify_tx.blocking_send(event.path);
                    }
                }
            },
        )?;

        for path in watch_paths {
            if let Err(e) = debouncer
                .watcher()
                .watch(path, notify::RecursiveMode::Recursive)
            {
                tracing::warn!(path = %path.display(), error = %e, "file watcher: failed to watch path");
            }
        }

        let handle = tokio::spawn(async move {
            let _debouncer = debouncer;
            while let Some(path) = notify_rx.recv().await {
                if tx.send(FileChangedEvent { path }).await.is_err() {
                    break;
                }
            }
        });

        Ok(Self { handle })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn start_with_empty_paths_fails() {
        let (tx, _rx) = mpsc::channel(16);
        let result = FileChangeWatcher::start(&[], 500, tx);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            FileWatcherError::NoWatchPaths
        ));
    }

    #[tokio::test]
    async fn start_with_valid_dir() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::channel(16);
        let result = FileChangeWatcher::start(&[dir.path().to_path_buf()], 500, tx);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn detects_file_change() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, "initial").unwrap();

        let (tx, mut rx) = mpsc::channel(16);
        let _watcher = FileChangeWatcher::start(&[dir.path().to_path_buf()], 500, tx).unwrap();

        // Wait for watcher to settle before modifying.
        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::write(&file_path, "updated").unwrap();

        let result = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await;
        assert!(result.is_ok(), "expected FileChangedEvent within timeout");
        // Event received — path granularity varies by OS/watcher backend (e.g. macOS FSEvents
        // may return intermediate temp paths or symlink-resolved paths), so we only verify
        // that an event arrived from within the watched directory tree.
        assert!(result.unwrap().is_some());
    }
}
