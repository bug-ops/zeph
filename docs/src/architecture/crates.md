# Crates

## zeph-core

Agent loop, configuration loading, and context builder.

- `Agent<P, C, T>` — main agent loop with streaming support
- `Config` — TOML config loading with env var overrides
- `Channel` trait — abstraction for I/O (CLI, Telegram)
- Context builder — assembles system prompt from skills, memory, and summaries
- `VaultProvider` trait — pluggable secret resolution
- `MetricsSnapshot` / `MetricsCollector` — real-time metrics via `tokio::sync::watch` for TUI dashboard

## zeph-llm

LLM provider abstraction and backend implementations.

- `LlmProvider` trait — `chat()`, `chat_stream()`, `embed()`, `supports_streaming()`, `supports_embeddings()`
- `OllamaProvider` — local inference via ollama-rs
- `ClaudeProvider` — Anthropic Messages API with SSE streaming
- `OpenAiProvider` — OpenAI + compatible APIs (raw reqwest)
- `CandleProvider` — local GGUF model inference via candle
- `AnyProvider` — enum dispatch for runtime provider selection
- `ModelOrchestrator` — task-based multi-model routing with fallback chains

## zeph-skills

SKILL.md loader, skill registry, and prompt formatter.

- `SkillMeta` / `Skill` — metadata + lazy body loading via `OnceLock`
- `SkillRegistry` — manages skill lifecycle, lazy body access
- `SkillMatcher` — in-memory cosine similarity matching
- `QdrantSkillMatcher` — persistent embeddings with BLAKE3 delta sync
- `format_skills_prompt()` — assembles prompt with OS-filtered resources
- `resource.rs` — `discover_resources()` + `load_resource()` with path traversal protection
- Filesystem watcher for hot-reload (500ms debounce)

## zeph-memory

SQLite-backed conversation persistence with Qdrant vector search.

- `SqliteStore` — conversations, messages, summaries, skill usage, skill versions
- `QdrantStore` — vector storage and cosine similarity search
- `SemanticMemory<P>` — orchestrator coordinating SQLite + Qdrant + LlmProvider
- Automatic collection creation, graceful degradation without Qdrant

## zeph-channels

Channel implementations for the Zeph agent.

- `CliChannel` — stdin/stdout with immediate streaming output
- `TelegramChannel` — teloxide adapter with MarkdownV2 rendering, streaming via edit-in-place, user whitelisting, inline confirmation keyboards

## zeph-tools

Tool execution abstraction and shell backend.

- `ToolExecutor` trait — accepts LLM response, returns tool output
- `ShellExecutor` — bash block parser, command safety filter, sandbox validation
- `WebScrapeExecutor` — HTML scraping with CSS selectors, SSRF protection
- `CompositeExecutor<A, B>` — generic chaining with first-match-wins dispatch
- `AuditLogger` — structured JSON audit trail for all executions

## zeph-mcp

MCP client for external tool servers (optional, feature-gated).

- `McpClient` / `McpManager` — multi-server lifecycle management
- `McpToolExecutor` — tool execution via MCP protocol
- `McpToolRegistry` — tool embeddings in Qdrant with delta sync
- Dual transport: Stdio (child process) and HTTP (Streamable HTTP)
- Dynamic server management via `/mcp add`, `/mcp remove`

## zeph-a2a

A2A protocol client and server (optional, feature-gated).

- `A2aClient` — JSON-RPC 2.0 client with SSE streaming
- `AgentRegistry` — agent card discovery with TTL cache
- `AgentCardBuilder` — construct agent cards from runtime config
- A2A Server — axum-based HTTP server with bearer auth, rate limiting, body size limits
- `TaskManager` — in-memory task lifecycle management

## zeph-tui

ratatui-based TUI dashboard (optional, feature-gated).

- `TuiChannel` — Channel trait implementation bridging agent loop and TUI render loop via mpsc, oneshot-based confirmation dialog
- `App` — TUI state machine with Normal/Insert/Confirm modes, keybindings, scroll, live metrics polling via `watch::Receiver`
- `EventReader` — crossterm event loop on dedicated OS thread (avoids tokio starvation)
- Side panel widgets: `skills` (active/total), `memory` (SQLite, Qdrant, embeddings), `resources` (tokens, API calls, latency)
- Chat widget with bottom-up message feed, newline-aware rendering, scroll indicators (▲/▼), and streaming cursor
- Confirmation modal overlay widget with Y/N keybindings and focus capture
- Responsive layout: side panels hidden on terminals < 80 cols
- Multiline input via Shift+Enter
- Status bar with mode, skill count, tokens, Qdrant status, uptime
- Panic hook for terminal state restoration
- Re-exports `MetricsSnapshot` / `MetricsCollector` from zeph-core
