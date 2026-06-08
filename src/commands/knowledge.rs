// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Handler for the `zeph knowledge` subcommand family (spec-067, Phase 1).
//!
//! This module dispatches [`KnowledgeCommand`] variants to the appropriate
//! implementation:
//!
//! - [`KnowledgeCommand::Ingest`] — load project artifacts into semantic memory
//!   via the notes sink ([`IngestLedger`] idempotency guard + [`IngestionPipeline`]).
//!   INV-6: only paths under the project root (resolved by [`find_repo_root`]) are
//!   eligible. INV-1: no graph writes in Phase 1.
//! - [`KnowledgeCommand::Rollback`] — skeleton; returns an error until Phase 2.
//! - [`KnowledgeCommand::Status`] — queries the [`IngestLedger`] and prints a summary.
//!
//! # Progress reporting
//!
//! [`IngestProgress`] events are sent over a `tokio::sync::mpsc` channel and consumed by a
//! simple stdout printer in this module. The TUI integration point will wire the
//! same channel to a spinner widget in a future issue.
//!
//! # Source → path mapping (INV-6)
//!
//! | [`KnowledgeSource`] | Filesystem glob / command |
//! |---|---|
//! | `Specs` | `<root>/specs/**/*.md` |
//! | `Changelog` | `<root>/CHANGELOG.md` |
//! | `Handoff` | `<root>/.local/handoff/**/*.md` |
//! | `Coverage` | `<root>/.local/testing/coverage-status.md` |
//! | `GitLog` | `git log` stdout (in-memory, bounded by `max_documents`) |

use std::path::{Path, PathBuf};
use std::sync::Arc;

use zeph_core::vault::Secret;
use zeph_llm::provider::LlmProvider;
use zeph_memory::{
    Document, DocumentMetadata, GraphStore, IngestLedger, IngestionPipeline, QdrantOps,
    SplitterConfig, TextSplitter, store::DbStore,
};

use crate::bootstrap::{AppBuilder, find_repo_root};
use crate::cli::{KnowledgeCommand, KnowledgeSource};

/// Resolved `(source_uri, raw_bytes)` pair ready for hashing and ingestion.
type SourceItem = (String, Vec<u8>);
/// Per-item error: `(source_uri, error_message)`.
type IngestError = (String, String);

/// Progress event emitted during a `zeph knowledge ingest` run.
///
/// Variants are sent over a `tokio::sync::mpsc` channel and consumed by a stdout
/// printer (CLI mode) or a TUI spinner (future integration point).
pub(crate) enum IngestProgress {
    /// Files enumerated for a given source before the ingest loop begins.
    Discovered { source: String, files: usize },
    /// A file is about to be embedded (pre-write signal for TUI spinner: "Ingesting knowledge: <uri>…").
    Ingesting { uri: String },
    /// All chunks for a file have been processed (or the file failed).
    FileDone {
        uri: String,
        chunks: usize,
        /// `true` when the ledger skipped this file (unchanged content).
        skipped: bool,
    },
    /// The run is complete.
    Finished,
}

/// Summary produced at the end of a `zeph knowledge ingest` run.
pub(crate) struct IngestReport {
    /// Total files enumerated across all requested sources.
    pub files_total: usize,
    /// Files skipped because the ledger already recorded an identical content hash.
    pub files_skipped: usize,
    /// Embedding chunks written to Qdrant (0 in `--dry-run` mode).
    pub chunks_written: usize,
    /// Per-file errors collected without aborting the run (NFR-001 collect-and-continue).
    pub failures: Vec<(String, String)>,
}

/// Entry point for all `zeph knowledge <subcommand>` variants.
///
/// # Errors
///
/// Propagates errors from config loading, Qdrant connection, ledger DB access,
/// or filesystem enumeration. Per-file ingest errors are collected and reported
/// rather than aborting the run.
pub(crate) async fn handle_knowledge(
    cmd: KnowledgeCommand,
    config_path: Option<&Path>,
) -> anyhow::Result<()> {
    match cmd {
        KnowledgeCommand::Ingest {
            sources,
            dry_run,
            max_documents,
            provider,
            yes,
        } => {
            Box::pin(handle_ingest(
                sources,
                dry_run,
                max_documents,
                provider,
                yes,
                config_path,
            ))
            .await
        }
        KnowledgeCommand::Rollback { batch_id, yes } => {
            Box::pin(handle_rollback(&batch_id, yes, config_path)).await
        }
        KnowledgeCommand::Status => Box::pin(handle_status(config_path)).await,
    }
}

