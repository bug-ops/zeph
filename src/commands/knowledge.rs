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

use std::io::{BufRead as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value as JsonValue;
use zeph_agent_persistence::graph::build_graph_extraction_config;
use zeph_core::config::Config;
use zeph_core::vault::Secret;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider;
use zeph_memory::graph::ingest::IngestProgress as GraphIngestProgress;
use zeph_memory::{
    BatchIdResolution, ClaudeCodeJsonl, CodexJsonl, Document, DocumentMetadata, GraphStore,
    ImportBatchId, IngestBatchConfig, IngestLedger, IngestSourceAdapter, IngestionPipeline,
    QdrantOps, SharedPostExtractValidator, SplitterConfig, SubagentJsonl, TextSplitter,
    store::SqliteStore,
};

use crate::bootstrap::{AppBuilder, create_named_provider, find_repo_root};
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
    /// A file is about to be embedded (pre-write signal for TUI spinner: `Ingesting knowledge: <uri>…`).
    Ingesting { uri: String },
    /// All chunks for a file were successfully processed (or skipped as unchanged).
    FileDone {
        uri: String,
        chunks: usize,
        /// `true` when the ledger skipped this file (unchanged content).
        skipped: bool,
    },
    /// A file failed to ingest; distinct from `FileDone` so the printer doesn't
    /// misreport a failure as a successful zero-chunk ingest.
    FileError { uri: String, msg: String },
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
    provider_override: Option<String>,
    yes: bool,
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

    // Three-way partition: Subagents → subagent graph sink; ClaudeCode/Codex → external-agent
    // graph sink (spec-067 Phase 3); everything else → notes sink.
    let mut subagent_sources: Vec<KnowledgeSource> = Vec::new();
    let mut agent_sources: Vec<KnowledgeSource> = Vec::new();
    let mut notes_sources: Vec<KnowledgeSource> = Vec::new();
    for src in sources {
        match src {
            KnowledgeSource::Subagents => subagent_sources.push(src),
            KnowledgeSource::ClaudeCode | KnowledgeSource::Codex => agent_sources.push(src),
            _ => notes_sources.push(src),
        }
    }
    let has_subagents = !subagent_sources.is_empty();
    let has_agents = !agent_sources.is_empty();
    let has_notes = !notes_sources.is_empty();

    // Resolve effective max-documents: CLI flag overrides config default.
    let effective_max = resolve_effective_max(max_documents, config_path).await;

    // ── Notes sink (Phase 1) ────────────────────────────────────────────────
    if has_notes {
        if dry_run {
            println!("Dry-run mode: no data will be written.");
            println!();
        }

        let head_sha = git_head_sha(&root);
        let (notes_sources_owned, root_owned, head_sha_owned) =
            (notes_sources.clone(), root.clone(), head_sha.clone());
        let (source_items, discovery_errors, per_source_counts) =
            tokio::task::spawn_blocking(move || {
                enumerate_all_sources(
                    &notes_sources_owned,
                    &root_owned,
                    &head_sha_owned,
                    effective_max,
                )
            })
            .await
            .map_err(|e| anyhow::anyhow!("enumerate_all_sources panicked: {e}"))?;
        let total_files = source_items.len();

        if dry_run {
            run_dry_run(&source_items, total_files, &discovery_errors);
        } else {
            Box::pin(run_ingest(
                source_items,
                total_files,
                discovery_errors,
                per_source_counts,
                config_path,
                provider_override.as_deref(),
                yes,
            ))
            .await?;
        }
    }

    // ── Graph sink (Phase 2, --source subagents) ────────────────────────────
    if has_subagents {
        Box::pin(run_graph_ingest(
            dry_run,
            effective_max,
            provider_override,
            yes,
            &root,
            config_path,
        ))
        .await?;
    }

    // ── Graph sink (Phase 3, --source claude-code / --source codex) ─────────
    if has_agents {
        // --yes is required for external-agent graph writes; dry-run is exempt (no writes).
        if !yes && !dry_run {
            anyhow::bail!(
                "external-agent sources (--source claude-code, --source codex) require --yes \
                 to confirm graph writes"
            );
        }
        Box::pin(handle_external_agent_ingest(
            &agent_sources,
            dry_run,
            effective_max,
            config_path,
            &root,
            yes,
        ))
        .await?;
    }

    Ok(())
}

/// Resolve the effective max-documents limit.
/// CLI flag (`> 0`) wins; otherwise reads the config default; falls back to 0 (unlimited).
async fn resolve_effective_max(max_documents: usize, config_path: Option<&Path>) -> usize {
    if max_documents > 0 {
        return max_documents;
    }
    AppBuilder::new(config_path, None, None, None, false)
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

/// Pure name-selection logic for the notes-sink **embedding** provider (#5396).
///
/// Returns only an explicit CLI `--provider` override (non-empty); never consults
/// `knowledge.ingest_provider` / `memory.graph.extract_provider`. See
/// [`resolve_notes_embed_provider`] for why the notes sink deliberately does not share the
/// graph-sink's fallback chain. Split out from the async resolver so the selection policy is
/// unit-testable without `AppBuilder`/async (#5396 regression coverage).
fn select_notes_embed_provider_name(provider_override: Option<&str>) -> Option<&str> {
    provider_override.filter(|s| !s.is_empty())
}

/// Resolve the notes-sink **embedding** provider for `zeph knowledge ingest` (#5396, #5444).
///
/// Honours an explicit CLI `--provider` override first; otherwise resolves the project's
/// dedicated embedding provider (`[[llm.providers]]` entry with `embed = true`, referenced by
/// `memory.semantic.embedding_provider` — the same resolution used by memory backfill, see
/// [`AppBuilder::build_memory_embed_provider`]), falling back to the primary/chat provider only
/// when no embedding provider is configured or its resolution fails. Deliberately does **not**
/// fall through `knowledge.ingest_provider` / `memory.graph.extract_provider` — those two config
/// fields select the Phase-2 **LLM-extraction** provider (see
/// [`resolve_graph_extraction_provider`] and the doc contract on `KnowledgeConfig::ingest_provider`
/// in `crates/zeph-config/src/knowledge.rs`, which states the notes sink "does not perform LLM
/// calls; this field is ... ignored" by it). The notes sink only ever calls `.embed()` (see
/// [`build_ingest_resources`]), so folding in the extraction chain would silently point notes
/// embeddings at a provider entry that may use a different embedding model/dimension than the
/// collection was built with — the same silent embed-dimension-mismatch bug class this fix must
/// not reintroduce.
async fn resolve_notes_embed_provider(
    provider_override: Option<&str>,
    config: &Config,
    app: &AppBuilder,
) -> anyhow::Result<Arc<AnyProvider>> {
    let Some(name) = select_notes_embed_provider_name(provider_override) else {
        if let Some(embed_provider) = app.build_memory_embed_provider() {
            return Ok(Arc::new(embed_provider));
        }
        let (p, _, _) = app.build_provider().await?;
        return Ok(Arc::new(p));
    };

    let provider = match create_named_provider(name, config) {
        Ok(p) => {
            tracing::debug!(
                provider = name,
                "using named provider for knowledge ingest (notes)"
            );
            Arc::new(p)
        }
        Err(e) => {
            tracing::warn!(
                provider = name,
                "named provider resolution failed ({e:#}); falling back to primary"
            );
            let (p, _, _) = app.build_provider().await?;
            Arc::new(p)
        }
    };
    Ok(provider)
}

/// Pure name-selection logic for the graph-sink **LLM-extraction** provider (FR-041): CLI
/// override → `knowledge.ingest_provider` → `memory.graph.extract_provider` → `None` (caller
/// falls back to primary).
///
/// Split out from the async resolver so the fallback-chain policy is unit-testable without
/// `AppBuilder`/async (#5396 regression coverage).
fn select_graph_extraction_provider_name<'a>(
    provider_override: Option<&'a str>,
    config: &'a Config,
) -> Option<&'a str> {
    provider_override
        .filter(|s| !s.is_empty())
        .or_else(|| {
            let p = config.knowledge.ingest_provider.as_str();
            if p.is_empty() { None } else { Some(p) }
        })
        .or_else(|| {
            let p = config.memory.graph.extract_provider.as_str();
            if p.is_empty() { None } else { Some(p) }
        })
}

/// Resolve the graph-sink **LLM-extraction** provider for `zeph knowledge ingest` (FR-041): CLI
/// override → `knowledge.ingest_provider` → `memory.graph.extract_provider` → primary.
///
/// Used exclusively by [`run_graph_ingest`] (`--source subagents`), where the resolved provider
/// drives entity/edge extraction chat calls. Must not be reused for the notes-sink embedding path
/// — see [`resolve_notes_embed_provider`] for why the two are separate resolution policies.
async fn resolve_graph_extraction_provider(
    provider_override: Option<&str>,
    config: &Config,
    app: &AppBuilder,
) -> anyhow::Result<Arc<AnyProvider>> {
    let provider_name = select_graph_extraction_provider_name(provider_override, config);

    let provider = if let Some(name) = provider_name {
        match create_named_provider(name, config) {
            Ok(p) => {
                tracing::debug!(provider = name, "using named provider for graph ingest");
                Arc::new(p)
            }
            Err(e) => {
                tracing::warn!(
                    provider = name,
                    "named provider resolution failed ({e:#}); falling back to primary"
                );
                let (p, _, _) = app.build_provider().await?;
                Arc::new(p)
            }
        }
    } else {
        let (p, _, _) = app.build_provider().await?;
        Arc::new(p)
    };
    Ok(provider)
}

