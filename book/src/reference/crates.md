# API Reference

Full API documentation for all Zeph crates is available on [docs.rs](https://docs.rs).

| Crate | Description | docs.rs |
|---|---|---|
| [zeph](https://crates.io/crates/zeph) | Binary entry point — bootstrap, AnyChannel dispatch, vault resolution | [docs](https://docs.rs/zeph) |
| [zeph-core](https://crates.io/crates/zeph-core) | Agent loop, config, channel trait, context builder, metrics, vault, redact | [docs](https://docs.rs/zeph-core) |
| [zeph-llm](https://crates.io/crates/zeph-llm) | `LlmProvider` trait, Ollama / Claude / OpenAI / Candle backends, orchestrator | [docs](https://docs.rs/zeph-llm) |
| [zeph-skills](https://crates.io/crates/zeph-skills) | SKILL.md parser, registry, embedding matcher, hot-reload, self-learning | [docs](https://docs.rs/zeph-skills) |
| [zeph-memory](https://crates.io/crates/zeph-memory) | SQLite + Qdrant, `SemanticMemory` orchestrator, summarization | [docs](https://docs.rs/zeph-memory) |
| [zeph-channels](https://crates.io/crates/zeph-channels) | Telegram adapter (teloxide) with streaming, CLI channel | [docs](https://docs.rs/zeph-channels) |
| [zeph-tools](https://crates.io/crates/zeph-tools) | `ToolExecutor` trait, `ShellExecutor`, `WebScrapeExecutor`, `CompositeExecutor`, audit | [docs](https://docs.rs/zeph-tools) |
| [zeph-tui](https://crates.io/crates/zeph-tui) | ratatui-based TUI dashboard with real-time metrics (feature-gated) | [docs](https://docs.rs/zeph-tui) |
| [zeph-mcp](https://crates.io/crates/zeph-mcp) | MCP client via rmcp, multi-server lifecycle, Qdrant tool registry | [docs](https://docs.rs/zeph-mcp) |
| [zeph-a2a](https://crates.io/crates/zeph-a2a) | A2A protocol client + server, agent discovery, JSON-RPC 2.0 | [docs](https://docs.rs/zeph-a2a) |
| [zeph-acp](https://crates.io/crates/zeph-acp) | ACP protocol support | [docs](https://docs.rs/zeph-acp) |
| [zeph-index](https://crates.io/crates/zeph-index) | AST-based code indexing, semantic retrieval, repo map generation | [docs](https://docs.rs/zeph-index) |
| [zeph-gateway](https://crates.io/crates/zeph-gateway) | HTTP gateway for webhook ingestion with bearer auth | [docs](https://docs.rs/zeph-gateway) |
| [zeph-scheduler](https://crates.io/crates/zeph-scheduler) | Cron-based periodic task scheduler with SQLite persistence | [docs](https://docs.rs/zeph-scheduler) |
| [zeph-orchestration](https://crates.io/crates/zeph-orchestration) | Multi-model orchestration and routing | [docs](https://docs.rs/zeph-orchestration) |
| [zeph-subagent](https://crates.io/crates/zeph-subagent) | Subagent spawning and lifecycle management | [docs](https://docs.rs/zeph-subagent) |
| [zeph-common](https://crates.io/crates/zeph-common) | Shared types and utilities | [docs](https://docs.rs/zeph-common) |
| [zeph-config](https://crates.io/crates/zeph-config) | Configuration schema and loading | [docs](https://docs.rs/zeph-config) |
| [zeph-vault](https://crates.io/crates/zeph-vault) | Secret storage with age encryption | [docs](https://docs.rs/zeph-vault) |
| [zeph-db](https://crates.io/crates/zeph-db) | Database layer (SQLite via sqlx) | [docs](https://docs.rs/zeph-db) |
| [zeph-sanitizer](https://crates.io/crates/zeph-sanitizer) | Input sanitization and content filtering | [docs](https://docs.rs/zeph-sanitizer) |
| [zeph-experiments](https://crates.io/crates/zeph-experiments) | Feature experiments and A/B testing | [docs](https://docs.rs/zeph-experiments) |
| [zeph-bench](https://crates.io/crates/zeph-bench) | Benchmarking CLI — LOCOMO, FRAMES, GAIA dataset loaders | [docs](https://docs.rs/zeph-bench) |
