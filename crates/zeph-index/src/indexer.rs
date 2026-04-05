// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Project indexing orchestrator: walk → chunk → embed → store.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use futures::StreamExt as _;
use tokio::sync::watch;

use crate::chunker::{ChunkerConfig, CodeChunk, chunk_file};
use crate::context::contextualize_for_embedding;
use crate::error::{IndexError, Result};
use crate::languages::{detect_language, is_indexable};
use crate::store::{ChunkInsert, CodeStore};
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider;

/// Indexer configuration.
#[derive(Debug, Clone)]
pub struct IndexerConfig {
    pub chunker: ChunkerConfig,
    /// Number of files to process concurrently during `index_project`. Default: 4.
    pub concurrency: usize,
    /// Maximum number of new chunks to batch per file into a single Qdrant upsert. Default: 32.
    pub batch_size: usize,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            chunker: ChunkerConfig::default(),
            concurrency: 4,
            batch_size: 32,
        }
    }
}

/// Snapshot of indexing progress, sent via watch channel.
#[derive(Debug, Clone, Default)]
pub struct IndexProgress {
    /// Number of files processed so far.
    pub files_done: usize,
    /// Total number of indexable files discovered.
    pub files_total: usize,
    /// Cumulative chunks created across all files.
    pub chunks_created: usize,
}

/// Summary of an indexing run.
#[derive(Debug, Default)]
pub struct IndexReport {
    pub files_scanned: usize,
    pub files_indexed: usize,
    pub chunks_created: usize,
    pub chunks_skipped: usize,
    pub chunks_removed: usize,
    pub errors: Vec<String>,
    pub duration_ms: u64,
}

/// Orchestrates code indexing over a project tree.
pub struct CodeIndexer {
    store: CodeStore,
    provider: Arc<AnyProvider>,
    config: IndexerConfig,
}

impl CodeIndexer {
    #[must_use]
    pub fn new(store: CodeStore, provider: Arc<AnyProvider>, config: IndexerConfig) -> Self {
        Self {
            store,
            provider,
            config,
        }
    }