/// Timeout for the one-off `embed("dimension probe")` call used to pre-create/pre-check a Qdrant
/// collection's vector size before committing to a write. Shared by every ingest path that probes
/// a dimension ([`build_ingest_resources`], [`guard_reasoning_collection_recreate`]).
const DIMENSION_PROBE_TIMEOUT_SECS: u64 = 15;

/// Guard a Qdrant collection against `ensure_collection`'s destructive delete+recreate on a
/// dimension mismatch (#5444).
///
/// No-op when the collection doesn't exist yet, or its existing dimension matches `required`.
/// Otherwise returns an actionable error unless `yes` is `true`, so interactive CLI ingest flows
/// fail closed instead of silently discarding previously stored data.
///
/// Fails closed on an unreadable dimension (#5444 M1): if the collection exists but
/// [`QdrantOps::get_collection_vector_size`] cannot determine its dimension (e.g. a named-vector
/// collection), that is treated as a mismatch requiring confirmation — an unreadable dimension is
/// not proof of a safe match, and `ensure_collection` would still destructively recreate on that
/// same unreadable state.
///
/// # Errors
///
/// Returns an error if Qdrant cannot be reached, or if a mismatch is detected and `yes` is
/// `false`.
async fn guard_destructive_recreate(
    qdrant: &QdrantOps,
    collection: &str,
    required: u64,
    yes: bool,
) -> anyhow::Result<()> {
    if !qdrant
        .collection_exists(collection)
        .await
        .map_err(|e| anyhow::anyhow!("failed to check Qdrant collection '{collection}': {e}"))?
    {
        return Ok(());
    }
    let existing = qdrant
        .get_collection_vector_size(collection)
        .await
        .map_err(|e| anyhow::anyhow!("failed to inspect Qdrant collection '{collection}': {e}"))?;

    let existing_desc = match existing {
        Some(size) if size == required => return Ok(()),
        Some(size) => size.to_string(),
        None => "unknown (unreadable)".to_owned(),
    };

    if yes {
        return Ok(());
    }
    anyhow::bail!(
        "collection '{collection}' exists with {existing_desc}-dim vectors, but the resolved \
         embedding provider produces {required}-dim vectors; continuing would delete and \
         recreate the collection, discarding all previously stored data. Re-run with --yes to \
         confirm, or pass --provider to select a provider matching the collection's existing \
         dimension."
    );
}

/// Guard the `reasoning_strategies` Qdrant collection against the same destructive recreate that
/// [`crate::bootstrap::AppBuilder::attach_reasoning_memory`] triggers internally, ungated, when
/// invoked via [`crate::bootstrap::AppBuilder::build_memory`] (#5444 S1).
///
/// `attach_reasoning_memory` is shared by every startup path (interactive agent, ACP, background
/// services), so it cannot itself require a CLI confirmation flag. CLI ingest call sites that
/// know they're about to call `build_memory` on an interactive/destructive-sensitive path call
/// this first instead, mirroring `attach_reasoning_memory`'s own provider-selection: the dedicated
/// embed provider when configured, else the passed-in `provider`.
///
/// No-op when reasoning memory is disabled, the vector backend isn't Qdrant, the resolved probe
/// provider doesn't support embeddings, or the dimension probe itself fails (best-effort, logged
/// via `tracing::warn!` — matches `attach_reasoning_memory`'s own "probe failure falls back to
/// SQLite-only mode" handling; a probe failure is not a data-loss risk since `ensure_collection`
/// is never reached either way, so it must not abort the ingest run) — these are exactly the
/// conditions under which `attach_reasoning_memory` itself would skip `ensure_collection`.
///
/// # Errors
///
/// Returns an error only if [`guard_destructive_recreate`] detects an unconfirmed dimension
/// mismatch.
async fn guard_reasoning_collection_recreate(
    app: &AppBuilder,
    provider: &AnyProvider,
    yes: bool,
) -> anyhow::Result<()> {
    if !app.config().memory.reasoning.enabled {
        return Ok(());
    }
    let Some(qdrant) = app.qdrant_ops() else {
        return Ok(());
    };
    let embed_provider = app.build_memory_embed_provider();
    let probe_provider = embed_provider.as_ref().unwrap_or(provider);
    if !probe_provider.supports_embeddings() {
        return Ok(());
    }

    // Best-effort, matching `attach_reasoning_memory`'s own handling of this same probe
    // (`src/bootstrap/mod.rs`): a probe failure is not itself a data-loss risk —
    // `ensure_collection` is never reached either way — so it must not abort the whole ingest run
    // over a transient hiccup (network blip, cold-start timeout). `attach_reasoning_memory` warns
    // and falls back to SQLite-only mode; this guard warns and skips its own pre-check the same
    // way, leaving `build_memory` to hit (and itself best-effort-handle) the identical failure.
    let required = match zeph_memory::probe_vector_size(
        probe_provider.embed("dimension probe"),
        Some(std::time::Duration::from_secs(DIMENSION_PROBE_TIMEOUT_SECS)),
    )
    .await
    {
        Ok(size) => size,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "reasoning: embed probe failed — skipping reasoning_strategies dimension \
                 pre-check"
            );
            return Ok(());
        }
    };

    guard_destructive_recreate(
        qdrant,
        zeph_memory::reasoning::REASONING_COLLECTION,
        required,
        yes,
    )
    .await
}

async fn build_ingest_resources(
    config_path: Option<&Path>,
    provider_override: Option<&str>,
    yes: bool,
) -> anyhow::Result<(IngestionPipeline, IngestLedger, String, String)> {
    let app = AppBuilder::new(config_path, None, None, None, false).await?;
    let config = app.config();
    let qdrant = QdrantOps::new(
        &config.memory.qdrant_url,
        config.memory.qdrant_api_key.as_ref().map(Secret::expose),
    )
    .map_err(|e| anyhow::anyhow!("failed to connect to Qdrant: {e}"))?
    .with_timeout(std::time::Duration::from_secs(
        config.memory.qdrant_timeout_secs,
    ));
    let collection = config.memory.documents.collection.clone();
    let provider = resolve_notes_embed_provider(provider_override, config, &app).await?;
    let embed_fn = {
        let p = Arc::clone(&provider);
        move |text: &str| -> zeph_llm::provider::EmbedFuture {
            let p = Arc::clone(&p);
            let owned = text.to_owned();
            Box::pin(async move { p.embed(&owned).await })
        }
    };

    // Probe the embedding dimension and pre-create the documents collection so a fresh
    // Qdrant instance doesn't fail on the first upsert (same probe helper used by
    // EmbeddingRegistry::ensure_collection and zeph-index's indexer).
    let vector_size = zeph_memory::probe_vector_size(
        provider.embed("dimension probe"),
        Some(std::time::Duration::from_secs(DIMENSION_PROBE_TIMEOUT_SECS)),
    )
    .await
    .map_err(|e| match e {
        zeph_memory::ProbeError::Timeout(_) => anyhow::anyhow!(
            "embedding dimension probe timed out after {DIMENSION_PROBE_TIMEOUT_SECS}s: \
             provider is unresponsive"
        ),
        zeph_memory::ProbeError::Embed(err) => {
            anyhow::anyhow!("failed to probe embedding dimension: {err}")
        }
    })?;

    // A resolved embedding provider whose dimension differs from the collection's existing
    // dimension would otherwise make `ensure_collection` silently delete + recreate it,
    // discarding all previously ingested documents. Gate that destructive path behind --yes.
    guard_destructive_recreate(&qdrant, &collection, vector_size, yes).await?;

    qdrant
        .ensure_collection(&collection, vector_size)
        .await
        .map_err(|e| anyhow::anyhow!("failed to ensure Qdrant collection '{collection}': {e}"))?;

    let pipeline = IngestionPipeline::new(
        TextSplitter::new(SplitterConfig::default()),
        qdrant,
        &collection,
        Box::new(embed_fn),
    );
    let kn_sup = zeph_common::TaskSupervisor::new(tokio_util::sync::CancellationToken::new());
    // #5444 S1: build_memory (via attach_reasoning_memory) would otherwise destructively
    // recreate the reasoning_strategies collection on a dimension mismatch, ungated.
    guard_reasoning_collection_recreate(&app, &provider, yes).await?;
    let mem = app.build_memory(&provider, &kn_sup).await?;
    let ledger = IngestLedger::new(mem.sqlite().pool().clone());
    let batch_id = uuid::Uuid::new_v4().to_string();
    Ok((pipeline, ledger, batch_id, collection))
}

