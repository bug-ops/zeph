// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify_debouncer_mini::{DebouncedEventKind, new_debouncer};
use tokio::sync::mpsc;

use crate::error::Result;
use crate::indexer::CodeIndexer;
use crate::languages::is_indexable;

pub struct IndexWatcher {
    _handle: tokio::task::JoinHandle<()>,
}

impl IndexWatcher {
    /// # Errors
    ///
    /// Returns an error if the filesystem watcher cannot be initialized.
    pub fn start(root: &Path, indexer: Arc<CodeIndexer>) -> Result<Self> {
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
        let handle = tokio::spawn(async move {
            let _debouncer = debouncer;
            while let Some(path) = notify_rx.recv().await {
                if let Err(e) = indexer.reindex_file(&root, &path).await {
                    tracing::warn!(path = %path.display(), "reindex failed: {e:#}");
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
        let ops = QdrantOps::new("http://localhost:6334").unwrap();
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
        let watcher = IndexWatcher::start(dir.path(), create_test_indexer().await);
        assert!(watcher.is_ok());
    }

    #[tokio::test]
    async fn start_with_nonexistent_directory_fails() {
        let result = IndexWatcher::start(
            Path::new("/nonexistent/path/xyz"),
            create_test_indexer().await,
        );
        assert!(result.is_err());
    }
}
