// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Project indexing orchestrator: walk → chunk → embed → store.
//!
//! The top-level type is [`CodeIndexer`]. It drives a full project index via
//! [`CodeIndexer::index_project`] and supports incremental updates via
//! [`CodeIndexer::reindex_file`] (called by the file watcher).
//!
//! ## Concurrency model
//!
//! Files are processed in two nested loops:
//!
//! 1. **Memory batches** — files are split into groups of
//!    [`IndexerConfig::memory_batch_size`] to bound peak in-flight state.
//! 2. **Per-batch concurrency** — within each memory batch, files are processed
//!    concurrently up to [`IndexerConfig::embed_concurrency`] using
//!    `futures::stream::buffer_unordered`.
//!
//! Chunks that already exist in the store (matched by content hash) are skipped
//! without any embedding call, making re-runs over an unchanged project O(1) in
//! LLM API cost.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use futures::StreamExt as _;
use tokio::sync::watch;

use crate::chunker::{ChunkerConfig, CodeChunk, chunk_file};
use crate::context::contextualize_for_embedding;
use crate::error::{IndexError, Result};
use crate::languages::{detect_language, is_indexable};
use crate::store::{ChunkInsert, CodeStore};
use zeph_common::BlockingSpawner;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider;

/// Monotonically increasing counter for generating unique `chunk_file` task names.
///
/// Multiple concurrent `index_file` calls use the same logical name `"chunk_file"`.
/// The supervisor aborts any existing task with the same name on re-registration, so
/// each spawn must get a unique name to avoid silently aborting in-flight tasks.
static CHUNK_TASK_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Configuration for [`CodeIndexer`].
///
/// All fields have reasonable defaults via [`Default`]. Override individual fields
/// when you need to tune throughput, memory use, or API rate limits.
///
/// # Examples
///
/// ```no_run
/// use zeph_index::indexer::IndexerConfig;
///
/// let config = IndexerConfig::default();
/// assert_eq!(config.concurrency, 2);
/// assert_eq!(config.embed_concurrency, 1);
///
/// // High-throughput mode for a fast local embedding server.
/// let fast = IndexerConfig {
///     embed_concurrency: 8,
///     memory_batch_size: 64,
///     ..IndexerConfig::default()
/// };
/// ```
#[derive(Debug, Clone)]
pub struct IndexerConfig {
    /// Chunker configuration controlling chunk size thresholds.
    pub chunker: ChunkerConfig,
    /// Number of files to process concurrently within each memory batch. Default: 2.
    pub concurrency: usize,
    /// Maximum number of new chunks to upsert per Qdrant call. Default: 16.
    ///
    /// Larger values reduce round-trips but increase per-call memory.
    pub batch_size: usize,
    /// Number of files per outer memory batch during initial indexing. Default: 16.
    ///
    /// Lowering this reduces peak heap usage at the cost of more `yield_now` calls.
    pub memory_batch_size: usize,
    /// Maximum file size in bytes. Files larger than this are silently skipped. Default: 512 KiB.
    ///
    /// Large files (e.g. generated code, vendored libraries) rarely provide useful
    /// retrieval signal and are expensive to embed.
    pub max_file_bytes: usize,
    /// Maximum parallel `embed_batch` calls per memory batch. Default: 1.
    ///
    /// Keep this low when using hosted embedding APIs with strict TPM rate limits.
    pub embed_concurrency: usize,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            chunker: ChunkerConfig::default(),
            concurrency: 2,
            batch_size: 16,
            memory_batch_size: 16,
            max_file_bytes: 512 * 1024,
            embed_concurrency: 1,
        }
    }
}