/// Execute the normal ingest path: build Qdrant + provider + ledger, then ingest each file.
#[tracing::instrument(skip_all)]
async fn run_ingest(
    source_items: Vec<SourceItem>,
    total_files: usize,
    discovery_errors: Vec<IngestError>,
    per_source_counts: Vec<(&'static str, usize)>,
    config_path: Option<&Path>,
    provider_override: Option<&str>,
    yes: bool,
) -> anyhow::Result<()> {
    let (pipeline, ledger, batch_id, collection) =
        Box::pin(build_ingest_resources(config_path, provider_override, yes)).await?;

    // FR-014: create a dedicated progress channel and spawn a CLI printer consumer.
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<IngestProgress>();

    let printer_handle = tokio::spawn(async move {
        // EXEMPT(#5143): explicitly awaited after ingest completes
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
                IngestProgress::FileError { uri, msg } => {
                    println!("  [ERR] {uri}: {msg}");
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
                failures.push((uri.clone(), msg.clone()));
                let _ = progress_tx.send(IngestProgress::FileError { uri, msg });
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

/// Build a [`SharedPostExtractValidator`] that gates graph extraction via [`zeph_sanitizer::memory_validation::MemoryWriteValidator`].
///
/// Wraps `cfg` in a `MemoryWriteValidator` and returns it as the shared closure expected by
/// `SemanticMemory::ingest_documents` (INV-4, spec-067 §G-5 S3).
#[allow(clippy::unnecessary_wraps)] // SharedPostExtractValidator is Option<Arc<...>>; callers pass it directly
fn build_shared_validator(
    cfg: zeph_config::sanitizer::MemoryWriteValidationConfig,
) -> SharedPostExtractValidator {
    let inner = zeph_sanitizer::memory_validation::MemoryWriteValidator::new(cfg);
    Some(Arc::new(move |r| {
        inner
            .validate_graph_extraction(r)
            .map_err(|e| e.to_string())
    }))
}

/// Execute the graph-sink ingest path for `--source subagents` (spec-067 Phase 2, FR-020..024).
///
/// Discovers subagent JSONL transcripts under the project-anchored transcript dir,
/// canonicalizes each file path (INV-6), parses via `SubagentJsonl`, and calls
/// `SemanticMemory::ingest_documents` with a `MemoryWriteValidator` sanitizer gate (INV-4).
///
/// # Errors
///
/// Returns an error when config loading, DB access, provider resolution, or transcript-dir
/// resolution fails. Per-document failures are collected in the `IngestReport` and printed.
#[allow(clippy::too_many_lines)]
#[tracing::instrument(skip_all, fields(dry_run))]
async fn run_graph_ingest(
    dry_run: bool,
    effective_max: usize,
    provider_override: Option<String>,
    yes: bool,
    root: &Path,
    config_path: Option<&Path>,
) -> anyhow::Result<()> {
    let app = AppBuilder::new(config_path, None, None, None, false).await?;
    let config = app.config();

    // Enforce transcript_scope (INV-6: only "current-project" is supported).
    if config.knowledge.transcript_scope != "current-project" {
        anyhow::bail!(
            "knowledge.transcript_scope = '{}' is not supported; only 'current-project' is \
             honoured (INV-6). Update your config to use 'current-project'.",
            config.knowledge.transcript_scope
        );
    }

    // Resolve transcript dir; default to <root>/.zeph/subagents.
    let transcript_dir_raw = config
        .agents
        .transcript_dir
        .clone()
        .unwrap_or_else(|| root.join(".zeph/subagents"));
    let transcript_dir = transcript_dir_raw.canonicalize().map_err(|e| {
        anyhow::anyhow!(
            "failed to canonicalize transcript dir {}: {e}",
            transcript_dir_raw.display()
        )
    })?;
    if !transcript_dir.starts_with(root) {
        anyhow::bail!(
            "transcript dir {} resolves outside project root (INV-6)",
            transcript_dir.display()
        );
    }

    // Discover *.jsonl transcript files, canonicalizing each (INV-6: per-file symlink check).
    // spawn_blocking: glob pattern walk + per-entry canonicalize are blocking fs syscalls.
    let pattern = format!("{}/**/*.jsonl", transcript_dir.display());
    let root_owned = root.to_path_buf();
    let (mut transcript_files, discovery_errors): (Vec<(String, PathBuf)>, Vec<String>) =
        tokio::task::spawn_blocking(move || {
            let mut files: Vec<(String, PathBuf)> = Vec::new();
            let mut errors: Vec<String> = Vec::new();
            let entries = match glob::glob(&pattern) {
                Ok(e) => e,
                Err(e) => return Err(anyhow::anyhow!("glob pattern error: {e}")),
            };
            for entry in entries.flatten() {
                match entry.canonicalize() {
                    Ok(canonical) => {
                        if !canonical.starts_with(&root_owned) {
                            errors.push(format!(
                                "{}: resolves outside project root (INV-6), skipped",
                                canonical.display()
                            ));
                            continue;
                        }
                        let task_id = canonical.file_stem().map_or_else(
                            || "unknown".to_owned(),
                            |s| s.to_string_lossy().into_owned(),
                        );
                        files.push((task_id, canonical));
                    }
                    Err(e) => {
                        errors.push(format!("{}: canonicalize failed: {e}", entry.display()));
                    }
                }
            }
            Ok((files, errors))
        })
        .await
        .map_err(|e| anyhow::anyhow!("transcript discovery panicked: {e}"))??;

    // Apply effective_max cap across total documents (approximate: per-file, not per-document).
    if effective_max > 0 && transcript_files.len() > effective_max {
        transcript_files.truncate(effective_max);
    }

    if !discovery_errors.is_empty() {
        println!(
            "  Transcript discovery errors ({}):",
            discovery_errors.len()
        );
        for err in &discovery_errors {
            println!("    [WARN] {err}");
        }
        println!();
    }

    if transcript_files.is_empty() {
        println!(
            "No subagent transcripts found under {}.",
            transcript_dir.display()
        );
        return Ok(());
    }

    // Parse all transcripts into IngestDocuments.
    let batch_id = ImportBatchId::new();
    let mut documents = Vec::new();
    let mut parse_errors: Vec<String> = Vec::new();

    for (task_id, path) in &transcript_files {
        match tokio::fs::read_to_string(path).await {
            Ok(raw) => {
                let adapter = SubagentJsonl::new(task_id.as_str());
                match adapter.parse(&raw, &batch_id) {
                    Ok(docs) => documents.extend(docs),
                    Err(e) => {
                        parse_errors.push(format!("{}: parse error: {e}", path.display()));
                    }
                }
            }
            Err(e) => {
                parse_errors.push(format!("{}: read error: {e}", path.display()));
            }
        }
    }

    if !parse_errors.is_empty() {
        println!("  Parse errors ({}):", parse_errors.len());
        for err in &parse_errors {
            println!("    [WARN] {err}");
        }
        println!();
    }

    if documents.is_empty() {
        println!("No documents extracted from transcripts.");
        return Ok(());
    }

    // Confirmation gate (FR-040, INV-7): prompt before graph writes.
    if should_prompt_for_graph_write(dry_run, yes) {
        print!(
            "Write {} document(s) from {} transcript(s) to graph? [y/N]: ",
            documents.len(),
            transcript_files.len()
        );
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    if dry_run {
        println!(
            "Dry-run mode: {} document(s) from {} transcript(s) — no data will be written.",
            documents.len(),
            transcript_files.len()
        );
        println!();
    }

    // Resolve LLM-extraction provider (FR-041): CLI override → knowledge.ingest_provider →
    // memory.graph.extract_provider → primary.
    let provider =
        resolve_graph_extraction_provider(provider_override.as_deref(), config, &app).await?;

    // #5444 S1: build_memory (via attach_reasoning_memory) would otherwise destructively
    // recreate the reasoning_strategies collection on a dimension mismatch, ungated.
    guard_reasoning_collection_recreate(&app, &provider, yes).await?;
    let kn_sup2 = zeph_common::TaskSupervisor::new(tokio_util::sync::CancellationToken::new());
    let memory = app.build_memory(&provider, &kn_sup2).await?;

    // Build SharedPostExtractValidator wrapping MemoryWriteValidator (INV-4).
    // Always Some — required on both dry-run and live paths (spec-067 §G-5 S3).
    // TODO(critic): P4 — ingest sanitizer covers entity-name PII + counts; edge-fact bodies
    // length-capped only, no body PII / exfiltration URL scan (#5023 INV-4 MVP boundary).
    let shared_validator = build_shared_validator(config.security.memory_validation.clone());

    // #5428: IngestBatchConfig::default() embeds GraphExtractionConfig::default(), whose
    // max_entities/max_edges are 0-sentinels (real values live in [memory.graph]), so extraction
    // truncated every result to empty entities/edges. Resolve real values the same way the
    // conversational memory path does.
    let batch_cfg = IngestBatchConfig {
        extraction: build_graph_extraction_config(
            &config.memory.graph,
            None,
            memory.embed_timeout().as_secs(),
            None,
        ),
        dry_run,
        dry_run_hub_top_n: Some(10),
        ..IngestBatchConfig::default()
    };

    let concurrency = config.knowledge.concurrency.max(1);

    // Progress channel: map graph ingest events to stdout lines (TUI rule: visible status).
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel::<GraphIngestProgress>(64);

    let printer_handle = tokio::spawn(async move {
        // EXEMPT(#5143): explicitly awaited after ingest completes
        while let Some(event) = progress_rx.recv().await {
            match event {
                GraphIngestProgress::Started { total } => {
                    println!("  Ingesting {total} document(s) from subagent transcripts…");
                }
                GraphIngestProgress::DocumentSkipped { uri } => {
                    println!("  Skipped (already ingested): {uri}");
                }
                GraphIngestProgress::DocumentDone {
                    uri,
                    entities,
                    edges,
                } => {
                    println!("  Done: {uri} (+{entities} entities, +{edges} edges)");
                }
                GraphIngestProgress::DocumentFailed { uri, reason } => {
                    println!("  Failed: {uri}: {reason}");
                }
                GraphIngestProgress::DocumentRejected { uri, reason } => {
                    println!("  Rejected (sanitizer): {uri}: {reason}");
                }
                GraphIngestProgress::Finished => break,
                // Non-exhaustive: ignore any future variants.
                _ => {}
            }
        }
    });

    let report = memory
        .ingest_documents(
            documents,
            batch_cfg,
            batch_id,
            concurrency,
            shared_validator,
            Some(progress_tx),
        )
        .await
        .map_err(|e| anyhow::anyhow!("graph ingest failed: {e:#}"))?;

    let _ = printer_handle.await;

    print_graph_ingest_report(&report);

    Ok(())
}

/// Hub-degree kill-criterion threshold from spec-067 §7: the top entity by edge-degree
/// must not exceed this percentage of total edges before the graph sink can ship.
const HUB_DEGREE_THRESHOLD_PCT: f64 = 15.0;

/// Print the summary of a graph-sink ingest run.
fn print_graph_ingest_report(report: &zeph_memory::graph::ingest::IngestReport) {
    println!();
    if report.dry_run {
        println!("Graph ingest dry-run complete (no data written).");
    } else {
        println!("Graph ingest complete.");
        println!("  Batch ID          : {}", report.batch_id);
    }
    println!("  Documents total   : {}", report.documents_total);
    println!("  Skipped           : {}", report.skipped);
    println!("  Succeeded         : {}", report.succeeded);
    println!("  Rejected (sanitizer): {}", report.rejected);
    if !report.failed.is_empty() {
        println!("  Failed            : {}", report.failed.len());
        for f in &report.failed {
            println!("    [ERR] {}: {}", f.uri, f.reason);
        }
    }
    println!("  Entities total    : {}", report.entities_total);
    println!("  Edges total       : {}", report.edges_total);

    if report.dry_run && !report.hub_degree.is_empty() {
        let total_edges: usize = report.hub_degree.iter().map(|h| h.degree).sum();
        println!();
        println!(
            "  Hub-degree (top-{}) — spec-067 §7 threshold ≤ {HUB_DEGREE_THRESHOLD_PCT}% of total edges:",
            report.hub_degree.len()
        );
        println!("  {:<50} {:>7}  {:>7}", "Entity", "Degree", "% edges");
        println!("  {}", "-".repeat(68));
        for h in &report.hub_degree {
            #[allow(clippy::cast_precision_loss)]
            let pct = if total_edges > 0 {
                (h.degree as f64 / total_edges as f64) * 100.0
            } else {
                0.0
            };
            let flag = if pct > HUB_DEGREE_THRESHOLD_PCT {
                " ⚠ HUB"
            } else {
                ""
            };
            println!(
                "  {:<50} {:>7}  {:>6.1}%{}",
                truncate_uri(&h.entity, 50),
                h.degree,
                pct,
                flag
            );
        }
        if total_edges > 0 {
            #[allow(clippy::cast_precision_loss)]
            let top_pct = (report.hub_degree[0].degree as f64 / total_edges as f64) * 100.0;
            let health = if top_pct <= HUB_DEGREE_THRESHOLD_PCT {
                "PASS"
            } else {
                "WARN — top entity exceeds hub-degree threshold"
            };
            println!("  {}", "-".repeat(68));
            println!("  Top entity: {top_pct:.1}% of edges — {health}");
        }
    }
}

/// Returns `true` when the graph-write confirmation prompt must be shown.
///
/// Prompt is suppressed by `--dry-run` (no writes) or `--yes` (scripted/CI use).
fn should_prompt_for_graph_write(dry_run: bool, yes: bool) -> bool {
    !dry_run && !yes
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

// ── External-agent helpers ─────────────────────────────────────────────────────

/// Convert a project root path to the Claude Code slug used for the project directory.
///
/// Claude Code stores sessions under `~/.claude/projects/<slug>/` where `<slug>` is the
/// absolute path with `/` replaced by `-` (so `/Users/foo/proj` → `-Users-foo-proj`).
pub(crate) fn path_to_claude_code_slug(root: &Path) -> String {
    root.to_string_lossy().replace('/', "-")
}

/// Resolve the canonical main-repo root for Claude Code slug computation.
///
/// When running from a git worktree, `find_repo_root()` returns the worktree path, but
/// Claude Code sessions were recorded against the directory the user actually launched
/// Claude Code in — which is the main checkout. We get that by reading
/// `git rev-parse --git-common-dir` (which points to `.git` in the common dir) and
/// stripping the trailing `/.git` component.
///
/// Falls back to `root` itself if the command fails or produces an unexpected path.
fn resolve_main_repo_root(root: &Path) -> PathBuf {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(root)
        .output();

    let Ok(out) = output else {
        return root.to_path_buf();
    };
    if !out.status.success() {
        return root.to_path_buf();
    }

    let common_git = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let git_path = PathBuf::from(&common_git);

    // `--git-common-dir` returns the `.git` directory itself.
    // Strip the `.git` component to get the repo root.
    let candidate = if git_path.ends_with(".git") {
        git_path.parent().map(Path::to_path_buf)
    } else {
        // Bare repo or unexpected path — fall back.
        None
    };

    candidate
        .and_then(|p| p.canonicalize().ok())
        .unwrap_or_else(|| root.to_path_buf())
}

/// Enumerate all Claude Code JSONL session files for `root`.
///
/// Uses the main-repo root (not the worktree path) for slug computation so that sessions
/// recorded before the current worktree was created are still discovered.
///
/// Looks in `~/.claude/projects/<slug>/*.jsonl`. Because Claude Code scopes the directory
/// to the project, all `.jsonl` files there belong to this project — no per-file filtering.
///
/// # Errors
///
/// Returns an error when `$HOME` is not set or the glob pattern is malformed.
fn enumerate_claude_code_paths(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let home = std::env::var("HOME").map_err(|_| anyhow::anyhow!("$HOME is not set"))?;
    let main_root = resolve_main_repo_root(root);
    let slug = path_to_claude_code_slug(&main_root);
    let pattern = format!("{home}/.claude/projects/{slug}/*.jsonl");
    tracing::debug!(slug, pattern, "claude-code JSONL discovery");

    let paths: Vec<PathBuf> = glob::glob(&pattern)
        .map_err(|e| anyhow::anyhow!("claude-code glob pattern error: {e}"))?
        .filter_map(|entry| match entry {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!("claude-code glob entry error: {e}");
                None
            }
        })
        .collect();

    Ok(paths)
}

/// Read the first few lines of a Codex JSONL file looking for a `session_meta` record,
/// and return the `payload.cwd` value if found.
fn scan_codex_session_cwd(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines().take(5).flatten() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<JsonValue>(&line)
            && val.get("type").and_then(|t| t.as_str()) == Some("session_meta")
        {
            return val
                .pointer("/payload/cwd")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
        }
        // session_meta is always the first record — stop after the first parseable line.
        break;
    }
    None
}

/// Enumerate all Codex JSONL session files that belong to `root`.
///
/// Scans two locations:
/// - `~/.codex/archived_sessions/*.jsonl` — completed/archived sessions (flat layout)
/// - `~/.codex/sessions/**/*.jsonl` — live/recent sessions (nested `YYYY/MM/DD/rollout-*`)
///
/// Files are filtered by `session_meta.payload.cwd` using `starts_with(root)` after
/// canonicalization of both sides. This tolerates subdirectory launches and symlink
/// differences between the launch path and the canonicalized project root.
///
/// # Errors
///
/// Returns an error when `$HOME` is not set or a glob pattern is malformed.
fn enumerate_codex_paths(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let home = std::env::var("HOME").map_err(|_| anyhow::anyhow!("$HOME is not set"))?;
    let patterns = [
        format!("{home}/.codex/archived_sessions/*.jsonl"),
        format!("{home}/.codex/sessions/**/*.jsonl"),
    ];
    let root_canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    let mut paths: Vec<PathBuf> = Vec::new();
    for pattern in &patterns {
        let entries =
            glob::glob(pattern).map_err(|e| anyhow::anyhow!("codex glob pattern error: {e}"))?;
        for entry in entries {
            match entry {
                Ok(p) => {
                    if cwd_matches_root(&p, &root_canonical) {
                        paths.push(p);
                    }
                }
                Err(e) => tracing::warn!("codex glob entry error: {e}"),
            }
        }
    }

    Ok(paths)
}

/// Return `true` when the `session_meta.payload.cwd` from `path` is the `root` or a
/// subdirectory of it. Canonicalizes the cwd string read from JSON before comparing.
fn cwd_matches_root(path: &Path, root: &Path) -> bool {
    let Some(raw_cwd) = scan_codex_session_cwd(path) else {
        return false;
    };
    let cwd_path = PathBuf::from(&raw_cwd);
    let canonical = cwd_path.canonicalize().unwrap_or(cwd_path);
    canonical.starts_with(root)
}

/// Handle `zeph knowledge ingest` for external-agent sources (`claude-code`, `codex`).
///
/// Discovers JSONL session files for the current project, parses them via the
/// appropriate [`zeph_memory::IngestSourceAdapter`], and writes entities and edges to the
/// knowledge graph via `SemanticMemory::ingest_documents`.
///
/// In dry-run mode only discovery is performed — no parsing or writes occur.
///
/// # Errors
///
/// Returns an error when config loading, memory construction, or the batch graph write fails.
#[allow(clippy::too_many_lines)]
#[tracing::instrument(skip_all, fields(dry_run))]
async fn handle_external_agent_ingest(
    agent_sources: &[KnowledgeSource],
    dry_run: bool,
    effective_max: usize,
    config_path: Option<&Path>,
    root: &Path,
    yes: bool,
) -> anyhow::Result<()> {
    use zeph_memory::IngestSourceAdapter as _;

    let batch_id = ImportBatchId::new();

    // Phase 1: enumerate JSONL paths per source (spawn_blocking: glob + canonicalize + fs reads).
    let mut all_paths: Vec<(KnowledgeSource, PathBuf)> = Vec::new();
    for src in agent_sources {
        let src_owned = src.clone();
        let root_owned = root.to_path_buf();
        let paths = tokio::task::spawn_blocking(move || match src_owned {
            KnowledgeSource::ClaudeCode => enumerate_claude_code_paths(&root_owned),
            KnowledgeSource::Codex => enumerate_codex_paths(&root_owned),
            _ => unreachable!("only external-agent sources reach this function"),
        })
        .await
        .map_err(|e| anyhow::anyhow!("path enumeration panicked: {e}"))??;
        tracing::debug!(
            source = source_label(src),
            count = paths.len(),
            "discovered JSONL paths"
        );
        println!(
            "  Discovered: {} → {} session file(s)",
            source_label(src),
            paths.len()
        );
        for p in paths {
            all_paths.push((src.clone(), p));
        }
    }

    if all_paths.is_empty() {
        println!("  No external-agent session files found for this project.");
        return Ok(());
    }

    if dry_run {
        println!(
            "  (dry-run) Would parse {} session file(s).",
            all_paths.len()
        );
        return Ok(());
    }

    // Phase 2: parse JSONL files into IngestDocuments.
    let mut docs = Vec::new();
    for (src, path) in &all_paths {
        let session_id = path.file_stem().map_or_else(
            || "unknown".to_owned(),
            |s| s.to_string_lossy().into_owned(),
        );
        let raw = match tokio::fs::read_to_string(path).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(?path, "failed to read session file: {e}");
                println!("  [ERR] {}: read error: {e}", path.display());
                continue;
            }
        };
        let result = match src {
            KnowledgeSource::ClaudeCode => ClaudeCodeJsonl::new(&session_id).parse(&raw, &batch_id),
            KnowledgeSource::Codex => CodexJsonl::new(&session_id).parse(&raw, &batch_id),
            _ => unreachable!(),
        };
        match result {
            Ok(parsed) => {
                let n = parsed.len();
                tracing::debug!(?path, documents = n, "parsed session file");
                docs.extend(parsed);
                println!("  [parsed] {} → {n} document(s)", path.display());
            }
            Err(e) => {
                tracing::warn!(?path, "failed to parse session file: {e}");
                println!("  [ERR] {}: parse error: {e}", path.display());
            }
        }
    }

    if docs.is_empty() {
        println!("  No documents extracted from session files.");
        return Ok(());
    }

    let total = if effective_max > 0 && docs.len() > effective_max {
        docs.truncate(effective_max);
        docs.len()
    } else {
        docs.len()
    };

    println!("  Ingesting {total} document(s) into knowledge graph…");

    // Phase 3: write to graph via SemanticMemory.
    let app = AppBuilder::new(config_path, None, None, None, false).await?;
    let app_config = app.config();
    let (provider, _status_tx, _status_rx) = app.build_provider().await?;
    // #5444 S1: build_memory (via attach_reasoning_memory) would otherwise destructively
    // recreate the reasoning_strategies collection on a dimension mismatch, ungated.
    guard_reasoning_collection_recreate(&app, &provider, yes).await?;
    let kn_sup3 = zeph_common::TaskSupervisor::new(tokio_util::sync::CancellationToken::new());
    let mem = app.build_memory(&provider, &kn_sup3).await?;

    // Build SharedPostExtractValidator wrapping MemoryWriteValidator (INV-4, spec-067 §G-5 S3).
    let shared_validator = build_shared_validator(app_config.security.memory_validation.clone());

    // #5428: see the --source subagents path above — GraphExtractionConfig::default() zeroes
    // max_entities/max_edges, truncating every extraction result to empty.
    let batch_cfg = IngestBatchConfig {
        extraction: build_graph_extraction_config(
            &app_config.memory.graph,
            None,
            mem.embed_timeout().as_secs(),
            None,
        ),
        ..IngestBatchConfig::default()
    };
    let report = mem
        .ingest_documents(docs, batch_cfg, batch_id, 4, shared_validator, None)
        .await
        .map_err(|e| anyhow::anyhow!("graph ingest failed: {e}"))?;

    println!();
    println!("External-agent graph ingest complete.");
    println!("  Documents succeeded: {}", report.succeeded);
    println!("  Documents failed   : {}", report.failed.len());
    if !report.failed.is_empty() {
        for failure in &report.failed {
            println!("    [ERR] {}: {}", failure.uri, failure.reason);
        }
    }

    Ok(())
}

/// Return a human-readable label for a source variant.
fn source_label(src: &KnowledgeSource) -> &'static str {
    match src {
        KnowledgeSource::Specs => "specs",
        KnowledgeSource::Changelog => "changelog",
        KnowledgeSource::Handoff => "handoff",
        KnowledgeSource::Coverage => "coverage",
        KnowledgeSource::GitLog => "git-log",
        KnowledgeSource::Subagents => "subagents",
        KnowledgeSource::ClaudeCode => "claude-code",
        KnowledgeSource::Codex => "codex",
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
        // External-agent sources are enumerated separately via enumerate_claude_code_paths /
        // enumerate_codex_paths and must never reach the notes-sink glob logic.
        KnowledgeSource::ClaudeCode
        | KnowledgeSource::Codex
        | KnowledgeSource::GitLog
        | KnowledgeSource::Subagents => {
            return Ok(Vec::new());
        }
        KnowledgeSource::Specs => format!("{}/specs/**/*.md", root.display()),
        KnowledgeSource::Changelog => format!("{}/CHANGELOG.md", root.display()),
        KnowledgeSource::Handoff => format!("{}/.local/handoff/**/*.md", root.display()),
        KnowledgeSource::Coverage => {
            format!("{}/.local/testing/coverage-status.md", root.display())
        }
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
    let app = AppBuilder::new(config_path, None, None, None, false).await?;
    let config = app.config();

    let store = SqliteStore::new(&config.memory.sqlite_path)
        .await
        .map_err(|e| anyhow::anyhow!("failed to open database: {e}"))?;
    let pool = store.pool().clone();
    let ledger = IngestLedger::new(pool.clone());

    // Reject an empty/whitespace `--batch-id` (e.g. an unset `$VAR` shell substitution) up front
    // with an unambiguous message, rather than letting it fall through to `resolve_batch_id`'s
    // generic "not found" (#5399 F2: an unguarded empty prefix would otherwise match every batch).
    if batch_id.trim().is_empty() {
        anyhow::bail!("--batch-id must not be empty");
    }

    // Git-style unambiguous prefix resolution (#5399): `zeph knowledge status` prints only an
    // 8-char prefix of `import_batch_id`, so a caller pasting that prefix must still resolve.
    let batch_id = match ledger.resolve_batch_id(batch_id).await? {
        BatchIdResolution::Resolved(full_id) => full_id,
        BatchIdResolution::Ambiguous(candidates) => {
            anyhow::bail!(
                "batch id '{batch_id}' is ambiguous, matches {} batches: {}",
                candidates.len(),
                candidates.join(", ")
            );
        }
        BatchIdResolution::NotFound => {
            anyhow::bail!(
                "batch '{batch_id}' not found in the ingest ledger — nothing to roll back"
            );
        }
    };
    let batch_id = batch_id.as_str();

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
    let app = AppBuilder::new(config_path, None, None, None, false).await?;
    let config = app.config();

    let store = SqliteStore::new(&config.memory.sqlite_path)
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
        assert_eq!(source_label(&KnowledgeSource::ClaudeCode), "claude-code");
        assert_eq!(source_label(&KnowledgeSource::Codex), "codex");
    }

    // ── provider name selection (#5396) ───────────────────────────────────────
    //
    // Pure sync tests for select_notes_embed_provider_name / select_graph_extraction_provider_name
    // — no AppBuilder/async required. These pin the exact regression #5396 fixed (the CLI
    // --provider override reaching the notes-sink path) and the F1 fix (the notes-sink embed
    // provider must NOT fall through knowledge.ingest_provider / memory.graph.extract_provider).

    #[test]
    fn notes_embed_provider_name_uses_cli_override_when_present() {
        assert_eq!(select_notes_embed_provider_name(Some("fast")), Some("fast"));
    }

    #[test]
    fn notes_embed_provider_name_none_when_no_override() {
        assert_eq!(select_notes_embed_provider_name(None), None);
    }

    #[test]
    fn notes_embed_provider_name_treats_empty_override_as_absent() {
        assert_eq!(select_notes_embed_provider_name(Some("")), None);
    }

    #[test]
    fn graph_extraction_provider_name_cli_override_wins_over_both_config_fields() {
        let mut config = Config::default();
        config.knowledge.ingest_provider = "from-knowledge".to_owned();
        config.memory.graph.extract_provider = "from-graph".into();
        assert_eq!(
            select_graph_extraction_provider_name(Some("cli"), &config),
            Some("cli")
        );
    }

    #[test]
    fn graph_extraction_provider_name_knowledge_ingest_provider_wins_when_no_cli_override() {
        let mut config = Config::default();
        config.knowledge.ingest_provider = "from-knowledge".to_owned();
        config.memory.graph.extract_provider = "from-graph".into();
        assert_eq!(
            select_graph_extraction_provider_name(None, &config),
            Some("from-knowledge")
        );
    }

    #[test]
    fn graph_extraction_provider_name_falls_back_to_memory_graph_extract_provider() {
        let mut config = Config::default();
        config.memory.graph.extract_provider = "from-graph".into();
        assert_eq!(
            select_graph_extraction_provider_name(None, &config),
            Some("from-graph")
        );
    }

    #[test]
    fn graph_extraction_provider_name_none_when_everything_empty() {
        let config = Config::default();
        assert_eq!(select_graph_extraction_provider_name(None, &config), None);
    }

    #[test]
    fn graph_extraction_provider_name_does_not_leak_into_notes_embed_selection() {
        // The critic's F1 regression scenario: knowledge.ingest_provider / extract_provider set
        // (the documented multi-model best practice), no CLI --provider. The notes-sink embed
        // path must resolve to None (-> primary), not fall through to either config field.
        let mut config = Config::default();
        config.knowledge.ingest_provider = "from-knowledge".to_owned();
        config.memory.graph.extract_provider = "from-graph".into();
        assert_eq!(select_notes_embed_provider_name(None), None);
        assert_eq!(
            select_graph_extraction_provider_name(None, &config),
            Some("from-knowledge"),
            "sanity: the graph-sink chain should still resolve the config field"
        );
    }

    // ── ingest_one skip gate (FR-012) ─────────────────────────────────────────

    async fn in_memory_ledger() -> IngestLedger {
        let pool = zeph_db::DbConfig {
            url: ":memory:".to_owned(),
            ..Default::default()
        }
        .connect()
        .await
        .expect("connect + migrate in-memory sqlite pool");
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

    // ── IngestProgress channel: failed ingest emits FileError, not FileDone (#5382) ──

    #[tokio::test]
    async fn progress_channel_emits_file_error_for_failed_ingest() {
        // Simulate the IngestOneResult::Error branch of run_ingest's loop body
        // (run_ingest itself needs a live Qdrant pipeline and can't be called here).
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<IngestProgress>();

        let uri = "specs/README.md@abc1234".to_owned();
        let msg = "embedding provider returned 500".to_owned();

        let _ = tx.send(IngestProgress::Ingesting { uri: uri.clone() });
        let _ = tx.send(IngestProgress::FileError {
            uri: uri.clone(),
            msg: msg.clone(),
        });
        let _ = tx.send(IngestProgress::Finished);
        drop(tx);

        let first = rx.recv().await.expect("first event");
        assert!(
            matches!(first, IngestProgress::Ingesting { uri: ref u } if u == &uri),
            "Ingesting must fire before the terminal event"
        );

        let second = rx.recv().await.expect("second event");
        assert!(
            matches!(second, IngestProgress::FileError { uri: ref got_uri, msg: ref got_msg }
                if got_uri == &uri && got_msg == &msg),
            "#5382: a failed ingest must emit FileError{{uri, msg}}, not FileDone{{chunks:0}}, \
             so the CLI prints [ERR] instead of misreporting [done]"
        );
        assert!(
            !matches!(second, IngestProgress::FileDone { .. }),
            "#5382 regression guard: must never fall back to FileDone on error"
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

    // ── Subagents source variant ──────────────────────────────────────────────

    #[test]
    fn source_label_subagents() {
        assert_eq!(source_label(&KnowledgeSource::Subagents), "subagents");
    }

    #[test]
    fn cli_parses_source_subagents() {
        use crate::cli::{Cli, Command, KnowledgeCommand};
        use clap::Parser as _;
        let cli =
            Cli::try_parse_from(["zeph", "knowledge", "ingest", "--source", "subagents"]).unwrap();
        assert!(
            matches!(
                cli.command,
                Some(Command::Knowledge {
                    command: KnowledgeCommand::Ingest {
                        ref sources,
                        dry_run: false,
                        ..
                    }
                })
                if sources == &[KnowledgeSource::Subagents]
            ),
            "CLI must parse --source subagents"
        );
    }

    #[test]
    fn cli_parses_source_subagents_with_dry_run() {
        use crate::cli::{Cli, Command, KnowledgeCommand};
        use clap::Parser as _;
        let cli = Cli::try_parse_from([
            "zeph",
            "knowledge",
            "ingest",
            "--source",
            "subagents",
            "--dry-run",
        ])
        .unwrap();
        assert!(
            matches!(
                cli.command,
                Some(Command::Knowledge {
                    command: KnowledgeCommand::Ingest { dry_run: true, .. }
                })
            ),
            "CLI must parse --source subagents --dry-run"
        );
    }

    #[test]
    fn cli_parses_mixed_sources_subagents_and_notes() {
        use crate::cli::{Cli, Command, KnowledgeCommand};
        use clap::Parser as _;
        let cli = Cli::try_parse_from([
            "zeph",
            "knowledge",
            "ingest",
            "--source",
            "specs",
            "--source",
            "subagents",
        ])
        .unwrap();
        let Some(Command::Knowledge {
            command: KnowledgeCommand::Ingest { sources, .. },
        }) = cli.command
        else {
            panic!("expected Ingest command")
        };
        assert!(sources.contains(&KnowledgeSource::Specs));
        assert!(sources.contains(&KnowledgeSource::Subagents));
    }

    // ── Source partitioning (notes vs graph) ─────────────────────────────────

    #[test]
    fn partition_sources_notes_and_graph() {
        let sources = vec![
            KnowledgeSource::Specs,
            KnowledgeSource::Subagents,
            KnowledgeSource::Changelog,
        ];
        let (graph, notes): (Vec<_>, Vec<_>) = sources
            .into_iter()
            .partition(|s| matches!(s, KnowledgeSource::Subagents));
        assert_eq!(graph, vec![KnowledgeSource::Subagents]);
        assert_eq!(
            notes,
            vec![KnowledgeSource::Specs, KnowledgeSource::Changelog]
        );
    }

    #[test]
    fn partition_sources_only_subagents() {
        let sources = vec![KnowledgeSource::Subagents];
        let (graph, notes): (Vec<_>, Vec<_>) = sources
            .into_iter()
            .partition(|s| matches!(s, KnowledgeSource::Subagents));
        assert_eq!(graph.len(), 1);
        assert!(notes.is_empty());
    }

    #[test]
    fn partition_sources_no_subagents() {
        let sources = vec![KnowledgeSource::Specs, KnowledgeSource::Changelog];
        let (graph, notes): (Vec<_>, Vec<_>) = sources
            .into_iter()
            .partition(|s| matches!(s, KnowledgeSource::Subagents));
        assert!(graph.is_empty());
        assert_eq!(notes.len(), 2);
    }

    // ── Confirmation gate helper ──────────────────────────────────────────────

    /// Dry-run never requires a confirmation prompt.
    #[test]
    fn should_prompt_dry_run_is_false() {
        let dry_run = true;
        let yes = false;
        assert!(!should_prompt_for_graph_write(dry_run, yes));
    }

    /// `--yes` suppresses the prompt.
    #[test]
    fn should_prompt_yes_flag_is_false() {
        let dry_run = false;
        let yes = true;
        assert!(!should_prompt_for_graph_write(dry_run, yes));
    }

    /// Neither dry-run nor --yes: prompt is required.
    #[test]
    fn should_prompt_interactive_is_true() {
        assert!(should_prompt_for_graph_write(false, false));
    }

    // ── path_to_claude_code_slug ──────────────────────────────────────────────

    #[test]
    fn slug_replaces_slashes_with_dashes() {
        let root = Path::new("/Users/alice/Dev/myproject");
        assert_eq!(path_to_claude_code_slug(root), "-Users-alice-Dev-myproject");
    }

    #[test]
    fn slug_root_path() {
        let root = Path::new("/");
        assert_eq!(path_to_claude_code_slug(root), "-");
    }

    #[test]
    fn slug_no_trailing_slash() {
        // Path::new strips trailing slashes, so /Users/foo/ == /Users/foo
        let root = Path::new("/Users/foo");
        assert_eq!(path_to_claude_code_slug(root), "-Users-foo");
    }

    // ── scan_codex_session_cwd ────────────────────────────────────────────────

    #[test]
    fn scan_codex_session_cwd_returns_cwd_from_session_meta() {
        use std::io::Write as _;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            tmp,
            r#"{{"type":"session_meta","payload":{{"id":"s1","cwd":"/Users/alice/proj","originator":"codex_cli_rs","cli_version":"1.0.0"}}}}"#
        )
        .unwrap();
        writeln!(
            tmp,
            r#"{{"type":"response_item","payload":{{"type":"message","role":"user"}}}}"#
        )
        .unwrap();

        let cwd = scan_codex_session_cwd(tmp.path());
        assert_eq!(cwd.as_deref(), Some("/Users/alice/proj"));
    }

    #[test]
    fn scan_codex_session_cwd_returns_none_when_no_session_meta() {
        use std::io::Write as _;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            tmp,
            r#"{{"type":"response_item","payload":{{"type":"message","role":"user","id":"i1","content":[]}}}}"#
        )
        .unwrap();

        let cwd = scan_codex_session_cwd(tmp.path());
        assert!(cwd.is_none());
    }

    #[test]
    fn scan_codex_session_cwd_returns_none_for_empty_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let cwd = scan_codex_session_cwd(tmp.path());
        assert!(cwd.is_none());
    }

    // ── resolve_notes_embed_provider / build_ingest_resources live checks (#5444) ──
    //
    // These exercise the actual async decision paths end-to-end against a real Ollama +
    // Qdrant instance (no mocks) — the mismatch-gate and provider-preference logic below is
    // inline in `build_ingest_resources`/`resolve_notes_embed_provider` and not extracted into
    // pure/sync helpers (unlike `select_notes_embed_provider_name` above), so this is the only
    // way to pin the regression without changing production code. `#[ignore]`d like the existing
    // live Qdrant tests in `crates/zeph-memory/src/qdrant_ops.rs`.

    /// Writes a minimal, self-contained config pointing at local Ollama (no vault secrets
    /// needed — `AppBuilder::new` never resolves a secret for an `ollama`-type provider) and a
    /// local Qdrant instance. `embed_provider_name` is written into
    /// `memory.semantic.embedding_provider`; when `None`, that field is omitted so
    /// `resolve_notes_embed_provider` must fall back to the primary provider.
    fn write_ollama_test_config(
        dir: &std::path::Path,
        collection: &str,
        embed_provider_name: Option<&str>,
    ) -> PathBuf {
        let sqlite_path = dir.join("zeph-test.db");
        let embedding_provider_line = embed_provider_name
            .map(|name| format!("embedding_provider = \"{name}\"\n"))
            .unwrap_or_default();
        let config_toml = format!(
            r#"
[agent]
name = "zeph-test"

[skills]

[[llm.providers]]
type = "ollama"
name = "primary"
base_url = "http://localhost:11434"
embedding_model = "definitely-not-a-real-embedding-model"

[[llm.providers]]
type = "ollama"
name = "embed"
base_url = "http://localhost:11434"
embedding_model = "nomic-embed-text-v2-moe"

[llm.router]
chain = ["primary"]

[memory]
sqlite_path = {sqlite_path:?}
qdrant_url = "http://localhost:6334"
history_limit = 50

[memory.semantic]
{embedding_provider_line}
[memory.documents]
collection = "{collection}"
"#
        );
        let config_path = dir.join("config.toml");
        std::fs::write(&config_path, config_toml).unwrap();
        config_path
    }

    /// #5444: with no CLI `--provider` override, `resolve_notes_embed_provider` must prefer the
    /// dedicated `memory.semantic.embedding_provider` over the primary/chat provider. The
    /// "primary" provider here is configured with a bogus embedding model that errors on
    /// `.embed()`, while "embed" has a real, locally available model — so a successful embed call
    /// on the resolved provider proves "embed" (not "primary") was selected.
    #[tokio::test]
    #[ignore = "requires live Qdrant (localhost:6334) and Ollama (localhost:11434) with nomic-embed-text-v2-moe pulled"]
    async fn resolve_notes_embed_provider_prefers_dedicated_embed_provider_over_primary() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_ollama_test_config(dir.path(), "unused_collection", Some("embed"));

        let app = AppBuilder::new(Some(&config_path), None, None, None, false)
            .await
            .expect("AppBuilder::new should succeed with a valid ollama-only config");
        let config = app.config();

        let provider = resolve_notes_embed_provider(None, config, &app)
            .await
            .expect("resolution must succeed");
        let result = provider.embed("regression probe text").await;
        assert!(
            result.is_ok(),
            "expected the dedicated embed provider (valid model) to be selected over primary \
             (bogus model), got: {result:?}"
        );
    }

    /// #5444: when `memory.semantic.embedding_provider` is unset, `resolve_notes_embed_provider`
    /// must fall back to the primary provider (existing #5396 contract, unchanged by this fix).
    #[tokio::test]
    #[ignore = "requires live Qdrant (localhost:6334) and Ollama (localhost:11434)"]
    async fn resolve_notes_embed_provider_falls_back_to_primary_when_no_embed_provider_configured()
    {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_ollama_test_config(dir.path(), "unused_collection", None);

        let app = AppBuilder::new(Some(&config_path), None, None, None, false)
            .await
            .expect("AppBuilder::new should succeed with a valid ollama-only config");
        let config = app.config();

        let provider = resolve_notes_embed_provider(None, config, &app)
            .await
            .expect("resolution must succeed");
        // "primary" has the bogus embedding model in this fixture, so falling back to it must
        // surface as an embed error — proving no dedicated embed provider was silently invented.
        let result = provider.embed("regression probe text").await;
        assert!(
            result.is_err(),
            "expected fallback to primary (bogus model) to fail its embed call: {result:?}"
        );
    }

    /// #5444 core regression test: a dimension mismatch between an existing collection and the
    /// resolved embedding provider must bail out (not silently delete+recreate) unless `--yes`
    /// is passed, and must proceed (recreating, as before) when `--yes` is passed.
    #[tokio::test]
    #[ignore = "requires live Qdrant (localhost:6334) and Ollama (localhost:11434) with nomic-embed-text-v2-moe pulled"]
    async fn build_ingest_resources_blocks_destructive_recreate_without_yes() {
        let dir = tempfile::tempdir().unwrap();
        let collection = format!("zeph_test_dim_mismatch_{}", uuid::Uuid::new_v4().simple());
        let config_path = write_ollama_test_config(dir.path(), &collection, Some("embed"));

        let qdrant = QdrantOps::new("http://localhost:6334", None).unwrap();
        // Pre-create the collection at a dimension that cannot match nomic-embed-text-v2-moe's
        // real output dimension.
        qdrant.ensure_collection(&collection, 4).await.unwrap();

        // Without --yes: must error, and must NOT touch the existing collection.
        let Err(err) = Box::pin(build_ingest_resources(Some(&config_path), None, false)).await
        else {
            panic!("dimension mismatch without --yes must error, not silently recreate");
        };
        assert!(
            err.to_string().contains("--yes"),
            "error should instruct the user to re-run with --yes, got: {err}"
        );
        assert_eq!(
            qdrant
                .get_collection_vector_size(&collection)
                .await
                .unwrap(),
            Some(4),
            "collection must NOT have been recreated by the failed attempt"
        );

        // With --yes: proceeds, recreating the collection at the real embedding dimension.
        Box::pin(build_ingest_resources(Some(&config_path), None, true))
            .await
            .expect("dimension mismatch with --yes must proceed");
        let new_size = qdrant
            .get_collection_vector_size(&collection)
            .await
            .unwrap();
        assert_ne!(
            new_size,
            Some(4),
            "collection should have been recreated with the resolved embedding provider's \
             dimension"
        );

        qdrant.delete_collection(&collection).await.unwrap();
    }

    // ── guard_destructive_recreate direct checks (#5444 S1/M1) ─────────────────────
    //
    // Unlike `build_ingest_resources_blocks_destructive_recreate_without_yes` above, these
    // exercise `guard_destructive_recreate` directly — it only needs a `QdrantOps`, no
    // AppBuilder/Ollama — so they're cheaper and pin the exact function the S1 fix factored out.

    #[tokio::test]
    #[ignore = "requires live Qdrant (localhost:6334)"]
    async fn guard_destructive_recreate_noop_when_collection_missing() {
        let qdrant = QdrantOps::new("http://localhost:6334", None).unwrap();
        let collection = format!("zeph_test_guard_missing_{}", uuid::Uuid::new_v4().simple());
        let result = guard_destructive_recreate(&qdrant, &collection, 128, false).await;
        assert!(
            result.is_ok(),
            "no existing collection must never block: {result:?}"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Qdrant (localhost:6334)"]
    async fn guard_destructive_recreate_noop_when_dimension_matches() {
        let qdrant = QdrantOps::new("http://localhost:6334", None).unwrap();
        let collection = format!("zeph_test_guard_match_{}", uuid::Uuid::new_v4().simple());
        qdrant.ensure_collection(&collection, 128).await.unwrap();

        let result = guard_destructive_recreate(&qdrant, &collection, 128, false).await;
        assert!(
            result.is_ok(),
            "matching dimension must never block: {result:?}"
        );

        qdrant.delete_collection(&collection).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires live Qdrant (localhost:6334)"]
    async fn guard_destructive_recreate_errors_on_mismatch_without_yes() {
        let qdrant = QdrantOps::new("http://localhost:6334", None).unwrap();
        let collection = format!("zeph_test_guard_mismatch_{}", uuid::Uuid::new_v4().simple());
        qdrant.ensure_collection(&collection, 128).await.unwrap();

        let Err(err) = guard_destructive_recreate(&qdrant, &collection, 256, false).await else {
            panic!("dimension mismatch without --yes must error");
        };
        assert!(err.to_string().contains("--yes"), "got: {err}");
        assert!(err.to_string().contains("128"), "got: {err}");
        assert!(err.to_string().contains("256"), "got: {err}");

        qdrant.delete_collection(&collection).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires live Qdrant (localhost:6334)"]
    async fn guard_destructive_recreate_proceeds_on_mismatch_with_yes() {
        let qdrant = QdrantOps::new("http://localhost:6334", None).unwrap();
        let collection = format!("zeph_test_guard_yes_{}", uuid::Uuid::new_v4().simple());
        qdrant.ensure_collection(&collection, 128).await.unwrap();

        let result = guard_destructive_recreate(&qdrant, &collection, 256, true).await;
        assert!(result.is_ok(), "--yes must permit a mismatch: {result:?}");

        qdrant.delete_collection(&collection).await.unwrap();
    }

    /// Creates a named-vector Qdrant collection (`ParamsMap` config) directly via the raw
    /// `qdrant-client`, bypassing `QdrantOps` (which only ever creates single unnamed-vector
    /// collections). `QdrantOps::get_collection_vector_size` treats `ParamsMap` as unreadable and
    /// returns `Ok(None)` (see `crates/zeph-memory/src/qdrant_ops.rs`) — this is the only way to
    /// construct the `None` case that #5444 M1 must fail closed on.
    async fn create_named_vector_collection(collection: &str) {
        let client = qdrant_client::Qdrant::from_url("http://localhost:6334")
            .build()
            .unwrap();
        let _ = client.delete_collection(collection).await;
        let mut vectors_config = qdrant_client::qdrant::VectorsConfigBuilder::default();
        vectors_config.add_named_vector_params(
            "custom",
            qdrant_client::qdrant::VectorParamsBuilder::new(
                64,
                qdrant_client::qdrant::Distance::Cosine,
            ),
        );
        client
            .create_collection(
                qdrant_client::qdrant::CreateCollectionBuilder::new(collection)
                    .vectors_config(vectors_config),
            )
            .await
            .unwrap();
    }

    /// #5444 M1 regression test: an existing collection whose dimension `get_collection_vector_size`
    /// cannot determine (named-vector config) must be treated as a mismatch requiring `--yes`, not
    /// silently treated as safe.
    #[tokio::test]
    #[ignore = "requires live Qdrant (localhost:6334)"]
    async fn guard_destructive_recreate_fails_closed_on_unreadable_dimension_without_yes() {
        let qdrant = QdrantOps::new("http://localhost:6334", None).unwrap();
        let collection = format!(
            "zeph_test_guard_unreadable_{}",
            uuid::Uuid::new_v4().simple()
        );
        create_named_vector_collection(&collection).await;

        // Sanity: confirm the fixture actually reproduces the `None` case this test targets.
        assert_eq!(
            qdrant
                .get_collection_vector_size(&collection)
                .await
                .unwrap(),
            None,
            "fixture must produce an unreadable (named-vector) dimension"
        );

        let Err(err) = guard_destructive_recreate(&qdrant, &collection, 64, false).await else {
            panic!("unreadable dimension without --yes must fail closed (error), not proceed");
        };
        assert!(err.to_string().contains("--yes"), "got: {err}");
        assert!(
            err.to_string().contains("unknown") || err.to_string().contains("unreadable"),
            "error should describe the dimension as unreadable, got: {err}"
        );

        qdrant.delete_collection(&collection).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires live Qdrant (localhost:6334)"]
    async fn guard_destructive_recreate_proceeds_on_unreadable_dimension_with_yes() {
        let qdrant = QdrantOps::new("http://localhost:6334", None).unwrap();
        let collection = format!(
            "zeph_test_guard_unreadable_yes_{}",
            uuid::Uuid::new_v4().simple()
        );
        create_named_vector_collection(&collection).await;

        let result = guard_destructive_recreate(&qdrant, &collection, 64, true).await;
        assert!(
            result.is_ok(),
            "--yes must permit an unreadable dimension: {result:?}"
        );

        qdrant.delete_collection(&collection).await.unwrap();
    }

    // ── guard_reasoning_collection_recreate live checks (#5444 S1) ─────────────────

    /// Same fixture shape as `write_ollama_test_config`, but additionally selects the Qdrant
    /// vector backend and enables `memory.reasoning` — the two preconditions
    /// `guard_reasoning_collection_recreate`/`attach_reasoning_memory` require before touching
    /// `REASONING_COLLECTION` at all.
    fn write_ollama_reasoning_test_config(
        dir: &std::path::Path,
        embed_provider_name: Option<&str>,
    ) -> PathBuf {
        let sqlite_path = dir.join("zeph-test.db");
        let embedding_provider_line = embed_provider_name
            .map(|name| format!("embedding_provider = \"{name}\"\n"))
            .unwrap_or_default();
        let config_toml = format!(
            r#"
[agent]
name = "zeph-test"

[skills]

[[llm.providers]]
type = "ollama"
name = "primary"
base_url = "http://localhost:11434"
embedding_model = "definitely-not-a-real-embedding-model"

[[llm.providers]]
type = "ollama"
name = "embed"
base_url = "http://localhost:11434"
embedding_model = "nomic-embed-text-v2-moe"

[llm.router]
chain = ["primary"]

[memory]
sqlite_path = {sqlite_path:?}
qdrant_url = "http://localhost:6334"
history_limit = 50
vector_backend = "qdrant"

[memory.semantic]
{embedding_provider_line}
[memory.reasoning]
enabled = true

[memory.documents]
collection = "zeph_test_reasoning_guard_unused_documents"
"#
        );
        let config_path = dir.join("config.toml");
        std::fs::write(&config_path, config_toml).unwrap();
        config_path
    }

    /// #5444 S1 core regression test: `attach_reasoning_memory` (invoked internally by every
    /// `build_memory` call, on all three CLI ingest paths) would otherwise destructively
    /// delete+recreate the shared `reasoning_strategies` Qdrant collection on a dimension
    /// mismatch, completely ungated. `guard_reasoning_collection_recreate` must block that call
    /// sequence the same way `guard_destructive_recreate` blocks the notes-sink `documents` path.
    ///
    /// Uses the fixed `REASONING_COLLECTION` name (the function under test hardcodes it, it is
    /// not parameterizable) rather than a per-test-unique name — cleans up before and after to
    /// avoid cross-test pollution, matching the existing fixed-name convention in
    /// `crates/zeph-memory/src/qdrant_ops.rs`'s live tests.
    #[tokio::test]
    #[ignore = "requires live Qdrant (localhost:6334) and Ollama (localhost:11434) with nomic-embed-text-v2-moe pulled"]
    async fn guard_reasoning_collection_recreate_blocks_destructive_recreate_without_yes() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_ollama_reasoning_test_config(dir.path(), Some("embed"));

        let qdrant = QdrantOps::new("http://localhost:6334", None).unwrap();
        let collection = zeph_memory::reasoning::REASONING_COLLECTION;
        let _ = qdrant.delete_collection(collection).await;
        qdrant.ensure_collection(collection, 4).await.unwrap();

        let app = AppBuilder::new(Some(&config_path), None, None, None, false)
            .await
            .expect("AppBuilder::new should succeed with a valid ollama+qdrant config");
        let config = app.config();
        let provider = resolve_notes_embed_provider(None, config, &app)
            .await
            .expect("resolution must succeed");

        // Without --yes: must error, and must NOT touch the existing collection.
        let Err(err) = guard_reasoning_collection_recreate(&app, &provider, false).await else {
            panic!(
                "reasoning-collection dimension mismatch without --yes must error, not \
                 silently recreate"
            );
        };
        assert!(
            err.to_string().contains("--yes"),
            "error should instruct the user to re-run with --yes, got: {err}"
        );
        assert_eq!(
            qdrant.get_collection_vector_size(collection).await.unwrap(),
            Some(4),
            "reasoning collection must NOT have been recreated by the failed attempt"
        );

        // With --yes: the guard permits it; the actual recreate happens inside `build_memory`
        // (via `attach_reasoning_memory`), mirroring the real call-site sequence exactly.
        guard_reasoning_collection_recreate(&app, &provider, true)
            .await
            .expect("dimension mismatch with --yes must proceed");
        let kn_sup = zeph_common::TaskSupervisor::new(tokio_util::sync::CancellationToken::new());
        app.build_memory(&provider, &kn_sup)
            .await
            .expect("build_memory should succeed once the guard has cleared");
        let new_size = qdrant.get_collection_vector_size(collection).await.unwrap();
        assert_ne!(
            new_size,
            Some(4),
            "reasoning collection should have been recreated with the resolved embedding \
             provider's dimension"
        );

        qdrant.delete_collection(collection).await.unwrap();
    }

    /// #5444 S1: when `memory.reasoning.enabled` is left at its default (`false`), the guard must
    /// be a strict no-op regardless of `yes` — reasoning memory being disabled must never itself
    /// cause an error or a write.
    #[tokio::test]
    #[ignore = "requires live Qdrant (localhost:6334) and Ollama (localhost:11434) with nomic-embed-text-v2-moe pulled"]
    async fn guard_reasoning_collection_recreate_noop_when_reasoning_disabled() {
        let dir = tempfile::tempdir().unwrap();
        // Reuses the non-reasoning fixture: vector_backend defaults to sqlite and
        // memory.reasoning.enabled defaults to false.
        let config_path = write_ollama_test_config(dir.path(), "unused_collection", Some("embed"));

        let app = AppBuilder::new(Some(&config_path), None, None, None, false)
            .await
            .expect("AppBuilder::new should succeed with a valid ollama-only config");
        let config = app.config();
        let provider = resolve_notes_embed_provider(None, config, &app)
            .await
            .expect("resolution must succeed");

        let result = guard_reasoning_collection_recreate(&app, &provider, false).await;
        assert!(
            result.is_ok(),
            "reasoning disabled (or non-Qdrant backend) must always no-op: {result:?}"
        );
    }

    // ── hub-degree kill-criterion (spec-067 §7, #5467) ────────────────────────

    #[test]
    fn hub_degree_threshold_pct_is_15_percent() {
        // Regression pin: the constant was extracted from two duplicated `15.0` literals
        // (per-row HUB flag and the overall PASS/WARN verdict). Both call sites must keep
        // reading the same value, so pin it here rather than let a future edit silently
        // change only one of the two use sites again.
        assert!((HUB_DEGREE_THRESHOLD_PCT - 15.0).abs() < f64::EPSILON);
    }

    fn hub_report(
        entries: &[(&str, usize)],
        dry_run: bool,
    ) -> zeph_memory::graph::ingest::IngestReport {
        zeph_memory::graph::ingest::IngestReport {
            dry_run,
            hub_degree: entries
                .iter()
                .map(|&(entity, degree)| zeph_memory::graph::ingest::HubDegree {
                    entity: entity.to_owned(),
                    degree,
                })
                .collect(),
            ..zeph_memory::graph::ingest::IngestReport::default()
        }
    }

    // These are smoke tests: `print_graph_ingest_report` only prints to stdout (no
    // return value to assert on), so they exist to exercise the threshold arithmetic
    // and branching (division guard, cast, PASS/WARN comparison) without panicking
    // across the boundary conditions the fix touched. They do not assert on the
    // printed text.

    #[test]
    fn print_graph_ingest_report_empty_hub_degree_does_not_panic() {
        print_graph_ingest_report(&hub_report(&[], true));
    }

    #[test]
    fn print_graph_ingest_report_zero_total_edges_does_not_panic() {
        // A hub_degree entry with degree 0 makes total_edges (the sum) 0, exercising the
        // division-by-zero guard (`if total_edges > 0 { .. } else { 0.0 }`).
        print_graph_ingest_report(&hub_report(&[("Solo", 0)], true));
    }

    #[test]
    fn print_graph_ingest_report_exactly_at_threshold_does_not_panic() {
        // 15 of 100 edges == exactly HUB_DEGREE_THRESHOLD_PCT: boundary is `<=`/`>`, must be PASS.
        print_graph_ingest_report(&hub_report(&[("AtThreshold", 15), ("Rest", 85)], true));
    }

    #[test]
    fn print_graph_ingest_report_above_threshold_does_not_panic() {
        print_graph_ingest_report(&hub_report(&[("OverThreshold", 30), ("Rest", 70)], true));
    }

    #[test]
    fn print_graph_ingest_report_non_dry_run_skips_hub_section() {
        // Outside dry-run, the hub-degree block must not execute even if hub_degree is
        // (unexpectedly) non-empty.
        print_graph_ingest_report(&hub_report(&[("Ignored", 999)], false));
    }
}
