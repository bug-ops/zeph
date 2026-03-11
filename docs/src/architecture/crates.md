# Crates

Each workspace crate has a focused responsibility. All leaf crates are independent and testable in isolation; only `zeph-core` depends on other workspace members.

## zeph (binary)

Thin entry point (26 LOC `main.rs`) that delegates all work to focused submodules:

- `runner.rs` — top-level dispatch: reads CLI flags, selects mode (ACP, TUI, CLI, daemon), and drives the `AnyChannel` loop
- `agent_setup.rs` — composes the `ToolExecutor` chain, initialises the MCP manager, and wires feature-gated extensions (code index, candle-stt, whisper-stt, response cache, cost tracker, summary provider)
- `tracing_init.rs` — configures the `tracing-subscriber` stack (env filter, JSON/pretty format)
- `tui_bridge.rs` — TUI event forwarding and TUI session runner
- `channel.rs` — constructs the runtime `AnyChannel` and CLI history builder
- `cli.rs` — clap argument definitions
- `acp.rs` — ACP server/client startup logic
- `daemon.rs` — daemon mode bootstrap
- `scheduler.rs` — scheduler bootstrap
- `commands/` — subcommand handlers for `vault`, `skill`, and `memory` management
- `tests.rs` — unit tests for the binary crate

## zeph-core

Agent loop, bootstrap orchestration, configuration loading, and context builder.

- `AppBuilder` — bootstrap orchestrator in `zeph-core::bootstrap/`, decomposed into:
  - `mod.rs` (278 LOC) — `AppBuilder` struct and orchestration entry points: `from_env()`, `build_provider()` with health check, `build_memory()`, `build_skill_matcher()`, `build_registry()`, `build_tool_executor()`, `build_watchers()`, `build_shutdown()`, `build_summary_provider()`
  - `config.rs` — config file resolution and vault argument parsing
  - `health.rs` — health check and provider warmup logic
  - `mcp.rs` — MCP manager and Qdrant tool registry creation
  - `provider.rs` — provider factory functions
  - `skills.rs` — skill matcher and embedding model helpers
  - `tests.rs` — unit tests for bootstrap logic
- `Agent<C>` — main agent loop generic over channel only. Tool execution uses `Box<dyn ErasedToolExecutor>` for object-safe dynamic dispatch (no `T` generic). Provider is resolved at construction time (`AnyProvider` enum dispatch, no `P` generic). Streaming support, message queue drain. Internal state is grouped into five domain structs (`MemoryState`, `SkillState`, `ContextState`, `McpState`, `IndexState`); logic is decomposed into `streaming.rs`, `persistence.rs`, and three dedicated subsystem structs described below
- `ContextManager` — owns context budget configuration, `token_counter` (`Arc<TokenCounter>`), compaction threshold (80%), compaction tail preservation, prune-protect token floor, and token safety margin. Exposes `should_compact()` used by the agent loop before each LLM call
- `ToolOrchestrator` — owns `doom_loop_history` (rolling hash window), `max_iterations` (default 10), summarize-tool-output flag, and `OverflowConfig`. Exposes `push_doom_hash()`, `clear_doom_history()`, and `is_doom_loop()` (returns `true` when last `DOOM_LOOP_WINDOW` hashes are identical)
- `LearningEngine` — owns `LearningConfig` and per-turn `reflection_used` flag. Exposes `is_enabled()`, `mark_reflection_used()`, `was_reflection_used()`, and `reset_reflection()` called at the start of each agent turn
- `SubAgentState` — state enum for sub-agent lifecycle (`Idle`, `Working`, `Completed`, `Failed`, `Cancelled`); defined in `zeph-core::subagent::state`, eliminating the former dependency on `zeph-a2a` for state types
- `AgentError` — typed error enum covering LLM, memory, channel, tool, context, and I/O failures (replaces prior `anyhow` usage)
- `Config` — TOML config loading with env var overrides
- `Channel` trait — abstraction for I/O (CLI, Telegram, TUI) with `recv()`, `try_recv()`, `send_queue_count()` for queue management. Returns `Result<_, ChannelError>` with typed variants (`Io`, `ChannelClosed`, `ConfirmationCancelled`)
- Context builder — assembles system prompt from skills, memory, summaries, environment, and project config
- Context engineering — proportional budget allocation, semantic recall injection, message trimming, runtime compaction
- `EnvironmentContext` — runtime gathering of cwd, git branch, OS, model name
- `project.rs` — ZEPH.md config discovery (walk up directory tree)
- `VaultProvider` trait — pluggable secret resolution
- `MetricsSnapshot` / `MetricsCollector` — real-time metrics via `tokio::sync::watch` for TUI dashboard
- `DaemonSupervisor` — component lifecycle monitor with health polling, PID file management, restart tracking
- `LoopbackChannel` / `LoopbackHandle` / `LoopbackEvent` — headless channel for daemon mode using paired tokio mpsc channels; auto-approves confirmations
- `LoopbackHandle::cancel_signal` — `Arc<Notify>` shared between the ACP session and the agent loop; calling `notify_one()` interrupts the running agent turn
- `hash::content_hash()` — BLAKE3-based utility returning a hex-encoded content hash for any byte slice; used for delta-sync checks and integrity verification across crates; available as `zeph_core::content_hash`
- `DiffData` — re-exported from `zeph_tools::executor::DiffData` as `zeph_core::DiffData`; the `zeph-core::diff` module has been removed in favour of this direct re-export

