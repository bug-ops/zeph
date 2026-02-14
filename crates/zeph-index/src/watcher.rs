use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify_debouncer_mini::{DebouncedEventKind, new_debouncer};
use tokio::sync::mpsc;
use zeph_llm::provider::LlmProvider;

use crate::indexer::CodeIndexer;
use crate::languages::is_indexable;

pub struct IndexWatcher {
    _handle: tokio::task::JoinHandle<()>,
}

impl IndexWatcher {
    /// # Errors
    ///
    /// Returns an error if the filesystem watcher cannot be initialized.
    pub fn start<P: LlmProvider + Clone + 'static>(
        root: &Path,
        indexer: Arc<CodeIndexer<P>>,
    ) -> anyhow::Result<Self> {
        let (notify_tx, mut notify_rx) = mpsc::channel::<PathBuf>(64);

        let mut debouncer = new_debouncer(
            Duration::from_secs(1),
            move |events: Result<Vec<notify_debouncer_mini::DebouncedEvent>, notify::Error>| {
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

    use zeph_llm::provider::{ChatStream, LlmProvider, Message};

    #[derive(Debug, Clone)]
    struct FakeProvider;

    impl LlmProvider for FakeProvider {
        fn name(&self) -> &'static str {
            "fake"
        }

        async fn chat(&self, _messages: &[Message]) -> anyhow::Result<String> {
            Ok(String::new())
        }

        async fn chat_stream(&self, _messages: &[Message]) -> anyhow::Result<ChatStream> {
            Ok(Box::pin(tokio_stream::empty()))
        }

        fn supports_streaming(&self) -> bool {
            false
        }

        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![0.0; 384])
        }

        fn supports_embeddings(&self) -> bool {
            true
        }
    }

    async fn create_test_pool() -> sqlx::SqlitePool {
        sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap()
    }

    async fn create_test_indexer() -> Arc<CodeIndexer<FakeProvider>> {
        let store = crate::store::CodeStore::new("http://localhost:6334", create_test_pool().await)
            .unwrap();
        Arc::new(CodeIndexer::new(
            store,
            Arc::new(FakeProvider),
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