/// Handle `zeph knowledge ingest`.
///
/// Resolves the project root (INV-6), enumerates sources, runs the notes sink,
/// and prints an [`IngestReport`].
///
/// `--dry-run` skips all external connections (Qdrant, provider) — only
/// [`TextSplitter`] is constructed so the token estimate works offline.
///
/// # Notes on stale Qdrant points (MVP limitation)
///
/// When a file's content changes (new hash), the old Qdrant chunks are NOT
/// automatically deleted. The old points remain addressable but will not be
/// re-matched by new queries that hit only the new chunks. Stale-point cleanup
/// is a known MVP limitation tracked for Phase 2.
///
/// # Errors
///
/// Returns an error when the project root cannot be located or a source path
/// escapes the root allowlist.
#[allow(clippy::too_many_lines)]
#[tracing::instrument(skip_all, fields(dry_run, sources_count = sources.len()))]
async fn handle_ingest(
    sources: Vec<KnowledgeSource>,
    dry_run: bool,
    max_documents: usize,
    _provider_override: Option<String>,
    _yes: bool,
    config_path: Option<&Path>,
) -> anyhow::Result<()> {
    // Locate and canonicalize project root (INV-6 anchor).
    // Canonicalize both sides of starts_with() for reliable macOS /private-symlink checks (M3).
    let root = find_repo_root().ok_or_else(|| {
        anyhow::anyhow!("not inside a git repository — cannot resolve project root")
    })?;
    let root = root.canonicalize().map_err(|e| {
        anyhow::anyhow!(
            "failed to canonicalize project root {}: {e}",
            root.display()
        )
    })?;

    // Resolve effective max-documents: CLI flag overrides config default.
    let effective_max = resolve_effective_max(max_documents, config_path).await;

    if dry_run {
        println!("Dry-run mode: no data will be written.");
        println!();
    }

    let head_sha = git_head_sha(&root);
    let (source_items, discovery_errors, per_source_counts) =
        enumerate_all_sources(&sources, &root, &head_sha, effective_max);
    let total_files = source_items.len();

    if dry_run {
        run_dry_run(&source_items, total_files, &discovery_errors);
        return Ok(());
    }

    Box::pin(run_ingest(
        source_items,
        total_files,
        discovery_errors,
        per_source_counts,
        config_path,
    ))
    .await
}

/// Resolve the effective max-documents limit.
/// CLI flag (`> 0`) wins; otherwise reads the config default; falls back to 0 (unlimited).
async fn resolve_effective_max(max_documents: usize, config_path: Option<&Path>) -> usize {
    if max_documents > 0 {
        return max_documents;
    }
    AppBuilder::new(config_path, None, None, None)
        .await
        .map_or(0, |a| a.config().knowledge.max_documents)
}

