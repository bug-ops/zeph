# Crates

Each workspace crate has a focused responsibility. All leaf crates are independent and testable in isolation; only `zeph-core` depends on other workspace members.

## zeph-core

Agent loop, configuration loading, and context builder.

- `Agent<P, C, T>` — main agent loop with streaming support, message queue drain, configurable `max_tool_iterations` (default 10), doom-loop detection, and context budget check (stops at 80% threshold)
- `Config` — TOML config loading with env var overrides
- `Channel` trait — abstraction for I/O (CLI, Telegram, TUI) with `recv()`, `try_recv()`, `send_queue_count()` for queue management
- Context builder — assembles system prompt from skills, memory, summaries, environment, and project config
- Context engineering — proportional budget allocation, semantic recall injection, message trimming, runtime compaction
- `EnvironmentContext` — runtime gathering of cwd, git branch, OS, model name
- `project.rs` — ZEPH.md config discovery (walk up directory tree)
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
- `format_skills_catalog()` — description-only entries for non-matched skills
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

- `CliChannel` — stdin/stdout with immediate streaming output, blocking recv (queue always empty)
- `TelegramChannel` — teloxide adapter with MarkdownV2 rendering, streaming via edit-in-place, user whitelisting, inline confirmation keyboards, mpsc-backed message queue with 500ms merge window

## zeph-tools

Tool execution abstraction and shell backend.

- `ToolExecutor` trait — accepts LLM response or structured `ToolCall`, returns tool output
- `ToolRegistry` — typed definitions for 7 built-in tools (bash, read, edit, write, glob, grep, web_scrape), injected into system prompt as `<tools>` catalog
- `ToolCall` / `execute_tool_call()` — structured tool invocation with typed parameters alongside legacy bash extraction (dual-mode)
- `FileExecutor` — sandboxed file operations (read, write, edit, glob, grep) with ancestor-walk path canonicalization
- `ShellExecutor` — bash block parser, command safety filter, sandbox validation
- `WebScrapeExecutor` — HTML scraping with CSS selectors, SSRF protection
- `CompositeExecutor<A, B>` — generic chaining with first-match-wins dispatch, routes structured tool calls by `tool_id` to the appropriate backend
- `AuditLogger` — structured JSON audit trail for all executions
- `truncate_tool_output()` — head+tail split at 30K chars with UTF-8 safe boundaries

## zeph-index

AST-based code indexing, semantic retrieval, and repo map generation (optional, feature-gated).

- `Lang` enum — supported languages with tree-sitter grammar registry, feature-gated per language group
- `chunk_file()` — AST-based chunking with greedy sibling merge, scope chains, import extraction
- `contextualize_for_embedding()` — prepends file path, scope, language, imports to code for better embedding quality
- `CodeStore` — dual-write storage: Qdrant vectors (`zeph_code_chunks` collection) + SQLite metadata with BLAKE3 content-hash change detection
- `CodeIndexer<P>` — project indexer orchestrator: walk, chunk, embed, store with incremental skip of unchanged chunks
- `CodeRetriever<P>` — hybrid retrieval with query classification (Semantic / Grep / Hybrid), budget-aware chunk packing
- `generate_repo_map()` — compact structural view via tree-sitter signature extraction, budget-constrained

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

- `TuiChannel` — Channel trait implementation bridging agent loop and TUI render loop via mpsc, oneshot-based confirmation dialog, bounded message queue (max 10) with 500ms merge window
- `App` — TUI state machine with Normal/Insert/Confirm modes, keybindings, scroll, live metrics polling via `watch::Receiver`, queue badge indicator `[+N queued]`, Ctrl+K to clear queue
- `EventReader` — crossterm event loop on dedicated OS thread (avoids tokio starvation)
- Side panel widgets: `skills` (active/total), `memory` (SQLite, Qdrant, embeddings), `resources` (tokens, API calls, latency)
- Chat widget with bottom-up message feed, pulldown-cmark markdown rendering, scrollbar with proportional thumb, mouse scroll, thinking block segmentation, and streaming cursor
- Splash screen widget with colored block-letter banner
- Conversation history loading from SQLite on startup
- Confirmation modal overlay widget with Y/N keybindings and focus capture
- Responsive layout: side panels hidden on terminals < 80 cols
- Multiline input via Shift+Enter
- Status bar with mode, skill count, tokens, Qdrant status, uptime
- Panic hook for terminal state restoration
- Re-exports `MetricsSnapshot` / `MetricsCollector` from zeph-core