/// Snapshot of indexing progress, sent through a [`tokio::sync::watch`] channel.
///
/// The caller passes an `Option<&watch::Sender<IndexProgress>>` to
/// [`CodeIndexer::index_project`]. Each time a file completes the sender receives an
/// updated snapshot so the TUI or CLI can display a live progress bar.
///
/// # Examples
///
/// ```no_run
/// use tokio::sync::watch;
/// use zeph_index::indexer::IndexProgress;
///
/// let (tx, mut rx) = watch::channel(IndexProgress::default());
/// tx.send(IndexProgress { files_done: 1, files_total: 10, chunks_created: 5 }).unwrap();
/// assert_eq!(rx.borrow().files_done, 1);
/// ```
#[derive(Debug, Clone, Default)]
pub struct IndexProgress {
    /// Number of files fully processed so far.
    pub files_done: usize,
    /// Total number of indexable files discovered in the project root.
    pub files_total: usize,
    /// Cumulative number of new chunks created across all processed files.
    pub chunks_created: usize,
}

/// Summary statistics produced at the end of a full [`CodeIndexer::index_project`] run.
///
/// Errors are collected rather than short-circuiting so the majority of the project
/// is indexed even when individual files fail (e.g. due to transient IO errors or
/// unsupported encodings).
#[derive(Debug, Default)]
pub struct IndexReport {
    /// Total number of files visited by the directory walker.
    pub files_scanned: usize,
    /// Number of files that produced at least one new chunk.
    pub files_indexed: usize,
    /// New chunks embedded and upserted into Qdrant.
    pub chunks_created: usize,
    /// Chunks skipped because an identical content hash already exists in the store.
    pub chunks_skipped: usize,
    /// Chunks deleted from the store because their file was removed from the project.
    pub chunks_removed: usize,
    /// Per-file error messages collected during the run.
    pub errors: Vec<String>,
    /// Wall-clock duration of the entire run in milliseconds.
    pub duration_ms: u64,
}

/// Orchestrates code indexing over a project tree.
///
/// `CodeIndexer` is the primary driver of the indexing pipeline. It walks the file
/// tree, delegates per-file work to `FileIndexWorker`, and coordinates the Qdrant +
/// `SQLite` writes via [`CodeStore`].
///
/// # Cloning and concurrency
///
/// `CodeIndexer` is **not** `Clone` — it is typically wrapped in an [`Arc`] and shared
/// between the initial indexing task and the file watcher.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
/// use zeph_index::indexer::{CodeIndexer, IndexerConfig};
/// use zeph_index::store::CodeStore;
/// # async fn example() -> zeph_index::Result<()> {
/// # let store: CodeStore = panic!("placeholder");
/// # let provider: Arc<zeph_llm::any::AnyProvider> = panic!("placeholder");
///
/// let indexer = CodeIndexer::new(store, provider, IndexerConfig::default());
/// let report = indexer.index_project(std::path::Path::new("."), None).await?;
/// println!("indexed {} files in {}ms", report.files_indexed, report.duration_ms);
/// # Ok(())
/// # }
/// ```
pub struct CodeIndexer {
    store: CodeStore,
    provider: Arc<AnyProvider>,
    config: IndexerConfig,
    /// Optional supervised spawner for `chunk_file` blocking tasks.
    ///
    /// When `Some`, each `chunk_file` call is routed through the spawner so it
    /// appears in the supervisor registry (snapshot, graceful shutdown, metrics).
    /// When `None`, falls back to `tokio::task::spawn_blocking`.
    spawner: Option<Arc<dyn BlockingSpawner>>,
    /// Re-entrancy guard: prevents concurrent `index_project` runs on the same indexer.
    indexing: Arc<AtomicBool>,
}

