# Architecture Overview

Cargo workspace (Edition 2024, resolver 3) with 8 crates + binary root.

Requires Rust 1.88+. Native async traits are used throughout — no `async-trait` crate.

## Workspace Layout

```
zeph (binary) — thin bootstrap glue
├── zeph-core       Agent loop, config, channel trait, context builder
├── zeph-llm        LlmProvider trait, Ollama + Claude + OpenAI + Candle backends, orchestrator, embeddings
├── zeph-skills     SKILL.md parser, registry with lazy body loading, embedding matcher, resource resolver, hot-reload
├── zeph-memory     SQLite + Qdrant, SemanticMemory orchestrator, summarization
├── zeph-channels   Telegram adapter (teloxide) with streaming
├── zeph-tools      ToolExecutor trait, ShellExecutor, WebScrapeExecutor, CompositeExecutor
├── zeph-mcp        MCP client via rmcp, multi-server lifecycle, unified tool matching (optional)
├── zeph-a2a        A2A protocol client + server, agent discovery, JSON-RPC 2.0 (optional)
└── zeph-tui        ratatui TUI dashboard with real-time metrics (optional)
```

## Dependency Graph

```
zeph (binary)
  └── zeph-core (orchestrates everything)
        ├── zeph-llm (leaf)
        ├── zeph-skills (leaf)
        ├── zeph-memory (leaf)
        ├── zeph-channels (leaf)
        ├── zeph-tools (leaf)
        ├── zeph-mcp (optional, leaf)
        ├── zeph-a2a (optional, leaf)
        └── zeph-tui (optional, leaf)
```

`zeph-core` is the only crate that depends on other workspace crates. All leaf crates are independent and can be tested in isolation.

## Key Design Decisions

- **Generic Agent:** `Agent<P: LlmProvider + Clone + 'static, C: Channel, T: ToolExecutor>` — fully generic over provider, channel, and tool executor
- **TLS:** rustls everywhere (no openssl-sys)
- **Errors:** `thiserror` for library crates, `anyhow` for application code (`zeph-core`, `main.rs`)
- **Lints:** workspace-level `clippy::all` + `clippy::pedantic` + `clippy::nursery`; `unsafe_code = "deny"`
- **Dependencies:** versions only in root `[workspace.dependencies]`; crates inherit via `workspace = true`
- **Feature gates:** optional crates (`zeph-mcp`, `zeph-a2a`, `zeph-tui`) are feature-gated in the binary
