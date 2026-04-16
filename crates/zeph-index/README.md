# zeph-index

[![Crates.io](https://img.shields.io/crates/v/zeph-index)](https://crates.io/crates/zeph-index)
[![docs.rs](https://img.shields.io/docsrs/zeph-index)](https://docs.rs/zeph-index)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.94-blue)](https://www.rust-lang.org)

AST-based code indexing and semantic retrieval for Zeph. Always-on — no feature flag required.

## Overview

Parses source files with tree-sitter to extract structured symbols (name, kind, visibility, line) via ts-query grammars, chunks them for embedding, and stores vectors in Qdrant for semantic code search. Supports Rust, Python, JavaScript, TypeScript, and Go. Generates concise repo maps that are injected into the agent context unconditionally across all LLM providers.

## Key Modules

- **indexer** — orchestrates file discovery, parsing, and embedding pipeline
- **retriever** — semantic search over indexed symbols and chunks
- **store** — persistence layer; vector operations go through the `VectorStore` trait from `zeph-memory` (backed by Qdrant)
- **repo_map** — generates tree-style repository summaries using tree-sitter ts-query symbol extraction; injected into all LLM providers regardless of Qdrant availability
- **lsp** — hover pre-filter using tree-sitter for multi-language symbol identification (Rust, Python, JS, TS, Go)
- **watcher** — filesystem watcher for incremental re-indexing
- **error** — `IndexError` error types

## Supported languages

| Language | Symbol extraction | Hover pre-filter |
|----------|------------------|-----------------|
| Rust | functions, structs, enums, traits, impls | yes |
| Python | functions, classes, methods | yes |
| JavaScript | functions, classes, arrow functions | yes |
| TypeScript | functions, classes, interfaces, types | yes |
| Go | functions, structs, interfaces | yes |

## Installation

```bash
cargo add zeph-index
```

> [!NOTE]
> `zeph-index` does not depend on `qdrant-client` directly. Vector storage is delegated to `zeph-memory`, which owns the Qdrant client lifecycle. Repo map generation works without Qdrant — it is injected into the agent context for all LLM providers unconditionally.

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

MIT