impl CodeIndexer {
    /// Create a new `CodeIndexer`.
    ///
    /// The `store` and `provider` are cloned cheaply (reference-counted) across
    /// the concurrent file-processing tasks.
    #[must_use]
    pub fn new(store: CodeStore, provider: Arc<AnyProvider>, config: IndexerConfig) -> Self {
        Self {
            store,
            provider,
            config,
            spawner: None,
            indexing: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Attach a supervised blocking spawner for `chunk_file` tasks.
    ///
    /// When set, each `chunk_file` call is routed through the spawner so it is
    /// visible in supervisor snapshots and subject to graceful shutdown.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use zeph_index::indexer::{CodeIndexer, IndexerConfig};
    /// use zeph_index::store::CodeStore;
    /// use zeph_common::BlockingSpawner;
    ///
    /// # fn example(
    /// #     store: CodeStore,
    /// #     provider: Arc<zeph_llm::any::AnyProvider>,
    /// #     spawner: Arc<dyn BlockingSpawner>,
    /// # ) {
    /// let indexer = CodeIndexer::new(store, provider, IndexerConfig::default())
    ///     .with_spawner(spawner);
    /// # }
    /// ```
    #[must_use]
    pub fn with_spawner(mut self, spawner: Arc<dyn BlockingSpawner>) -> Self {
        self.spawner = Some(spawner);
        self
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
        if self
            .indexing
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            tracing::info!("index_project already running, skipping concurrent request");
            return Ok(IndexReport::default());
        }
        let _guard = IndexingGuard(Arc::clone(&self.indexing));

        let start = std::time::Instant::now();
        let mut report = IndexReport::default();

        self.ensure_collection_for_provider().await?;
        let (entries, current_files) = self.walk_project_files(root).await?;
        let total = entries.len();
        tracing::info!(total, "indexing started");

        let memory_batch_size = self.config.memory_batch_size.max(1);
        let mut files_done = 0usize;
        for batch in entries.chunks(memory_batch_size) {
            self.index_batch(
                batch,
                root,
                total,
                &mut files_done,
                &mut report,
                progress_tx,
            )
            .await;
        }

        self.cleanup_removed_files(&current_files, &mut report)
            .await?;

        report.duration_ms = start.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
        Ok(report)
    }

    async fn ensure_collection_for_provider(&self) -> Result<()> {
        let probe = self.provider.embed("probe").await?;
        let vector_size = u64::try_from(probe.len())?;
        self.store.ensure_collection(vector_size).await
    }

    async fn walk_project_files(
        &self,
        root: &Path,
    ) -> Result<(Vec<ignore::DirEntry>, HashSet<String>)> {
        let root_buf = root.to_path_buf();
        // TODO(#2978-walk): directory walk is left as raw spawn_blocking; routing it
        // through BlockingSpawner is out of scope (single short-lived operation per run).
        tokio::task::spawn_blocking(move || {
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
        .map_err(|e| IndexError::Other(format!("directory walk panicked: {e:#}")))
    }

    #[allow(clippy::too_many_arguments)]
    async fn index_batch(
        &self,
        batch: &[ignore::DirEntry],
        root: &Path,
        total: usize,
        files_done: &mut usize,
        report: &mut IndexReport,
        progress_tx: Option<&watch::Sender<IndexProgress>>,
    ) {
        let store = self.store.clone();
        let provider = Arc::clone(&self.provider);
        let config = self.config.clone();
        let spawner = self.spawner.clone();
        let concurrency = self.config.embed_concurrency.max(1);

        let file_pairs = make_file_pairs(batch, root);

        let mut stream =
            futures::stream::iter(file_pairs.into_iter().map(|(rel_path, abs_path)| {
                let store = store.clone();
                let provider = Arc::clone(&provider);
                let config = config.clone();
                let spawner = spawner.clone();
                async move {
                    let worker = FileIndexWorker {
                        store,
                        provider,
                        config,
                        spawner,
                    };
                    let result = worker.index_file(&abs_path, &rel_path).await;
                    (rel_path, result)
                }
            }))
            .buffer_unordered(concurrency);

        while let Some((rel_path, outcome)) = stream.next().await {
            report.files_scanned += 1;
            *files_done += 1;
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
                    files_done: *files_done,
                    files_total: total,
                    chunks_created: report.chunks_created,
                });
            }
        }

        // Drop stream to release all in-flight future state before the next batch.
        drop(stream);
        tokio::task::yield_now().await;
    }