## zeph-llm

LLM provider abstraction and backend implementations.

- `LlmProvider` trait — `chat()`, `chat_typed()`, `chat_stream()`, `embed()`, `supports_streaming()`, `supports_embeddings()`, `supports_vision()`
- `MessagePart::Image` — image content part (raw bytes + MIME type) for multimodal input
- `EmbedFuture` / `EmbedFn` — canonical type aliases for embedding closures, re-exported by downstream crates (`zeph-skills`, `zeph-mcp`)
- `OllamaProvider` — local inference via ollama-rs
- `ClaudeProvider` — Anthropic Messages API with SSE streaming
- `OpenAiProvider` — OpenAI + compatible APIs (raw reqwest)
- `CandleProvider` — local GGUF model inference via candle
- `AnyProvider` — enum dispatch for runtime provider selection, generated via `delegate_provider!` macro
- `SpeechToText` trait — async transcription interface returning `Transcription` (text + duration + language)
- `WhisperProvider` — OpenAI Whisper API backend (feature-gated: `stt`)
- `ModelOrchestrator` — task-based multi-model routing with fallback chains

## zeph-skills

SKILL.md loader, skill registry, and prompt formatter.

- `SkillMeta` / `Skill` — metadata + lazy body loading via `OnceLock`
- `SkillRegistry` — manages skill lifecycle, lazy body access
- `SkillMatcher` — in-memory cosine similarity matching
- `QdrantSkillMatcher` — persistent embeddings with BLAKE3 delta sync
- `format_skills_prompt()` — assembles prompt with OS-filtered resources
- `format_skills_catalog()` — description-only entries for non-matched skills
- `resource.rs` — `discover_resources()` + `load_resource()` with path traversal protection and canonical path validation; lazy resource loading (resources resolved on first activation, not at startup)
- File reference validation — local links in skill bodies are checked against the skill directory; broken references and path traversal attempts are rejected at load time
- `sanitize_skill_body()` — escapes XML-like structural tags in untrusted (non-`Trusted`) skill bodies before prompt injection, preventing prompt boundary confusion
- `TrustLevel` — re-exported from `zeph-tools::trust_level` for use by skill trust logic; the canonical definition lives in `zeph-tools`
- Filesystem watcher for hot-reload (500ms debounce)

## zeph-memory

SQLite-backed conversation persistence with Qdrant vector search.

- `SqliteStore` — conversations, messages, summaries, skill usage, skill versions, ACP session persistence (`acp_sessions.rs`)
- `QdrantOps` — shared helper consolidating common Qdrant operations (ensure_collection, upsert, search, delete, scroll), used by `QdrantStore`, `CodeStore`, `QdrantSkillMatcher`, and `McpToolRegistry`
- `QdrantStore` — vector storage and cosine similarity search with `MessageKind` enum (`Regular` | `Summary`) for payload classification
- `SemanticMemory<P>` — orchestrator coordinating SQLite + Qdrant + LlmProvider
- `Embeddable` trait — generic interface for types that can be embedded and synced to Qdrant (provides `id`, `content_for_embedding`, `content_hash`, `to_payload`)
- `EmbeddingRegistry<T: Embeddable>` — generic Qdrant sync/search engine: delta-syncs items by BLAKE3 content hash, performs cosine similarity search, and returns scored results
- `VectorStore` trait — object-safe abstraction over vector database operations (`ensure_collection`, `upsert_points`, `search`, `delete_points`, `scroll_points`); implemented by `QdrantOps`. `zeph-index` uses this trait instead of depending on `qdrant-client` directly, keeping the crate decoupled from the Qdrant client library
- Automatic collection creation, graceful degradation without Qdrant
- `DocumentLoader` trait — async document loading with `load(&Path)` returning `Vec<Document>`, dyn-compatible via `Pin<Box<dyn Future>>`
- `TextLoader` — plain text and markdown loader (`.txt`, `.md`, `.markdown`) with configurable `max_file_size` (50 MiB default) and path canonicalization
- `PdfLoader` — PDF text extraction via `pdf-extract` with `spawn_blocking` (feature-gated: `pdf`)
- `TextSplitter` — configurable text chunking with `chunk_size`, `chunk_overlap`, and sentence-aware splitting
- `IngestionPipeline` — document ingestion orchestrator: load → split → embed → store via `QdrantOps`
- `TokenCounter` — BPE-based token counting via tiktoken-rs `cl100k_base`, DashMap cache (10K cap), 64 KiB input guard, OpenAI tool schema token formula, `chars/4` fallback

