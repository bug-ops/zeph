# zeph-index

[![Crates.io](https://img.shields.io/crates/v/zeph-index)](https://crates.io/crates/zeph-index)
[![docs.rs](https://img.shields.io/docsrs/zeph-index)](https://docs.rs/zeph-index)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)

AST-based code indexing and semantic retrieval for Zeph.

## Overview

Parses source files with tree-sitter to extract symbols, chunks them for embedding, and stores vectors in Qdrant for semantic code search. Generates concise repo maps that can be injected into the agent context. Feature-gated behind `index`.

## Key Modules

- **indexer** — orchestrates file discovery, parsing, and embedding pipeline
- **retriever** — semantic search over indexed symbols and chunks
- **store** — persistence layer backed by Qdrant
- **repo_map** — generates tree-style repository summaries
- **watcher** — filesystem watcher for incremental re-indexing
- **error** — `IndexError` error types

## Installation

```bash
cargo add zeph-index
```

Enabled via the `index` feature flag on the root `zeph` crate.

## License

MIT
