# Spec: Code Index

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

- Runs in `tokio::spawn` at startup — **never blocks agent loop**
- Progress broadcasted via `tokio::watch` channel: `IndexProgress { files_done, files_total, chunks_created }`
- TUI status bar shows: `Indexing repository… (N/M files)` with spinner during indexing
- File watcher (`notify` crate) triggers incremental re-index on file changes

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
