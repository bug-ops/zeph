# Code Indexing

AST-based code indexing and semantic retrieval for project-aware context. The `zeph-index` crate parses source files via tree-sitter, chunks them by AST structure, embeds the chunks in Qdrant, and retrieves relevant code via hybrid search (semantic + grep routing) for injection into the agent context window.

Disabled by default. Enable via `[index] enabled = true` in config.

## Why Code RAG

Cloud models with 200K token windows can afford multi-round agentic grep. Local models with 8K-32K windows cannot: a single grep cycle costs ~2K tokens (25% of an 8K budget), while 5 rounds would exceed the entire context. RAG retrieves 6-8 relevant chunks in ~3K tokens, preserving budget for history and response.

For cloud models, code RAG serves as pre-fill context alongside agentic search. For local models, it is the primary code retrieval mechanism.

## Setup

1. **Start Qdrant** (required for vector storage):

   ```bash
   docker compose up -d qdrant
   ```

2. **Enable indexing in config:**

   ```toml
   [index]
   enabled = true
   ```

3. **Index your project:**

   ```bash
   zeph index
   ```

   Or let auto-indexing handle it on startup when `auto_index = true` (default).

## Architecture

The `zeph-index` crate contains 7 modules:

| Module | Purpose |
|--------|---------|
| `languages` | Language detection from file extensions, tree-sitter grammar registry |
| `chunker` | AST-based chunking with greedy sibling merge (cAST-inspired algorithm) |
| `context` | Contextualized embedding text generation (file path + scope + imports + code) |
| `store` | Dual-write storage: Qdrant vectors + SQLite chunk metadata |
| `indexer` | Orchestrator: walk project tree, chunk files, embed, store with incremental change detection |
| `retriever` | Query classification, semantic search, budget-aware chunk packing |
| `repo_map` | Compact structural map of the project (signatures only, no function bodies) |

### Pipeline

```text
Source files
    |
    v
[languages.rs] detect language, load grammar
    |
    v
[chunker.rs] parse AST, split into chunks (target: ~600 non-ws chars)
    |
    v
[context.rs] prepend file path, scope chain, imports, language tag
    |
    v
[indexer.rs] embed via LlmProvider, skip unchanged (content hash)
    |
    v
[store.rs] upsert to Qdrant (vectors) + SQLite (metadata)
```

### Retrieval

```text
User query
    |
    v
[retriever.rs] classify_query()
    |
    +--> Semantic  --> embed query --> Qdrant search --> budget pack --> inject
    |
    +--> Grep      --> return empty (agent uses bash tools)
    |
    +--> Hybrid    --> semantic search + hint to agent
```

## Query Classification

The retriever classifies each query to route it to the appropriate search strategy:

| Strategy | Trigger | Action |
|----------|---------|--------|
| **Grep** | Exact symbols: `::`, `fn `, `struct `, CamelCase, snake_case identifiers | Agent handles via shell grep/ripgrep |
| **Semantic** | Conceptual queries: "how", "where", "why", "explain" | Vector similarity search in Qdrant |
| **Hybrid** | Both symbol patterns and conceptual words | Semantic search + hint that grep may also help |

Default (no pattern match): Semantic.

## AST-Based Chunking

Files are parsed via tree-sitter into AST, then chunked by entity boundaries (functions, structs, classes, impl blocks). The algorithm uses greedy sibling merge:

- **Target size:** 600 non-whitespace characters (~300-400 tokens)
- **Max size:** 1200 non-ws chars (forced recursive split)
- **Min size:** 100 non-ws chars (merge with adjacent sibling)

Config files (TOML, JSON, Markdown, Bash) are indexed as single file-level chunks since they lack named entities.

Each chunk carries rich metadata: file path, language, AST node type, entity name, line range, scope chain (e.g. `MyStruct > impl MyStruct > my_method`), imports, and a BLAKE3 content hash for change detection.

## Contextualized Embeddings

Embedding raw code alone yields poor retrieval quality for conceptual queries. Before embedding, each chunk is prepended with:

- File path (`# src/agent.rs`)
- Scope chain (`# Scope: Agent > prepare_context`)
- Language tag (`# Language: rust`)
- First 5 import/use statements

This contextualized form improves retrieval for queries like "where is auth handled?" where the code alone might not contain the word "auth".

## Storage

Chunks are dual-written to two stores:

| Store | Data | Purpose |
|-------|------|---------|
| Qdrant (`zeph_code_chunks`) | Embedding vectors + payload (code, metadata) | Semantic similarity search |
| SQLite (`chunk_metadata`) | File path, content hash, line range, language, node type | Change detection, cleanup of deleted files |

The Qdrant collection uses INT8 scalar quantization for ~4x memory reduction with minimal accuracy loss. Payload indexes on `language`, `file_path`, and `node_type` enable filtered search.

## Incremental Indexing

On subsequent runs, the indexer skips unchanged chunks by checking BLAKE3 content hashes in SQLite. Only modified or new files are re-embedded. Deleted files are detected by comparing the current file set against the SQLite index, and their chunks are removed from both stores.

