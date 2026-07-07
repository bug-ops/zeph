# zeph-index

[![Crates.io](https://img.shields.io/crates/v/zeph-index)](https://crates.io/crates/zeph-index)
[![docs.rs](https://img.shields.io/docsrs/zeph-index)](https://docs.rs/zeph-index)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.96-blue)](https://www.rust-lang.org)

AST-based code indexing, semantic retrieval, and repo map generation for Zeph.

## Overview

Implements the **Code RAG** pipeline that grounds the agent in a local codebase. Source files are parsed with tree-sitter into AST-level chunks (functions, structs, impl blocks) rather than fixed-size windows, embedded via the configured LLM provider, and written to a dual store: Qdrant for vector similarity and SQLite for exact hash deduplication. Retrieval classifies each query as *semantic*, *grep*, or *hybrid*, searches Qdrant, applies a score threshold, and packs results within a token budget. Concise repo maps are injected into the agent context unconditionally across all LLM providers, even when Qdrant is unavailable.

## Key Modules

- **chunker** — tree-sitter AST-level chunking of source files
- **indexer** — orchestrates file discovery, parsing, and the embedding pipeline
- **retriever** — semantic / grep / hybrid search over indexed chunks with token budgeting
- **store** — Qdrant + SQLite dual-write store (`CodeStore`)
- **repo_map** — tree-style repository summaries (file paths + symbol signatures) injected into all LLM providers
- **mcp_server** — in-process MCP server (`IndexMcpServer`) exposing `symbol_definition`, `find_text_references`, `call_graph`, and `module_summary` tools
- **languages** — language detection and tree-sitter grammar registry (`Lang`)
- **context** — retrieval-context assembly helpers
- **watcher** — filesystem watcher for incremental re-indexing
- **error** — `IndexError` error type

## Supported languages

Full symbol extraction (functions, types, impls) drives repo map and MCP tools; the remaining grammars are used for AST-level chunking only.

| Language | Symbol extraction | Chunking |
|----------|-------------------|----------|
| Rust | functions, structs, enums, traits, impls | yes |
| Python | functions, classes, methods | yes |
| JavaScript | functions, classes, arrow functions | yes |
| TypeScript | functions, classes, interfaces, types | yes |
| Go | functions, structs, interfaces | yes |
| Bash, TOML, JSON, Markdown | — | yes |

## Installation

```bash
cargo add zeph-index
```

## Feature flags

| Feature | Default | Description |
|---------|---------|-------------|
| `sqlite` | yes | SQLite backend for the dedup store (via `zeph-db`, `zeph-memory`, `zeph-tools`) |
| `postgres` | no | PostgreSQL backend |
| `test-utils` | no | Test utilities and testcontainers for PostgreSQL integration tests |

> [!NOTE]
> `zeph-index` does not depend on `qdrant-client` directly. Vector storage is delegated to `zeph-memory`, which owns the Qdrant client lifecycle. Repo map generation works without Qdrant — it is injected into the agent context for all LLM providers unconditionally.

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

MIT