## zeph-channels

Channel implementations for the Zeph agent.

- `AnyChannel` — enum dispatch over all channel variants (Cli, Telegram, Discord, Slack, Tui, Loopback), used by the binary for runtime channel selection
- `CliChannel` — stdin/stdout with immediate streaming output, blocking recv (queue always empty)
- `TelegramChannel` — teloxide adapter with MarkdownV2 rendering, streaming via edit-in-place, user whitelisting, inline confirmation keyboards, mpsc-backed message queue with 500ms merge window
- `ChannelError` is not defined in this crate; use `zeph_core::channel::ChannelError` directly. The duplicate definition that previously existed in `zeph-channels::error` has been removed.

## zeph-tools

Tool execution abstraction and shell backend. This crate has no dependency on `zeph-skills`.

- `ToolExecutor` trait + `ErasedToolExecutor` — `ErasedToolExecutor` is an object-safe wrapper enabling `Box<dyn ErasedToolExecutor>` for dynamic dispatch in `Agent<C>`
- `ToolRegistry` — typed definitions for built-in tools (bash, read, edit, write, find_path, list_directory, create_directory, delete_path, move_path, copy_path, grep, web_scrape, fetch, diagnostics), injected into system prompt as `<tools>` catalog
- `ToolCall` / `execute_tool_call()` — structured tool invocation with typed parameters alongside legacy bash extraction (dual-mode)
- `FileExecutor` — sandboxed file operations (read, write, edit, find_path, list_directory, create_directory, delete_path, move_path, copy_path, grep) with ancestor-walk path canonicalization and lstat-based symlink safety
- `ShellExecutor` — bash block parser, command safety filter, sandbox validation; exposes `check_blocklist()` and `DEFAULT_BLOCKED_COMMANDS` as public API so ACP executors apply the same blocklist
- `WebScrapeExecutor` — HTML scraping with CSS selectors (`web_scrape`) and plain URL-to-text (`fetch`), both with SSRF protection
- `DiagnosticsExecutor` — runs `cargo check`/`cargo clippy --message-format=json`, returns structured diagnostics capped at configurable max; uses `tokio::process::Command`
- `CompositeExecutor<A, B>` — generic chaining with first-match-wins dispatch, routes structured tool calls by `tool_id` to the appropriate backend; used to place ACP executors ahead of local tools so IDE-proxied operations take priority
- `DynExecutor` — newtype wrapping `Arc<dyn ErasedToolExecutor>` so a heap-allocated erased executor can be used anywhere a concrete `ToolExecutor` is required; enables runtime composition without static type chains
- `TrustLevel` — canonical trust tier enum (`Trusted`, `Verified`, `Quarantined`, `Blocked`) used by `TrustGateExecutor` to enforce per-skill tool access restrictions; re-exported by `zeph-skills` for convenience
- `TrustGateExecutor` — wraps any `ToolExecutor` and blocks tool calls that exceed the active skill's `TrustLevel`
- `DiffData` — structured diff payload; re-exported as `zeph_core::DiffData` via `pub use zeph_tools::executor::DiffData` in `zeph-core`
- `AuditLogger` — structured JSON audit trail for all executions
- `truncate_tool_output()` — head+tail split at 30K chars with UTF-8 safe boundaries

## zeph-index

AST-based code indexing, semantic retrieval, and repo map generation (always-on — no feature flag). All tree-sitter language grammars (Rust, Python, JavaScript/TypeScript, Go, and config formats) are compiled unconditionally. This crate does not depend directly on `qdrant-client`; all vector operations go through the `VectorStore` trait from `zeph-memory`, keeping the crate decoupled from the Qdrant client library.