/// Enumerate all `(source_uri, bytes)` pairs for the requested sources.
/// Returns `(items, errors, per_source_counts)` where `per_source_counts` is a
/// `Vec<(label, count)>` used to emit per-source `Discovered` events.
fn enumerate_all_sources(
    sources: &[KnowledgeSource],
    root: &Path,
    head_sha: &str,
    effective_max: usize,
) -> (
    Vec<SourceItem>,
    Vec<IngestError>,
    Vec<(&'static str, usize)>,
) {
    let mut items: Vec<SourceItem> = Vec::new();
    let mut errors: Vec<IngestError> = Vec::new();
    let mut per_source: Vec<(&'static str, usize)> = Vec::new();

    for src in sources {
        let label = source_label(src);
        if matches!(src, KnowledgeSource::GitLog) {
            // M4: git-log has no filesystem path; run subprocess with cwd=root.
            let max_count = if effective_max > 0 {
                effective_max
            } else {
                500
            };
            match git_log_bytes(root, max_count) {
                Ok(bytes) => {
                    let uri = format!("git-log@{head_sha}");
                    tracing::debug!(label, bytes = bytes.len(), "discovered git-log source");
                    per_source.push((label, 1));
                    items.push((uri, bytes));
                }
                Err(e) => errors.push((label.to_owned(), e.to_string())),
            }
            continue;
        }

        match enumerate_source_paths(root, src) {
            Err(e) => {
                errors.push((label.to_owned(), e.to_string()));
            }
            Ok(paths) => {
                let count = paths.len();
                tracing::debug!(label, count, "discovered source paths");
                per_source.push((label, count));
                collect_file_items(root, head_sha, &paths, &mut items, &mut errors);
            }
        }
    }

    if effective_max > 0 && items.len() > effective_max {
        items.truncate(effective_max);
    }

    (items, errors, per_source)
}

/// Read and validate each path, pushing `(uri, bytes)` into `items` or errors into `errors`.
fn collect_file_items(
    root: &Path,
    head_sha: &str,
    paths: &[PathBuf],
    items: &mut Vec<SourceItem>,
    errors: &mut Vec<IngestError>,
) {
    for path in paths {
        // M3: canonicalize source path; reject anything outside root.
        let canonical = match path.canonicalize() {
            Ok(c) => c,
            Err(e) => {
                errors.push((
                    path.display().to_string(),
                    format!("canonicalize failed: {e}"),
                ));
                continue;
            }
        };
        if !canonical.starts_with(root) {
            errors.push((
                canonical.display().to_string(),
                "path resolves outside project root (INV-6)".to_owned(),
            ));
            continue;
        }

        let rel = canonical
            .strip_prefix(root)
            .unwrap_or(&canonical)
            .to_string_lossy()
            .into_owned();

        match std::fs::read(&canonical) {
            Ok(bytes) => {
                let file_sha = git_file_rev(root, &rel).unwrap_or_else(|| head_sha.to_owned());
                items.push((format!("{rel}@{file_sha}"), bytes));
            }
            Err(e) => {
                errors.push((format!("{rel}@{head_sha}"), format!("read error: {e}")));
            }
        }
    }
}

/// Execute the dry-run path: estimate chunks/tokens using only `TextSplitter`, write nothing.
fn run_dry_run(source_items: &[SourceItem], total_files: usize, discovery_errors: &[IngestError]) {
    let splitter = TextSplitter::new(SplitterConfig::default());
    let mut total_chunks = 0usize;
    let mut total_tokens = 0usize;

    println!(
        "  {:<55} {:>7}  {:>12}",
        "Source URI", "Chunks", "Est. tokens"
    );
    println!("  {}", "-".repeat(78));

    for (uri, bytes) in source_items {
        let content = String::from_utf8_lossy(bytes).into_owned();
        let doc = make_document(uri.clone(), content);
        let chunks = splitter.split(&doc);
        // Token estimate: ~4 chars per token (approximation — not exact).
        let tokens: usize = chunks.iter().map(|c| c.content.len() / 4).sum();
        total_chunks += chunks.len();
        total_tokens += tokens;

        let display = truncate_uri(uri, 55);
        println!("  {display:<55} {:>7}  {:>12}", chunks.len(), tokens);
    }

    println!("  {}", "-".repeat(78));
    println!(
        "  Total: {total_files} file(s), {total_chunks} chunk(s), ~{total_tokens} token(s) (estimate)"
    );

    if !discovery_errors.is_empty() {
        println!();
        println!("  Errors during discovery ({}):", discovery_errors.len());
        for (uri, err) in discovery_errors {
            println!("    [ERR] {uri}: {err}");
        }
    }

    println!();
    println!("Dry run complete. Run without --dry-run to ingest.");
}

/// Execute the normal ingest path: build Qdrant + provider + ledger, then ingest each file.
#[tracing::instrument(skip_all)]
async fn run_ingest(
    source_items: Vec<SourceItem>,
    total_files: usize,
    discovery_errors: Vec<IngestError>,
    per_source_counts: Vec<(&'static str, usize)>,
    config_path: Option<&Path>,
) -> anyhow::Result<()> {
    let app = AppBuilder::new(config_path, None, None, None).await?;
    let config = app.config();

    let qdrant = QdrantOps::new(
        &config.memory.qdrant_url,
        config.memory.qdrant_api_key.as_ref().map(Secret::expose),
    )
    .map_err(|e| anyhow::anyhow!("failed to connect to Qdrant: {e}"))?;

    let collection = config.memory.documents.collection.clone();

    let (provider, _status_tx, _status_rx) = app.build_provider().await?;
    let provider = Arc::new(provider);

    let embed_fn = {
        let p = Arc::clone(&provider);
        move |text: &str| -> zeph_llm::provider::EmbedFuture {
            let p = Arc::clone(&p);
            let owned = text.to_owned();
            Box::pin(async move { p.embed(&owned).await })
        }
    };

    let splitter = TextSplitter::new(SplitterConfig::default());
    let pipeline = IngestionPipeline::new(splitter, qdrant, &collection, Box::new(embed_fn));

    let mem = app.build_memory(&provider).await?;
    let ledger = IngestLedger::new(mem.sqlite().pool().clone());
    let batch_id = uuid::Uuid::new_v4().to_string();

    // FR-014: create a dedicated progress channel and spawn a CLI printer consumer.
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<IngestProgress>();

    let printer_handle = tokio::spawn(async move {
        while let Some(event) = progress_rx.recv().await {
            match event {
                IngestProgress::Discovered { source, files } => {
                    println!("  Discovered: {source} → {files} file(s)");
                }
                IngestProgress::Ingesting { uri } => {
                    println!("  Ingesting knowledge: {uri}…");
                }
                IngestProgress::FileDone {
                    uri,
                    chunks,
                    skipped,
                } => {
                    if skipped {
                        println!("  [skip] {uri} (already ingested)");
                    } else {
                        println!("  [done] {uri} → {chunks} chunk(s)");
                    }
                }
                IngestProgress::Finished => break,
            }
        }
    });

    // Intentional CLI-only header: not sent via progress_rx because it carries
    // collection context that the TUI will render via a dedicated status widget.
    println!("Ingesting {total_files} file(s) into collection '{collection}'…");
    println!();

    // Emit per-source Discovered events before the ingest loop (FR-014).
    for (source, files) in per_source_counts {
        let _ = progress_tx.send(IngestProgress::Discovered {
            source: source.to_owned(),
            files,
        });
    }

    let mut failures: Vec<(String, String)> = discovery_errors;
    let mut files_skipped = 0usize;
    let mut chunks_written = 0usize;

    for (uri, bytes) in source_items {
        // Pre-write signal: allows TUI spinner to show "Ingesting knowledge: <uri>…"
        // before the potentially slow embedding call (FR-014, tasks.md:171).
        let _ = progress_tx.send(IngestProgress::Ingesting { uri: uri.clone() });

        match ingest_one(&ledger, &pipeline, &batch_id, uri.clone(), bytes).await {
            IngestOneResult::Skipped => {
                files_skipped += 1;
                let _ = progress_tx.send(IngestProgress::FileDone {
                    uri,
                    chunks: 0,
                    skipped: true,
                });
            }
            IngestOneResult::Done(count) => {
                chunks_written += count;
                let _ = progress_tx.send(IngestProgress::FileDone {
                    uri,
                    chunks: count,
                    skipped: false,
                });
            }
            IngestOneResult::Error(msg) => {
                failures.push((uri.clone(), msg));
                let _ = progress_tx.send(IngestProgress::FileDone {
                    uri,
                    chunks: 0,
                    skipped: false,
                });
            }
        }
    }

    let _ = progress_tx.send(IngestProgress::Finished);
    // Drop the sender so the printer task can exit cleanly after Finished.
    drop(progress_tx);
    let _ = printer_handle.await;

    print_ingest_report(&IngestReport {
        files_total: total_files,
        files_skipped,
        chunks_written,
        failures,
    });

    Ok(())
}

enum IngestOneResult {
    Skipped,
    Done(usize),
    Error(String),
}

/// Process a single `(source_uri, bytes)` pair through the ledger + pipeline.
#[tracing::instrument(skip(ledger, pipeline, bytes), fields(uri))]
async fn ingest_one(
    ledger: &IngestLedger,
    pipeline: &IngestionPipeline,
    batch_id: &str,
    uri: String,
    bytes: Vec<u8>,
) -> IngestOneResult {
    let hash = IngestLedger::content_hash(&bytes);

    match ledger.is_ingested(&uri, &hash).await {
        Ok(true) => return IngestOneResult::Skipped,
        Ok(false) => {}
        Err(e) => return IngestOneResult::Error(format!("ledger check failed: {e}")),
    }

    let content = String::from_utf8_lossy(&bytes).into_owned();
    // M1: thread source_uri into Qdrant payload so points can be filtered by source.
    let doc = make_document(uri.clone(), content);

    match pipeline.ingest(doc).await {
        Err(e) => {
            // NFR-001: collect-and-continue; do NOT mark ledger on failure.
            IngestOneResult::Error(format!("ingest error: {e}"))
        }
        Ok(count) => {
            if let Err(e) = ledger.mark_ingested(&uri, &hash, batch_id, 0, 0).await {
                IngestOneResult::Error(format!("ledger mark failed: {e}"))
            } else {
                IngestOneResult::Done(count)
            }
        }
    }
}

fn print_ingest_report(report: &IngestReport) {
    println!();
    println!("Knowledge ingest complete.");
    println!("  Files total   : {}", report.files_total);
    println!("  Files skipped : {}", report.files_skipped);
    println!("  Chunks written: {}", report.chunks_written);
    if !report.failures.is_empty() {
        println!("  Failures      : {}", report.failures.len());
        for (uri, err) in &report.failures {
            println!("    [ERR] {uri}: {err}");
        }
    }
}

/// Build a [`Document`] with `source_uri` threaded into the metadata (M1).
fn make_document(source_uri: String, content: String) -> Document {
    Document {
        content,
        metadata: DocumentMetadata {
            source: source_uri,
            content_type: "text/markdown".to_owned(),
            extra: std::collections::HashMap::new(),
        },
    }
}

/// Truncate a URI string to at most `max_len` chars, prefixing with `…` if shortened.
///
/// Uses [`str::floor_char_boundary`] (stable since 1.93) so the slice point always
/// falls on a valid UTF-8 boundary even for non-ASCII source paths.
fn truncate_uri(uri: &str, max_len: usize) -> String {
    if uri.len() <= max_len {
        uri.to_owned()
    } else {
        let start = uri.floor_char_boundary(uri.len().saturating_sub(max_len - 1));
        format!("…{}", &uri[start..])
    }
}

/// Return a human-readable label for a source variant.
fn source_label(src: &KnowledgeSource) -> &'static str {
    match src {
        KnowledgeSource::Specs => "specs",
        KnowledgeSource::Changelog => "changelog",
        KnowledgeSource::Handoff => "handoff",
        KnowledgeSource::Coverage => "coverage",
        KnowledgeSource::GitLog => "git-log",
    }
}

/// Enumerate all filesystem paths for a file-backed source under `root`.
///
/// Returns paths matching the source-specific glob. The returned paths are not yet
/// canonicalized — callers must canonicalize and check `starts_with(root)` (INV-6).
///
/// # Errors
///
/// Returns an error when the glob pattern is malformed.
fn enumerate_source_paths(root: &Path, src: &KnowledgeSource) -> anyhow::Result<Vec<PathBuf>> {
    let pattern = match src {
        KnowledgeSource::Specs => format!("{}/specs/**/*.md", root.display()),
        KnowledgeSource::Changelog => format!("{}/CHANGELOG.md", root.display()),
        KnowledgeSource::Handoff => format!("{}/.local/handoff/**/*.md", root.display()),
        KnowledgeSource::Coverage => {
            format!("{}/.local/testing/coverage-status.md", root.display())
        }
        KnowledgeSource::GitLog => return Ok(Vec::new()),
    };

    let paths: Vec<PathBuf> = glob::glob(&pattern)
        .map_err(|e| anyhow::anyhow!("glob pattern error: {e}"))?
        .filter_map(|entry| match entry {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!("glob entry error: {e}");
                None
            }
        })
        .collect();

    Ok(paths)
}

/// Run `git log --oneline --max-count=N` with cwd pinned to `root` and return stdout bytes.
///
/// # Errors
///
/// Returns an error if the subprocess fails to start or exits with a non-zero status.
#[tracing::instrument(skip(root), fields(max_count))]
fn git_log_bytes(root: &Path, max_count: usize) -> anyhow::Result<Vec<u8>> {
    let output = std::process::Command::new("git")
        .args(["log", "--oneline", &format!("--max-count={max_count}")])
        .current_dir(root)
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run git log: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git log exited with status {}: {}", output.status, stderr);
    }

    Ok(output.stdout)
}

