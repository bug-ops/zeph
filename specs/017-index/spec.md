---
aliases:
  - Code Index
  - Code Indexing
  - Semantic Retrieval
tags:
  - sdd
  - spec
  - index
  - code
created: 2026-04-08
status: approved
related:
  - "[[MOC-specs]]"
  - "[[004-memory/spec]]"
---

# Spec: Code Index

> [!info]
> AST-based code indexing, semantic retrieval, repo map generation;
> enables code-aware context injection in [[004-memory/spec|Memory Pipeline]].

## Sources

### Internal
| File | Contents |
|---|---|
| `crates/zeph-index/src/indexer.rs` | `CodeIndexer`, pipeline orchestration |
| `crates/zeph-index/src/store.rs` | `CodeStore`, Qdrant + SQLite |
| `crates/zeph-index/src/chunker.rs` | AST-aware chunking (tree-sitter) |
| `crates/zeph-index/src/retriever.rs` | Semantic + BM25 hybrid retrieval |
| `crates/zeph-index/src/repo_map.rs` | Repo map generation |
| `crates/zeph-index/src/languages.rs` | Extension → language mapping |
| `crates/zeph-index/src/watcher.rs` | File watcher, incremental re-index |
| `crates/zeph-index/src/context.rs` | `IndexProgress`, watch channel |

---

`crates/zeph-index/` (feature: `index`) — AST-based code indexing, semantic retrieval, repo map.

## Indexing Pipeline

```
CodeIndexer
├── FileWalker: ignore::WalkBuilder — .hidden(true), .git_ignore(true), follows symlinks
├── Language detection: extension-based (languages::is_indexable())
├── AstParser: tree-sitter per language → functions/classes/methods/structs/enums/traits
├── ChunkSplitter: AST-aware (never splits token streams; chunk boundaries = AST boundaries)
├── EmbeddingStore: Qdrant collection per project root (keyed by root path hash)
└── RepoMap: symbol signatures only (no bodies)
```

## Supported Languages

Rust, Python, JavaScript, TypeScript, Go, Bash, JSON, TOML, Markdown

## Symbol / Chunk Storage

```
CodeSymbol {
    path: PathBuf,
    name: String,
    kind: Fn | Struct | Enum | Trait | Type | Const | Module,
    signature: String,   // one-line, always present
    body: Option<String>, // full body, NOT stored in Qdrant — only in SQLite
    line_range: (usize, usize),
    language: Language,
}
```

- **Vector size** consistent across all chunks in collection — verified at init with probe embedding
- **Bodies not in Qdrant** — embeddings + metadata only; bodies fetched via `load_symbol` tool

## One Collection Per Project

- Collection name = hash of project root path — isolates codebases from each other
- Incremental update on file change:
  1. Compare current file set vs stored
  2. Skip unchanged files (hash check)
  3. Re-index changed files
  4. Remove chunks for deleted files
- Schema version mismatch → full rebuild

## Semantic Retrieval

1. Embed query text (same model as memory embeddings)
2. Qdrant ANN search in project collection (cosine similarity)
3. Return top-k `CodeSymbol` records (signatures, no bodies)
4. Injected as `MessagePart::CodeContext` (2nd recall source)

## Background Indexing

- Runs via `TaskSupervisor::spawn_restartable` (or plain `tokio::spawn` if supervisor absent) — **never blocks agent loop**
- Progress broadcasted via `tokio::watch` channel: `IndexProgress { files_done, files_total, chunks_created }`
- TUI status bar shows: `Indexing repository… (N/M files)` with spinner during indexing
- File watcher (`notify` crate) triggers incremental re-index on file changes

### File Watcher Debounce

FS events are collected in a **500 ms debounce window** (max 5 s cap). Each changed path is reindexed at most once per window. This prevents CPU saturation during git operations or editor saves that generate rapid bursts of events.

### Qdrant Upsert Timeout

`upsert_chunks_batch` is wrapped with a **30-second `tokio::time::timeout`**. On expiry the batch is skipped with a `WARN` log and indexing continues. This prevents indefinite stalls when Qdrant is slow or unavailable.

### Re-entry Guard

`CodeIndexer` tracks an `AtomicBool` flag. A second concurrent call to `index_project` returns `Ok(IndexReport::default())` immediately with an `INFO` log rather than running a redundant full-index pass.

### BlockingSpawner Integration

`CodeIndexer` accepts `Option<Arc<dyn BlockingSpawner>>` via a `with_spawner()` builder method. When a spawner is present, each `chunk_file` invocation is dispatched as a named blocking task (`chunk_file_{N}`, unique AtomicU64 counter) so concurrent indexing passes are fully visible in `TaskSupervisor::list_tasks()`. When absent, chunking runs inline. See [[039-background-task-supervisor/spec]] and [[043-zeph-common/spec]] for the `BlockingSpawner` trait.

### IndexerConfig Safe Defaults

Default values are tuned for developer machines to prevent resource saturation:

| Field | Old Default | New Default |
|-------|------------|-------------|
| `memory_batch_size` | 32 | 16 |
| `embed_concurrency` | 2 | 1 |
| `concurrency` | 4 | 2 |
| `batch_size` (Qdrant) | 32 | 16 |

All values remain user-configurable via `[index]` config section.

## Repo Map

Lightweight symbol map for context injection:

```
src/main.rs
  fn main() → Result<()>
crates/zeph-core/src/agent/mod.rs
  struct Agent<C: Channel>
  fn run(&mut self) → Result<(), AgentError>
```

- Signatures only, no bodies
- Size bounded by `max_repo_map_tokens` config
- Generated on demand, not pre-computed

## Key Invariants

- Indexing always runs in background — never block agent loop
- `.gitignore` and symlink rules from `ignore::WalkBuilder` are non-negotiable
- Vector size must be consistent within a collection — probe embedding at init
- Bodies are NOT stored in Qdrant — only embeddings and metadata
- `CodeContext` is the 2nd recall source (after semantic, before graph)
- TUI must show indexing progress with spinner while background indexing runs
- Invalid syntax files → log error in `IndexReport.errors`, skip — never panic
- One collection per project root hash — cross-project mixing is forbidden
- File watcher debounce window is 500 ms — reindex each path at most once per window
- Qdrant upsert has a 30s timeout — on expiry, skip batch with WARN, never stall indefinitely
- `index_project` re-entry is guarded by `AtomicBool` — second concurrent call returns immediately
- Task names are `Arc<str>` — never `&'static str` or `Box::leak` in `BlockingSpawner` calls
