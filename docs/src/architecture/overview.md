# Architecture Overview

Cargo workspace (Edition 2024, resolver 3) with 10 crates + binary root.

Requires Rust 1.88+. Native async traits are used throughout — no `async-trait` crate.

## Workspace Layout

```text
zeph (binary) — thin bootstrap glue
├── zeph-core       Agent loop, config, config hot-reload, channel trait, context builder
├── zeph-llm        LlmProvider trait, Ollama + Claude + OpenAI + Candle backends, orchestrator, embeddings
├── zeph-skills     SKILL.md parser, registry with lazy body loading, embedding matcher, resource resolver, hot-reload
├── zeph-memory     SQLite + Qdrant, SemanticMemory orchestrator, summarization
├── zeph-channels   Telegram adapter (teloxide) with streaming
├── zeph-tools      ToolExecutor trait, ShellExecutor, WebScrapeExecutor, CompositeExecutor
├── zeph-index      AST-based code indexing, hybrid retrieval, repo map (optional)
├── zeph-mcp        MCP client via rmcp, multi-server lifecycle, unified tool matching (optional)
├── zeph-a2a        A2A protocol client + server, agent discovery, JSON-RPC 2.0 (optional)
└── zeph-tui        ratatui TUI dashboard with real-time metrics (optional)
```

## Dependency Graph

```text
zeph (binary)
  └── zeph-core (orchestrates everything)
        ├── zeph-llm (leaf)
        ├── zeph-skills (leaf)
        ├── zeph-memory (leaf)
        ├── zeph-channels (leaf)
        ├── zeph-tools (leaf)
        ├── zeph-index (optional, leaf)
        ├── zeph-mcp (optional, leaf)
        ├── zeph-a2a (optional, leaf)
        └── zeph-tui (optional, leaf)
```

`zeph-core` is the only crate that depends on other workspace crates. All leaf crates are independent and can be tested in isolation.

## Agent Loop

The agent loop processes user input in a continuous cycle:

1. Read initial user message via `channel.recv()`
2. Build context from skills, memory, and environment
3. Stream LLM response token-by-token
4. Execute any tool calls in the response
5. Drain queued messages (if any) via `channel.try_recv()` and repeat from step 2

Queued messages are processed sequentially with full context rebuilding between each. Consecutive messages within 500ms are merged to reduce fragmentation. The queue holds a maximum of 10 messages; older messages are dropped when full.

## Key Design Decisions

- **Generic Agent:** `Agent<P: LlmProvider + Clone + 'static, C: Channel, T: ToolExecutor>` — fully generic over provider, channel, and tool executor. Internal state is grouped into five domain structs (`MemoryState`, `SkillState`, `ContextState`, `McpState`, `IndexState`) with logic decomposed into `streaming.rs` and `persistence.rs` submodules
- **TLS:** rustls everywhere (no openssl-sys)
- **Errors:** `thiserror` for all crates with typed error enums (`ChannelError`, `AgentError`, `LlmError`, etc.); `anyhow` only for top-level orchestration in `main.rs`
- **Lints:** workspace-level `clippy::all` + `clippy::pedantic` + `clippy::nursery`; `unsafe_code = "deny"`
- **Dependencies:** versions only in root `[workspace.dependencies]`; crates inherit via `workspace = true`
- **Feature gates:** optional crates (`zeph-index`, `zeph-mcp`, `zeph-a2a`, `zeph-tui`) are feature-gated in the binary
- **Context engineering:** proportional budget allocation, semantic recall injection, message trimming, runtime compaction, environment context injection, progressive skill loading, ZEPH.md project config discovery