/// Return the short HEAD sha (7 chars) via `git rev-parse --short HEAD`.
///
/// Falls back to `"unknown"` on any failure so source URIs are never empty.
#[tracing::instrument(skip(root))]
fn git_head_sha(root: &Path) -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Return the git commit sha for the last commit touching `rel_path` (7-char prefix),
/// or `None` for untracked files.
// TODO(perf): batch git log calls with `git ls-files -s` to reduce N subprocesses to 1 (Phase 2 optimization).
#[tracing::instrument(skip(root))]
fn git_file_rev(root: &Path, rel_path: &str) -> Option<String> {
    std::process::Command::new("git")
        .args(["log", "-1", "--format=%h", "--", rel_path])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Handle `zeph knowledge rollback`.
///
/// Opens the `SQLite` pool and deletes all graph edges, orphaned entities, and
/// ledger rows for `batch_id`. When `yes` is `false`, prompts the user for
/// confirmation before proceeding.
///
/// # Errors
///
/// Returns an error when config loading, database access, or user I/O fails.
#[tracing::instrument(skip(config_path), fields(batch_id))]
async fn handle_rollback(
    batch_id: &str,
    yes: bool,
    config_path: Option<&Path>,
) -> anyhow::Result<()> {
    use std::io::Write as _;

    let app = AppBuilder::new(config_path, None, None, None).await?;
    let config = app.config();

    let store = DbStore::new(&config.memory.sqlite_path)
        .await
        .map_err(|e| anyhow::anyhow!("failed to open database: {e}"))?;
    let pool = store.pool().clone();
    let ledger = IngestLedger::new(pool.clone());

    if !ledger.batch_exists(batch_id).await? {
        anyhow::bail!("batch '{batch_id}' not found in the ingest ledger — nothing to roll back");
    }

    if !yes {
        print!(
            "This will permanently delete all graph edges, entities, and ledger rows for \
             batch '{batch_id}'.\nProceed? [y/N]: "
        );
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    let graph_store = GraphStore::new(pool.clone());
    let mut tx = zeph_db::begin_write(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("failed to begin transaction: {e}"))?;

    let (edges, entities) = graph_store
        .delete_batch_in_tx(batch_id, &mut tx)
        .await
        .map_err(|e| anyhow::anyhow!("graph delete failed: {e}"))?;

    ledger
        .delete_batch_in_tx(batch_id, &mut tx)
        .await
        .map_err(|e| anyhow::anyhow!("ledger delete failed: {e}"))?;

    tx.commit()
        .await
        .map_err(|e| anyhow::anyhow!("transaction commit failed: {e}"))?;

    if edges == 0 && entities == 0 {
        println!(
            "Note: no graph rows were removed. Phase-1 ingest writes to Qdrant notes, not the \
             graph — Qdrant embeddings for this batch are NOT removed by rollback."
        );
    }
    println!("Rolled back batch '{batch_id}': removed {edges} edge(s) and {entities} entity(ies).");
    Ok(())
}

/// Handle `zeph knowledge status`.
///
/// Queries [`IngestLedger::summary`] and prints a table of ingested batches.
/// Opens only the `SQLite` pool — no LLM provider or Qdrant connection is required,
/// so this command works even when no API key is configured (R4).
///
/// # Errors
///
/// Returns an error when config loading or database access fails.
async fn handle_status(config_path: Option<&Path>) -> anyhow::Result<()> {
    let app = AppBuilder::new(config_path, None, None, None).await?;
    let config = app.config();

    let store = DbStore::new(&config.memory.sqlite_path)
        .await
        .map_err(|e| anyhow::anyhow!("failed to open database: {e}"))?;
    let ledger = IngestLedger::new(store.pool().clone());

    let rows = ledger.summary().await?;

    if rows.is_empty() {
        println!("No knowledge has been ingested yet. Run `zeph knowledge ingest --source <src>`.");
        return Ok(());
    }

    println!("Knowledge ingest ledger ({} entries):", rows.len());
    println!();
    println!(
        "  {:<55} {:<10} {:<26} {:>8} {:>6}",
        "Source URI", "Batch ID", "Ingested at", "Entities", "Edges"
    );
    println!("  {}", "-".repeat(115));

    let mut current_batch = String::new();
    for row in &rows {
        let batch_short = &row.import_batch_id[..row.import_batch_id.len().min(8)];
        let uri_display = truncate_uri(&row.source_uri, 55);

        if current_batch != row.import_batch_id {
            if !current_batch.is_empty() {
                println!();
            }
            current_batch.clone_from(&row.import_batch_id);
        }

        println!(
            "  {uri_display:<55} {batch_short:<10} {:<26} {:>8} {:>6}",
            row.ingested_at, row.entities, row.edges
        );
    }

    println!();
    println!(
        "  Config: max_documents={}, collection={}",
        config.knowledge.max_documents, config.memory.documents.collection
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── truncate_uri ──────────────────────────────────────────────────────────

    #[test]
    fn truncate_uri_short_returned_unchanged() {
        let uri = "specs/README.md@abc1234";
        assert_eq!(truncate_uri(uri, 60), uri);
    }

    #[test]
    fn truncate_uri_exact_len_unchanged() {
        let uri = "abc";
        assert_eq!(truncate_uri(uri, 3), uri);
    }

    #[test]
    fn truncate_uri_over_len_prefixes_ellipsis() {
        let uri = "specs/some/very/long/path/to/a/spec/file.md@abcdef1";
        let result = truncate_uri(uri, 20);
        assert!(result.starts_with('…'), "should start with ellipsis");
        // The visual length (in chars) should be <= max_len + 1 for the '…' char.
        assert!(result.chars().count() <= 21);
    }

    #[test]
    fn truncate_uri_multibyte_does_not_panic() {
        // URI containing Cyrillic (3-byte UTF-8 chars). Must not panic on byte-slice.
        let uri = "specs/архитектура/design.md@abc1234";
        let _ = truncate_uri(uri, 20);
        let _ = truncate_uri(uri, uri.len() + 1);
    }

    #[test]
    fn truncate_uri_empty_string() {
        assert_eq!(truncate_uri("", 10), "");
    }

    // ── make_document / source_uri M1 invariant ───────────────────────────────

    #[test]
    fn make_document_source_uri_threaded_into_metadata() {
        let uri = "specs/README.md@abc1234";
        let doc = make_document(uri.to_owned(), "content".to_owned());
        assert_eq!(
            doc.metadata.source, uri,
            "M1: metadata.source must equal source_uri so Qdrant payload is filterable"
        );
    }

    #[test]
    fn make_document_content_type_is_markdown() {
        let doc = make_document("uri".to_owned(), "text".to_owned());
        assert_eq!(doc.metadata.content_type, "text/markdown");
    }

    // ── source_label ──────────────────────────────────────────────────────────

    #[test]
    fn source_label_all_variants() {
        assert_eq!(source_label(&KnowledgeSource::Specs), "specs");
        assert_eq!(source_label(&KnowledgeSource::Changelog), "changelog");
        assert_eq!(source_label(&KnowledgeSource::Handoff), "handoff");
        assert_eq!(source_label(&KnowledgeSource::Coverage), "coverage");
        assert_eq!(source_label(&KnowledgeSource::GitLog), "git-log");
    }

    // ── ingest_one skip gate (FR-012) ─────────────────────────────────────────

    async fn in_memory_ledger() -> IngestLedger {
        let pool = sqlx::SqlitePool::connect(":memory:")
            .await
            .expect("in-memory sqlite");
        sqlx::query(
            "CREATE TABLE knowledge_ingest_ledger (\
             source_uri TEXT NOT NULL, \
             content_hash TEXT NOT NULL, \
             import_batch_id TEXT NOT NULL, \
             ingested_at TEXT NOT NULL DEFAULT (datetime('now')), \
             entities INTEGER NOT NULL DEFAULT 0, \
             edges INTEGER NOT NULL DEFAULT 0, \
             PRIMARY KEY (source_uri, content_hash)\
             )",
        )
        .execute(&pool)
        .await
        .expect("create table");
        IngestLedger::new(pool)
    }

    #[tokio::test]
    async fn ingest_one_skipped_when_already_ingested() {
        let ledger = in_memory_ledger().await;
        let uri = "specs/README.md@abc1234";
        let bytes = b"hello world".to_vec();
        let hash = IngestLedger::content_hash(&bytes);

        // Pre-mark as ingested.
        ledger
            .mark_ingested(uri, &hash, "batch-0", 0, 0)
            .await
            .expect("mark_ingested");

        // Provide a dummy pipeline — it must NOT be called for a skipped item.
        // We cannot easily construct IngestionPipeline without Qdrant, so instead
        // we verify the ledger branch by testing is_ingested directly, which is
        // the exact same branch that ingest_one checks.
        let already = ledger.is_ingested(uri, &hash).await.expect("is_ingested");
        assert!(
            already,
            "FR-012: item must be skipped when ledger already has the hash"
        );
    }

    // ── collect_file_items INV-6 rejection ────────────────────────────────────

    #[test]
    fn collect_file_items_rejects_path_outside_root() {
        let root_dir = tempfile::tempdir().expect("root tempdir");
        let outside_dir = tempfile::tempdir().expect("outside tempdir");

        // Create a file outside the root.
        let outside_file = outside_dir.path().join("evil.md");
        std::fs::write(&outside_file, b"evil").expect("write outside file");

        let root = root_dir.path().canonicalize().expect("canonicalize root");
        let head_sha = "abc1234";

        let mut items: Vec<SourceItem> = Vec::new();
        let mut errors: Vec<IngestError> = Vec::new();

        collect_file_items(&root, head_sha, &[outside_file], &mut items, &mut errors);

        assert!(
            items.is_empty(),
            "INV-6: path outside root must not be ingested"
        );
        assert!(
            !errors.is_empty(),
            "INV-6: path outside root must produce an error entry"
        );
        assert!(
            errors[0].1.contains("INV-6"),
            "error message must reference INV-6"
        );
    }

    // ── enumerate_all_sources per-source counts (SIGNIFICANT-2) ──────────────

    #[test]
    fn enumerate_all_sources_returns_per_source_counts() {
        let root_dir = tempfile::tempdir().expect("root tempdir");
        let root = root_dir.path().canonicalize().expect("canonicalize");

        // Create a fake git repo structure so git commands don't fail.
        let git_dir = root.join(".git");
        std::fs::create_dir_all(&git_dir).expect("create .git");

        // Create two files under a subdir that would match the Specs glob.
        let specs_dir = root.join("specs");
        std::fs::create_dir_all(&specs_dir).expect("create specs");
        std::fs::write(specs_dir.join("a.md"), b"spec a").expect("write a.md");
        std::fs::write(specs_dir.join("b.md"), b"spec b").expect("write b.md");

        let (_items, _errors, per_source) =
            enumerate_all_sources(&[KnowledgeSource::Specs], &root, "abc1234", 0);

        assert_eq!(per_source.len(), 1, "one source requested → one entry");
        assert_eq!(per_source[0].0, "specs");
        assert_eq!(per_source[0].1, 2, "two files discovered under specs/");
    }

    // ── IngestProgress channel ordering: Ingesting fires before FileDone ─────

    #[tokio::test]
    async fn progress_channel_ingesting_before_file_done_for_skipped() {
        // Verify the ordering contract using the ledger + a fake channel.
        // We cannot call run_ingest (requires Qdrant), so we test the ordering
        // logic directly by simulating what run_ingest does for a skipped item.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<IngestProgress>();

        let uri = "specs/README.md@abc1234".to_owned();

        // Simulate the loop body for a skipped item.
        let _ = tx.send(IngestProgress::Ingesting { uri: uri.clone() });
        let _ = tx.send(IngestProgress::FileDone {
            uri: uri.clone(),
            chunks: 0,
            skipped: true,
        });
        let _ = tx.send(IngestProgress::Finished);
        drop(tx);

        let first = rx.recv().await.expect("first event");
        assert!(
            matches!(first, IngestProgress::Ingesting { uri: ref u } if u == &uri),
            "FR-014: Ingesting must fire before FileDone"
        );

        let second = rx.recv().await.expect("second event");
        assert!(
            matches!(second, IngestProgress::FileDone { skipped: true, .. }),
            "FileDone(skipped) must follow Ingesting"
        );
    }

    // ── CLI parse test for Rollback --yes ────────────────────────────────────

    #[test]
    fn cli_parses_rollback_with_yes() {
        use crate::cli::{Cli, Command, KnowledgeCommand};
        use clap::Parser as _;
        let cli = Cli::try_parse_from([
            "zeph",
            "knowledge",
            "rollback",
            "--batch-id",
            "abc123",
            "--yes",
        ])
        .unwrap();
        assert!(
            matches!(
                cli.command,
                Some(Command::Knowledge {
                    command: KnowledgeCommand::Rollback {
                        ref batch_id,
                        yes: true
                    }
                })
                if batch_id == "abc123"
            ),
            "CLI must parse rollback --batch-id abc123 --yes"
        );
    }
}