    async fn cleanup_removed_files(
        &self,
        current_files: &HashSet<String>,
        report: &mut IndexReport,
    ) -> Result<()> {
        let indexed = self.store.indexed_files().await?;
        for old_file in &indexed {
            if !current_files.contains(old_file) {
                match self.store.remove_file_chunks(old_file).await {
                    Ok(n) => report.chunks_removed += n,
                    Err(e) => report.errors.push(format!("cleanup {old_file}: {e:#}")),
                }
            }
        }
        Ok(())
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
            spawner: self.spawner.clone(),
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
    spawner: Option<Arc<dyn BlockingSpawner>>,
}

impl FileIndexWorker {
    /// Embed and upsert all new chunks from a single file.
    ///
    /// New chunks (those not already in the store) are accumulated, embedded in order, and
    /// upserted in a single batch call to minimise round-trips to `Qdrant` and `SQLite`.
    async fn index_file(&self, abs_path: &Path, rel_path: &str) -> Result<(usize, usize)> {
        let metadata = tokio::fs::metadata(abs_path).await?;
        if metadata.len() > self.config.max_file_bytes as u64 {
            tracing::debug!(
                file = %abs_path.display(),
                size = metadata.len(),
                "skipping oversized file"
            );
            return Ok((0, 0));
        }
        let source = tokio::fs::read_to_string(abs_path).await?;
        let lang = detect_language(abs_path).ok_or(IndexError::UnsupportedLanguage)?;

        let rel_path_owned = rel_path.to_owned();
        let chunker_config = self.config.chunker.clone();
        let chunks = if let Some(ref spawner) = self.spawner {
            // Route through the supervised spawner so the task appears in registry.
            // BlockingSpawner::spawn_blocking_named is object-safe (returns JoinHandle<()>),
            // so we communicate the typed result via a oneshot channel.
            //
            // Each spawn gets a unique name to prevent the supervisor's "abort if same
            // name already exists" logic from silently aborting concurrent in-flight tasks
            // when embed_concurrency > 1.
            let task_id = CHUNK_TASK_COUNTER.fetch_add(1, Ordering::Relaxed);
            let task_name: std::sync::Arc<str> =
                std::sync::Arc::from(format!("chunk_file_{task_id}").as_str());
            let (result_tx, result_rx) = tokio::sync::oneshot::channel();
            let _join = spawner.spawn_blocking_named(
                task_name,
                Box::new(move || {
                    let result = chunk_file(&source, &rel_path_owned, lang, &chunker_config);
                    let _ = result_tx.send(result);
                }),
            );
            result_rx
                .await
                .map_err(|_| IndexError::Other("chunk_file task dropped result".to_owned()))??
        } else {
            tokio::task::spawn_blocking(move || {
                chunk_file(&source, &rel_path_owned, lang, &chunker_config)
            })
            .await
            .map_err(|e| IndexError::Other(format!("chunk_file panicked: {e}")))??
        };

        // Batch-check which hashes already exist to avoid N individual queries.
        let all_hashes: Vec<&str> = chunks.iter().map(|c| c.content_hash.as_str()).collect();
        let existing = self.store.existing_hashes(&all_hashes).await?;

        let mut new_chunks: Vec<CodeChunk> = Vec::new();
        let mut skipped = 0usize;

        for chunk in chunks {
            if existing.contains(&chunk.content_hash) {
                skipped += 1;
            } else {
                new_chunks.push(chunk);
            }
        }

        if new_chunks.is_empty() {
            return Ok((0, skipped));
        }

        // Embed all new chunks in a single batch call, then zip with inserts.
        let embedding_texts: Vec<String> =
            new_chunks.iter().map(contextualize_for_embedding).collect();
        let text_refs: Vec<&str> = embedding_texts.iter().map(String::as_str).collect();
        let vectors = self.provider.embed_batch(&text_refs).await?;

        let batch: Vec<(ChunkInsert<'_>, Vec<f32>)> = new_chunks
            .iter()
            .zip(vectors)
            .map(|(chunk, vector)| (chunk_to_insert(chunk), vector))
            .collect();

        let created = match tokio::time::timeout(
            Duration::from_secs(30),
            self.store.upsert_chunks_batch(batch),
        )
        .await
        {
            Ok(Ok(inserted)) => inserted.len(),
            Ok(Err(e)) => {
                tracing::warn!("upsert_chunks_batch failed, skipping batch: {e}");
                0
            }
            Err(_elapsed) => {
                tracing::warn!(
                    "upsert_chunks_batch timed out after 30s, skipping batch of {} chunks",
                    new_chunks.len()
                );
                0
            }
        };

        if created > 0 {
            tracing::debug!("{rel_path}: {created} chunks indexed, {skipped} unchanged");
        }

        Ok((created, skipped))
    }
}

fn make_file_pairs(batch: &[ignore::DirEntry], root: &Path) -> Vec<(String, std::path::PathBuf)> {
    batch
        .iter()
        .map(|entry| {
            let rel = entry
                .path()
                .strip_prefix(root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .to_string();
            let abs = entry.path().to_path_buf();
            (rel, abs)
        })
        .collect()
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

/// RAII guard that resets the re-entrancy flag when dropped.
struct IndexingGuard(Arc<AtomicBool>);

impl Drop for IndexingGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
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
        assert_eq!(config.concurrency, 2);
        assert_eq!(config.batch_size, 16);
        assert_eq!(config.embed_concurrency, 1);
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

    /// Verify that `chunk_file` runs inside `spawn_blocking` and that the dedup path
    /// (all hashes already in `SQLite`) reaches `Ok((0, N))` without touching Qdrant.
    ///
    /// Two assertions:
    /// 1. First `index_file` call with pre-seeded hashes → `(0, N)` (all skipped).
    /// 2. Second identical call → same `(0, N)` (dedup is idempotent).
    ///
    /// The test does not require a live Qdrant instance because `upsert_chunks_batch`
    /// returns early when `new_chunks` is empty.
    #[tokio::test]
    async fn index_file_spawn_blocking_dedup_path() {
        use std::sync::Arc;
        use tempfile::TempDir;
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;
        use zeph_memory::QdrantOps;

        let dir = TempDir::new().unwrap();
        let rs_path = dir.path().join("sample.rs");
        std::fs::write(
            &rs_path,
            "pub fn hello() -> &'static str { \"hello\" }\n\
             pub fn world() -> &'static str { \"world\" }\n",
        )
        .unwrap();

        let pool = zeph_db::DbConfig {
            url: ":memory:".to_string(),
            ..Default::default()
        }
        .connect()
        .await
        .unwrap();

        // Pre-seed the chunk hashes into SQLite so `existing_hashes` returns them all
        // and `new_chunks` is empty — Qdrant upsert is never called.
        let source = std::fs::read_to_string(&rs_path).unwrap();
        let lang = crate::languages::detect_language(&rs_path).unwrap();
        let chunks =
            crate::chunker::chunk_file(&source, "sample.rs", lang, &ChunkerConfig::default())
                .unwrap();
        let chunk_count = chunks.len();
        assert!(chunk_count > 0, "test file must produce at least one chunk");

        for (i, chunk) in chunks.iter().enumerate() {
            zeph_db::query(zeph_db::sql!(
                "INSERT INTO chunk_metadata \
                 (qdrant_id, file_path, content_hash, line_start, line_end, language, node_type) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)"
            ))
            .bind(format!("q{i}"))
            .bind("sample.rs")
            .bind(&chunk.content_hash)
            .bind(i64::try_from(chunk.line_range.0).unwrap_or(i64::MAX))
            .bind(i64::try_from(chunk.line_range.1).unwrap_or(i64::MAX))
            .bind("rust")
            .bind("function_item")
            .execute(&pool)
            .await
            .unwrap();
        }

        let ops = QdrantOps::new("http://127.0.0.1:1").unwrap();
        let store = crate::store::CodeStore::with_ops(ops, pool);
        let provider = Arc::new(AnyProvider::Mock(
            MockProvider::default().with_embedding(vec![0.0_f32; 384]),
        ));
        let worker = FileIndexWorker {
            store,
            provider,
            config: IndexerConfig::default(),
            spawner: None,
        };

        // First call: all hashes exist → (0, chunk_count).
        let (created, skipped) = worker.index_file(&rs_path, "sample.rs").await.unwrap();
        assert_eq!(created, 0);
        assert_eq!(skipped, chunk_count);

        // Second call: same result — dedup is idempotent.
        let (created2, skipped2) = worker.index_file(&rs_path, "sample.rs").await.unwrap();
        assert_eq!(created2, 0);
        assert_eq!(skipped2, chunk_count);
    }

    /// Verify that `index_file` works correctly when a `BlockingSpawner` is provided.
    ///
    /// Uses a minimal `MockBlockingSpawner` that delegates to `tokio::task::spawn_blocking`,
    /// exercising the `spawner: Some(...)` branch in `FileIndexWorker::index_file`.
    #[tokio::test]
    async fn index_file_with_blocking_spawner() {
        use std::sync::Arc;
        use tempfile::TempDir;
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;
        use zeph_memory::QdrantOps;

        struct MockBlockingSpawner;

        impl BlockingSpawner for MockBlockingSpawner {
            fn spawn_blocking_named(
                &self,
                _name: std::sync::Arc<str>,
                f: Box<dyn FnOnce() + Send + 'static>,
            ) -> tokio::task::JoinHandle<()> {
                tokio::task::spawn_blocking(f)
            }
        }

        let dir = TempDir::new().unwrap();
        let rs_path = dir.path().join("sample.rs");
        tokio::fs::write(&rs_path, b"fn hello() {}\n")
            .await
            .unwrap();

        let pool = zeph_db::DbConfig {
            url: ":memory:".to_string(),
            ..Default::default()
        }
        .connect()
        .await
        .unwrap();

        let ops = QdrantOps::new("http://127.0.0.1:1").unwrap();
        let store = crate::store::CodeStore::with_ops(ops, pool);
        let provider = Arc::new(AnyProvider::Mock(
            MockProvider::default().with_embedding(vec![0.0_f32; 384]),
        ));
        let worker = FileIndexWorker {
            store,
            provider,
            config: IndexerConfig::default(),
            spawner: Some(Arc::new(MockBlockingSpawner)),
        };

        // With all hashes absent from SQLite the Qdrant upsert would be attempted, but
        // our mock QdrantOps uses port 1 so it would fail. The test verifies that the
        // spawner path is taken by confirming `chunk_file` runs (if it panicked or the
        // oneshot was dropped, we'd get IndexError::Other, not IndexError::VectorStore).
        let result = worker.index_file(&rs_path, "sample.rs").await;
        // Qdrant is unavailable → we expect a VectorStore/Other error, NOT a panic.
        // The important invariant is that we do NOT get "chunk_file task dropped result".
        if let Err(ref e) = result {
            let msg = e.to_string();
            assert!(
                !msg.contains("chunk_file task dropped result"),
                "spawner path must not drop the result channel; got: {msg}"
            );
        }
    }

    /// Verify that the re-entrancy guard resets correctly after a normal run.
    #[test]
    fn indexing_guard_resets_flag_on_drop() {
        let flag = Arc::new(AtomicBool::new(false));
        {
            // Simulate acquiring the guard.
            flag.store(true, Ordering::Relaxed);
            let _guard = IndexingGuard(Arc::clone(&flag));
            assert!(flag.load(Ordering::Relaxed));
        }
        // Guard dropped — flag must be false.
        assert!(!flag.load(Ordering::Relaxed));
    }

    /// Verify that `compare_exchange` rejects a second caller while the flag is set.
    #[test]
    fn indexing_guard_compare_exchange_skips_concurrent() {
        let flag = Arc::new(AtomicBool::new(false));

        // First caller acquires.
        assert!(
            flag.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok(),
            "first caller should succeed"
        );
        // Second caller must be rejected.
        assert!(
            flag.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err(),
            "second caller should be rejected while flag is true"
        );

        // Reset.
        flag.store(false, Ordering::Release);

        // Third caller can acquire again.
        assert!(
            flag.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok(),
            "third caller should succeed after reset"
        );
    }
}
