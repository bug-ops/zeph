//! Project indexing orchestrator: walk → chunk → embed → store.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use crate::chunker::{ChunkerConfig, CodeChunk, chunk_file};
use crate::context::contextualize_for_embedding;
use crate::error::{IndexError, Result};
use crate::languages::{detect_language, is_indexable};
use crate::store::{ChunkInsert, CodeStore};
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider;

/// Indexer configuration.
#[derive(Debug, Clone, Default)]
pub struct IndexerConfig {
    pub chunker: ChunkerConfig,
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
    pub async fn index_project(&self, root: &Path) -> Result<IndexReport> {
        let start = std::time::Instant::now();
        let mut report = IndexReport::default();

        let probe = self.provider.embed("probe").await?;
        let vector_size = u64::try_from(probe.len())?;
        self.store.ensure_collection(vector_size).await?;

        let entries: Vec<_> = ignore::WalkBuilder::new(root)
            .hidden(true)
            .git_ignore(true)
            .build()
            .flatten()
            .filter(|e| e.file_type().is_some_and(|ft| ft.is_file()) && is_indexable(e.path()))
            .collect();

        let total = entries.len();
        tracing::info!(total, "indexing started");

        let mut current_files: HashSet<String> = HashSet::new();

        for (i, entry) in entries.iter().enumerate() {
            report.files_scanned += 1;
            let rel_path = entry
                .path()
                .strip_prefix(root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .to_string();
            current_files.insert(rel_path.clone());

            match self.index_file(entry.path(), &rel_path).await {
                Ok((created, skipped)) => {
                    if created > 0 {
                        report.files_indexed += 1;
                    }
                    report.chunks_created += created;
                    report.chunks_skipped += skipped;
                    tracing::info!(
                        file = %rel_path,
                        progress = format_args!("{}/{total}", i + 1),
                        created,
                        skipped,
                    );
                }
                Err(e) => {
                    report.errors.push(format!("{rel_path}: {e:#}"));
                }
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
        let (created, _) = self.index_file(abs_path, &rel_path).await?;
        Ok(created)
    }

    async fn index_file(&self, abs_path: &Path, rel_path: &str) -> Result<(usize, usize)> {
        let source = tokio::fs::read_to_string(abs_path).await?;
        let lang = detect_language(abs_path).ok_or(IndexError::UnsupportedLanguage)?;

        let chunks = chunk_file(&source, rel_path, lang, &self.config.chunker)?;

        let mut created = 0usize;
        let mut skipped = 0usize;

        for chunk in &chunks {
            if self.store.chunk_exists(&chunk.content_hash).await? {
                skipped += 1;
                continue;
            }

            let embedding_text = contextualize_for_embedding(chunk);
            let vector = self.provider.embed(&embedding_text).await?;

            let insert = chunk_to_insert(chunk);
            self.store.upsert_chunk(&insert, vector).await?;
            created += 1;
        }

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
    }

    #[test]
    fn index_report_defaults() {
        let report = IndexReport::default();
        assert_eq!(report.files_scanned, 0);
        assert!(report.errors.is_empty());
    }
}
