# zeph-index

AST-based code indexing, semantic retrieval, and repo map generation.

## Overview

Parses source files with tree-sitter to extract symbols, chunks them for embedding, and stores vectors in Qdrant for semantic code search. Generates concise repo maps that can be injected into the agent context. Feature-gated behind `index`.

## Key Modules

- **indexer** — orchestrates file discovery, parsing, and embedding pipeline
- **retriever** — semantic search over indexed symbols and chunks
- **store** — persistence layer backed by Qdrant
- **repo_map** — generates tree-style repository summaries
- **watcher** — filesystem watcher for incremental re-indexing
- **error** — `IndexError` error types

## Usage

```toml
# Cargo.toml (workspace root)
zeph-index = { path = "crates/zeph-index" }
```

Enabled via the `index` feature flag on the root `zeph` crate.

## License

MIT