    /// Full project indexing with incremental change detection.
    ///
    /// # Errors
    ///
    /// Returns an error if the embedding probe or collection setup fails.
    pub async fn index_project(
        &self,
        root: &Path,
        progress_tx: Option<&watch::Sender<IndexProgress>>,
    ) -> Result<IndexReport> {
        let start = std::time::Instant::now();
        let mut report = IndexReport::default();

        let probe = self.provider.embed("probe").await?;
        let vector_size = u64::try_from(probe.len())?;
        self.store.ensure_collection(vector_size).await?;

        let root_buf = root.to_path_buf();
        let (entries, current_files) = tokio::task::spawn_blocking(move || {
            let entries: Vec<_> = ignore::WalkBuilder::new(&root_buf)
                .hidden(true)
                .git_ignore(true)
                .build()
                .flatten()
                .filter(|e| e.file_type().is_some_and(|ft| ft.is_file()) && is_indexable(e.path()))
                .collect();

            let mut current_files: HashSet<String> = HashSet::new();
            for entry in &entries {
                let rel_path = entry
                    .path()
                    .strip_prefix(&root_buf)
                    .unwrap_or(entry.path())
                    .to_string_lossy()
                    .to_string();
                current_files.insert(rel_path);
            }
            (entries, current_files)
        })
        .await
        .map_err(|e| IndexError::Other(format!("directory walk panicked: {e}")))?;

        let total = entries.len();
        tracing::info!(total, "indexing started");

        let concurrency = self.config.concurrency;
        let store = self.store.clone();
        let provider = Arc::clone(&self.provider);
        let config = self.config.clone();

        let mut stream = futures::stream::iter(entries.into_iter().map(|entry| {
            let store = store.clone();
            let provider = Arc::clone(&provider);
            let config = config.clone();
            let rel_path = entry
                .path()
                .strip_prefix(root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .to_string();
            let abs_path = entry.path().to_path_buf();
            async move {
                let worker = FileIndexWorker {
                    store,
                    provider,
                    config,
                };
                let result = worker.index_file(&abs_path, &rel_path).await;
                (rel_path, result)
            }
        }))
        .buffer_unordered(concurrency);

        let mut files_done = 0usize;
        while let Some((rel_path, outcome)) = stream.next().await {
            report.files_scanned += 1;
            files_done += 1;
            match outcome {
                Ok((created, skipped)) => {
                    if created > 0 {
                        report.files_indexed += 1;
                    }
                    report.chunks_created += created;
                    report.chunks_skipped += skipped;
                    tracing::info!(
                        file = %rel_path,
                        progress = format_args!("{files_done}/{total}"),
                        created,
                        skipped,
                    );
                }
                Err(e) => {
                    report.errors.push(format!("{rel_path}: {e:#}"));
                }
            }
            if let Some(tx) = progress_tx {
                let _ = tx.send(IndexProgress {
                    files_done,
                    files_total: total,
                    chunks_created: report.chunks_created,
                });
            }
        }

        let indexed = self.store.indexed_files().await?;
        for old_file in &indexed {
            if !current_files.contains(old_file) {
                match self.store.remove_file_chunks(old_file).await {
                    Ok(n) => report.chunks_removed += n,
                    Err(e) => report.errors.push(format!("cleanup {old_file}: {e:#}")),
                }
            }
        }

        report.duration_ms = start.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
        Ok(report)
    }

    /// Re-index a specific file (for file watcher).
    ///
    /// # Errors
    ///
    /// Returns an error if reading, chunking, or embedding fails.
    pub async fn reindex_file(&self, root: &Path, abs_path: &Path) -> Result<usize> {
        let rel_path = abs_path
            .strip_prefix(root)
            .unwrap_or(abs_path)
            .to_string_lossy()
            .to_string();

        self.store.remove_file_chunks(&rel_path).await?;
        let worker = FileIndexWorker {
            store: self.store.clone(),
            provider: Arc::clone(&self.provider),
            config: self.config.clone(),
        };
        let (created, _) = worker.index_file(abs_path, &rel_path).await?;
        Ok(created)
    }
}

/// Per-file indexing worker — cloneable and `Send` so it can run inside `buffer_unordered`.
struct FileIndexWorker {
    store: CodeStore,
    provider: Arc<AnyProvider>,
    config: IndexerConfig,
}

impl FileIndexWorker {
    /// Embed and upsert all new chunks from a single file.
    ///
    /// New chunks (those not already in the store) are accumulated, embedded in order, and
    /// upserted in a single batch call to minimise round-trips to `Qdrant` and `SQLite`.
    async fn index_file(&self, abs_path: &Path, rel_path: &str) -> Result<(usize, usize)> {
        let source = tokio::fs::read_to_string(abs_path).await?;
        let lang = detect_language(abs_path).ok_or(IndexError::UnsupportedLanguage)?;

        let chunks = chunk_file(&source, rel_path, lang, &self.config.chunker)?;

        let mut new_chunks: Vec<CodeChunk> = Vec::new();
        let mut skipped = 0usize;

        for chunk in chunks {
            if self.store.chunk_exists(&chunk.content_hash).await? {
                skipped += 1;
            } else {
                new_chunks.push(chunk);
            }
        }

        if new_chunks.is_empty() {
            return Ok((0, skipped));
        }

        // Embed all new chunks and collect (insert, vector) pairs for batched upsert.
        let mut batch: Vec<(ChunkInsert<'_>, Vec<f32>)> = Vec::with_capacity(new_chunks.len());
        for chunk in &new_chunks {
            let embedding_text = contextualize_for_embedding(chunk);
            let vector = self.provider.embed(&embedding_text).await?;
            batch.push((chunk_to_insert(chunk), vector));
        }

        let created = self.store.upsert_chunks_batch(batch).await?.len();

        if created > 0 {
            tracing::debug!("{rel_path}: {created} chunks indexed, {skipped} unchanged");
        }

        Ok((created, skipped))
    }
}

fn chunk_to_insert(chunk: &CodeChunk) -> ChunkInsert<'_> {
    ChunkInsert {
        file_path: &chunk.file_path,
        language: chunk.language.id(),
        node_type: &chunk.node_type,
        entity_name: chunk.entity_name.as_deref(),
        line_start: chunk.line_range.0,
        line_end: chunk.line_range.1,
        code: &chunk.code,
        scope_chain: &chunk.scope_chain,
        content_hash: &chunk.content_hash,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_progress_default() {
        let p = IndexProgress::default();
        assert_eq!(p.files_done, 0);
        assert_eq!(p.files_total, 0);
        assert_eq!(p.chunks_created, 0);
    }

    #[test]
    fn progress_send_no_receivers_is_ignored() {
        let (tx, rx) = tokio::sync::watch::channel(IndexProgress::default());
        drop(rx);
        // send with no receivers must not panic
        let _ = tx.send(IndexProgress {
            files_done: 1,
            files_total: 5,
            chunks_created: 3,
        });
    }

    #[test]
    fn progress_send_multiple_times_accumulates() {
        let (tx, rx) = tokio::sync::watch::channel(IndexProgress::default());
        for i in 1..=3usize {
            let _ = tx.send(IndexProgress {
                files_done: i,
                files_total: 3,
                chunks_created: i * 2,
            });
        }
        let p = rx.borrow();
        assert_eq!(p.files_done, 3);
        assert_eq!(p.files_total, 3);
        assert_eq!(p.chunks_created, 6);
    }

    #[test]
    fn progress_none_tx_skips_send() {
        // When progress_tx is None the loop body must not panic — verified by
        // constructing the same conditional used in index_project.
        let progress_tx: Option<&tokio::sync::watch::Sender<IndexProgress>> = None;
        let entries = [1usize, 2, 3];
        for (i, _) in entries.iter().enumerate() {
            if let Some(tx) = progress_tx {
                let _ = tx.send(IndexProgress {
                    files_done: i + 1,
                    files_total: entries.len(),
                    chunks_created: 0,
                });
            }
        }
        // reaching here means no panic when tx is None
    }

    #[test]
    fn chunk_to_insert_maps_fields() {
        let chunk = CodeChunk {
            code: "fn test() {}".to_string(),
            file_path: "src/lib.rs".to_string(),
            language: crate::languages::Lang::Rust,
            node_type: "function_item".to_string(),
            entity_name: Some("test".to_string()),
            line_range: (1, 3),
            scope_chain: "Foo".to_string(),
            imports: String::new(),
            content_hash: "abc".to_string(),
        };

        let insert = chunk_to_insert(&chunk);
        assert_eq!(insert.file_path, "src/lib.rs");
        assert_eq!(insert.language, "rust");
        assert_eq!(insert.entity_name, Some("test"));
        assert_eq!(insert.line_start, 1);
        assert_eq!(insert.line_end, 3);
    }

    #[test]
    fn default_config() {
        let config = IndexerConfig::default();
        assert_eq!(config.chunker.target_size, 600);
        assert_eq!(config.concurrency, 4);
        assert_eq!(config.batch_size, 32);
    }

    #[test]
    fn indexer_config_custom_concurrency_and_batch_size() {
        let config = IndexerConfig {
            concurrency: 8,
            batch_size: 64,
            ..IndexerConfig::default()
        };
        assert_eq!(config.concurrency, 8);
        assert_eq!(config.batch_size, 64);
    }

    #[test]
    fn index_report_defaults() {
        let report = IndexReport::default();
        assert_eq!(report.files_scanned, 0);
        assert!(report.errors.is_empty());
    }
}