- `Lang` enum — supported languages with tree-sitter grammar registry
- `chunk_file()` — AST-based chunking with greedy sibling merge, scope chains, import extraction
- `contextualize_for_embedding()` — prepends file path, scope, language, imports to code for better embedding quality
- `CodeStore` — dual-write storage: vector store via `VectorStore` trait (`zeph_code_chunks` collection) + SQLite metadata with BLAKE3 content-hash change detection; vector operations are delegated to `QdrantOps` which implements `VectorStore`
- `CodeIndexer<P>` — project indexer orchestrator: walk, chunk, embed, store with incremental skip of unchanged chunks
- `CodeRetriever<P>` — hybrid retrieval with query classification (Semantic / Grep / Hybrid), budget-aware chunk packing
- `generate_repo_map()` — compact structural view via tree-sitter ts-query, extracting `SymbolInfo` (name, kind, visibility, line) for all supported languages; injected unconditionally for all providers regardless of Qdrant availability
- `hover_symbol_at()` — tree-sitter hover pre-filter for LSP context injection; resolves the symbol under cursor for any supported language (replaces previous Rust-only regex)

## zeph-gateway

HTTP gateway for webhook ingestion (optional, feature-gated).

- `GatewayServer` -- axum-based HTTP server with fluent builder API
- `POST /webhook` -- accepts JSON payloads (`channel`, `sender`, `body`), forwards to agent loop via `mpsc::Sender<String>`
- `GET /health` -- unauthenticated health endpoint returning uptime
- Bearer token auth middleware with constant-time comparison (blake3 + `subtle`)
- Per-IP rate limiting with 60s sliding window and automatic eviction at 10K entries
- Body size limit via `tower_http::limit::RequestBodyLimitLayer`
- Graceful shutdown via `watch::Receiver<bool>`

## zeph-scheduler

Cron-based periodic task scheduler with SQLite persistence (optional, feature-gated).

- `Scheduler` -- tick loop checking due tasks every 60 seconds
- `ScheduledTask` -- task definition with 6-field cron expression (via `cron` crate)
- `TaskKind` -- built-in kinds (`memory_cleanup`, `skill_refresh`, `health_check`, `update_check`) and `Custom(String)`
- `TaskHandler` trait -- async execution interface receiving `serde_json::Value` config
- `JobStore` -- SQLite-backed persistence tracking `last_run` timestamps and status
- Graceful shutdown via `watch::Receiver<bool>`

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
- A2A Server — axum-based HTTP server with bearer auth, rate limiting with TTL-based eviction (60s sweep, 10K max entries), body size limits
- `TaskManager` — in-memory task lifecycle management
- `ProcessorEvent` — streaming event enum (`StatusUpdate`, `ArtifactChunk`) for per-token SSE delivery; `TaskProcessor::process` accepts `mpsc::Sender<ProcessorEvent>`

## zeph-acp

Agent Client Protocol server — IDE integration via ACP (optional, feature-gated).

- **Rich content** — ACP prompts may contain multi-modal content blocks. Image blocks are forwarded to LLM providers that support vision (Claude, OpenAI, Ollama). Resource content blocks (embedded text from IDE) are appended to the user prompt. Tool output includes `ToolCallLocation` for IDE navigation (file path, line range).
- `ZephAcpAgent` — `acp::Agent` implementation; manages concurrent sessions with LRU eviction (`max_sessions`, default 4), forwards prompts to the agent loop, and emits `SessionNotification` updates back to the IDE
- `AcpContext` — per-session bundle of IDE-proxied capabilities passed to `AgentSpawner`:
  - `file_executor: Option<AcpFileExecutor>` — reads/writes routed to the IDE filesystem proxy
  - `shell_executor: Option<AcpShellExecutor>` — shell commands routed through the IDE terminal proxy
  - `permission_gate: Option<AcpPermissionGate>` — confirmation requests forwarded to the IDE UI
  - `cancel_signal: Arc<Notify>` — shared with `LoopbackHandle`; firing it interrupts the running agent turn