## File Watcher

When `watch = true` (default), an `IndexWatcher` monitors project files for changes during the session. On file modification, the changed file is automatically re-indexed via `reindex_file()` without rebuilding the entire index. The watcher uses 1-second debounce to batch rapid changes and only processes files with indexable extensions.

Disable with:

```toml
[index]
watch = false
```

## Repo Map

A lightweight structural map of the project, generated via tree-sitter signature extraction (no function bodies). Included in the system prompt and cached with a configurable TTL (default: 5 minutes) to avoid per-message filesystem traversal.

Example output:

```text
<repo_map>
  src/agent.rs :: struct:Agent, impl:Agent, fn:new, fn:run, fn:prepare_context
  src/config.rs :: struct:Config, fn:load
  src/main.rs :: fn:main, fn:setup_logging
  ... and 12 more files
</repo_map>
```

The map is budget-constrained (default: 1024 tokens) and sorted by symbol count (files with more symbols appear first). It gives the model a structural overview of the project without consuming significant context.

## Budget-Aware Retrieval

Retrieved chunks are packed into a token budget (default: 40% of available context for code). Chunks are sorted by similarity score and greedily packed until the budget is exhausted. A minimum score threshold (default: 0.25) filters low-relevance results.

Retrieved code is injected as a transient `<code_context>` XML block before the conversation history. It is re-generated on every turn and never persisted.

## Context Window Layout (with Code RAG)

When code indexing is enabled, the context window includes two additional sections:

```text
+---------------------------------------------------+
| System prompt + environment + ZEPH.md             |
+---------------------------------------------------+
| <repo_map> (structural overview, cached)          |  <= 1024 tokens
+---------------------------------------------------+
| <available_skills>                                |
+---------------------------------------------------+
| <code_context> (per-query RAG chunks, transient)  |  <= 30% available
+---------------------------------------------------+
| [semantic recall] past messages                   |  <= 10% available
+---------------------------------------------------+
| Recent message history                            |  <= 50% available
+---------------------------------------------------+
| [response reserve]                                |  20% of total
+---------------------------------------------------+
```

## Configuration

```toml
[index]
# Enable codebase indexing for semantic code search.
# Requires Qdrant running (uses separate collection "zeph_code_chunks").
enabled = false

# Auto-index on startup and re-index changed files during session.
auto_index = true

# Directories to index (relative to cwd).
paths = ["."]

# Patterns to exclude (in addition to .gitignore).
exclude = ["target", "node_modules", ".git", "vendor", "dist", "build", "__pycache__"]

# Token budget for repo map in system prompt (0 = no repo map).
repo_map_budget = 1024

# Cache TTL for repo map in seconds (avoids per-message regeneration).
repo_map_ttl_secs = 300

[index.chunker]
# Target chunk size in non-whitespace characters (~300-400 tokens).
target_size = 600
# Maximum chunk size before forced split.
max_size = 1200
# Minimum chunk size — smaller chunks merge with siblings.
min_size = 100

[index.retrieval]
# Maximum chunks to fetch from Qdrant (before budget packing).
max_chunks = 12
# Minimum cosine similarity score to accept.
score_threshold = 0.25
# Maximum fraction of available context budget for code chunks.
budget_ratio = 0.40
```

## Supported Languages

Language support is controlled by feature flags on the `zeph-index` crate. All default features are enabled when the `index` binary feature is active.

| Language | Feature | Extensions |
|----------|---------|------------|
| Rust | `lang-rust` | `.rs` |
| Python | `lang-python` | `.py`, `.pyi` |
| JavaScript | `lang-js` | `.js`, `.jsx`, `.mjs`, `.cjs` |
| TypeScript | `lang-js` | `.ts`, `.tsx`, `.mts`, `.cts` |
| Go | `lang-go` | `.go` |
| Bash | `lang-config` | `.sh`, `.bash`, `.zsh` |
| TOML | `lang-config` | `.toml` |
| JSON | `lang-config` | `.json`, `.jsonc` |
| Markdown | `lang-config` | `.md`, `.markdown` |

## Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `ZEPH_INDEX_ENABLED` | Enable code indexing | `false` |
| `ZEPH_INDEX_AUTO_INDEX` | Auto-index on startup | `true` |
| `ZEPH_INDEX_REPO_MAP_BUDGET` | Token budget for repo map | `1024` |
| `ZEPH_INDEX_REPO_MAP_TTL_SECS` | Cache TTL for repo map in seconds | `300` |

## Embedding Model Recommendations

The indexer uses the same `LlmProvider.embed()` as semantic memory. Any embedding model works. For code-heavy workloads:

| Model | Dims | Notes |
|-------|------|-------|
| `qwen3-embedding` | 1024 | Current Zeph default, good general performance |
| `nomic-embed-text` | 768 | Lightweight universal model |
| `nomic-embed-code` | 768 | Optimized for code, higher RAM (~7.5GB) |