- `SessionContext` — per-session struct carrying `session_id`, `conversation_id`, and `working_dir`; ensures each ACP session maps to exactly one Zeph conversation in SQLite
- `AgentSpawner` — `Arc<dyn Fn(LoopbackChannel, Option<AcpContext>, SessionContext) -> ...>` factory that the main binary supplies; wires `AcpContext` and `SessionContext` into the agent loop
- `AcpPermissionGate` — permission gate backed by `acp::Connection`; cache key uses `tool_call_id` as fallback when `title` is `None` to prevent distinct untitled tools from sharing a cached decision. `AllowAlways`/`RejectAlways` decisions are persisted to a TOML file (`~/.config/zeph/acp-permissions.toml` by default, configurable via `acp.permission_file` or `ZEPH_ACP_PERMISSION_FILE`). The file is written atomically with `0o600` permissions on Unix. Persisted rules are loaded on startup and saved on each decision change
- `AcpFileExecutor` / `AcpShellExecutor` — IDE-proxied file and shell backends; each spawns a local task for the connection handler
- **Model switching** — `set_session_config_option` with `config_id = "model"` validates the requested model against `available_models` allowlist, resolves it via `ProviderFactory` (`Arc<dyn Fn(&str) -> Option<AnyProvider>>`), and stores the result in a shared `provider_override: Arc<RwLock<Option<AnyProvider>>>` that the agent loop checks on each turn. RwLock uses `PoisonError::into_inner` for poison recovery
- **Extension methods** — `ext_method` dispatches custom JSON-RPC methods: `_agent/mcp/add`, `_agent/mcp/remove`, `_agent/mcp/list` delegate to `McpManager` for runtime MCP server management
- **HTTP+SSE transport** (feature `acp-http`) — axum-based POST `/acp` accepts JSON-RPC requests and returns SSE response streams; GET `/acp` reconnects SSE notifications with `Acp-Session-Id` header routing. Includes 1 MiB body limit, UUID session ID validation, CORS deny-all, and SSE keepalive pings (15s)
- **WebSocket transport** (feature `acp-http`) — GET `/acp/ws` upgrades to bidirectional WebSocket with 1 MiB message limit and max_sessions enforcement (503)
- **Duplex bridge** — `tokio::io::duplex` connects axum handlers to the ACP SDK's `AsyncRead+AsyncWrite` interface. Each HTTP/WS connection spawns a dedicated OS thread with `LocalSet` (required because Agent trait is `!Send`)
- `AcpTransport` enum (`Stdio`/`Http`/`Both`) and `http_bind` config field control which transports are active

### Session Lifecycle

`ZephAcpAgent` supports multi-session concurrency with configurable `max_sessions` (default 4). Sessions are tracked in an LRU map; when the limit is reached, the least-recently-used session is evicted and its agent task cancelled.

- **Persistence** — session state and events are persisted to SQLite via `acp_sessions` and `acp_session_events` tables. Each session links to a `conversation_id` (migration 026) so that message history is isolated per-session. On `load_session`, the existing conversation is restored; on `fork_session`, messages are copied to a new conversation.
- **Idle reaper** — a background task periodically scans sessions and removes those idle longer than `session_idle_timeout_secs` (default 1800).
- **Configuration** — `AcpConfig` exposes `max_sessions` and `session_idle_timeout_secs`, with env overrides `ZEPH_ACP_MAX_SESSIONS` and `ZEPH_ACP_SESSION_IDLE_TIMEOUT_SECS`.

### AcpContext wiring

When a new ACP session starts, `ZephAcpAgent::new_session` calls `build_acp_context`, which constructs the three proxied executors from the IDE capabilities advertised during `initialize`. The context is passed to `AgentSpawner` alongside the `LoopbackChannel`. The spawner builds a `CompositeExecutor` with ACP executors as the primary layer and local `ShellExecutor`/`FileExecutor` as fallback:

```text
CompositeExecutor
├── primary:  AcpShellExecutor / AcpFileExecutor  (IDE-proxied, used when AcpContext present)
└── fallback: ShellExecutor / FileExecutor        (local, used in non-ACP sessions)
```

### Cancellation

`LoopbackHandle::cancel_signal` (`Arc<Notify>`) is cloned into `AcpContext` at session creation. When the IDE calls `cancel`, `ZephAcpAgent::cancel` fires `notify_one()` on the signal and removes the session. The agent loop polls this notifier and aborts the current turn. `AgentBuilder::with_cancel_signal()` wires the signal into the agent so a new `Notify` is not created internally.

## zeph-tui

ratatui-based TUI dashboard (optional, feature-gated).

- `TuiChannel` — Channel trait implementation bridging agent loop and TUI render loop via mpsc, oneshot-based confirmation dialog, bounded message queue (max 10) with 500ms merge window
- `App` — TUI state machine with Normal/Insert/Confirm modes, keybindings, scroll, live metrics polling via `watch::Receiver`, queue badge indicator `[+N queued]`, Ctrl+K to clear queue, command palette with fuzzy matching
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
