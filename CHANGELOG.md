# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added
- Syntax-highlighted diff view for write/edit tool output in TUI (#451)
  - Diff rendering with green/red backgrounds for added/removed lines
  - Word-level change highlighting within modified lines
  - Syntax highlighting via tree-sitter
  - Compact/expanded toggle with existing 'e' key binding
  - New dependency: `similar` 2.7.0
- Per-tool inline filter stats in CLI chat: `[shell] cargo test (342 lines -> 28 lines, 91.8% filtered)` (#449)
- Filter metrics in TUI Resources panel: confidence distribution, command hit rate, token savings (#448)
- Periodic 250ms tick in TUI event loop for real-time metrics refresh (#447)
- Output filter architecture improvements (M26.1): `CommandMatcher` enum, `FilterConfidence`, `FilterPipeline`, `SecurityPatterns`, per-filter TOML config (#452)
- Token savings tracking and metrics for output filtering (#445)
- Smart tool output filtering: command-aware filters that compress tool output before context insertion
- `OutputFilter` trait and `OutputFilterRegistry` with first-match-wins dispatch
- `sanitize_output()` ANSI escape and progress bar stripping (runs on all tool output)
- Test output filter: cargo test/nextest failures-only mode (94-99% token savings on green suites)
- Git output filter: compact status/diff/log/push compression (80-99% savings)
- Clippy output filter: group warnings by lint rule (70-90% savings)
- Directory listing filter: hide noise directories (target, node_modules, .git)
- Log deduplication filter: normalize timestamps/UUIDs, count repeated patterns (70-85% savings)
- `[tools.filters]` config section with `enabled` toggle
- Skill trust levels: 4-tier model (Trusted, Verified, Quarantined, Blocked) with per-turn enforcement
- `TrustGateExecutor` wrapping tool execution with trust-level permission checks
- `AnomalyDetector` with sliding-window threshold counters for quarantined skill monitoring
- blake3 content hashing for skill integrity verification on load and hot-reload
- Quarantine prompt wrapping for structural isolation of untrusted skill bodies
- Self-learning gate: skills with trust < Verified skip auto-improvement
- `skill_trust` SQLite table with migration 009
- CLI commands: `/skill trust`, `/skill block`, `/skill unblock`
- `[skills.trust]` config section (default_level, local_level, hash_mismatch_level)
- `ProviderKind` enum for type-safe provider selection in config
- `RuntimeConfig` struct grouping agent runtime fields
- `AnyProvider::embed_fn()` shared embedding closure helper
- `Config::validate()` with bounds checking for critical config values
- `sanitize_paths()` for stripping absolute paths from error messages
- 10-second timeout wrapper for embedding API calls
- `full` feature flag enabling all optional features

### Changed
- Extract bootstrap logic from main.rs into `zeph-core::bootstrap::AppBuilder` (#393): main.rs reduced from 2313 to 978 lines
- `SecurityConfig` and `TimeoutConfig` gain `Clone + Copy`
- `AnyChannel` moved from main.rs to zeph-channels crate
- Remove 8 lightweight feature gates, make always-on: openai, compatible, orchestrator, router, self-learning, qdrant, vault-age, mcp (#438)
- Default features reduced to minimal set (empty after M26)
- Skill matcher concurrency reduced from 50 to 20
- `String::with_capacity` in context building loops
- CI updated to use `--features full`

### Breaking
- `LlmConfig.provider` changed from `String` to `ProviderKind` enum
- Default features reduced -- users needing a2a, candle, mcp, openai, orchestrator, router, tui must enable explicitly or use `--features full`
- Telegram channel rejects empty `allowed_users` at startup
- Config with extreme values now rejected by `Config::validate()`

### Deprecated
- `ToolExecutor::execute()` string-based dispatch (use `execute_tool_call()` instead)

### Fixed
- Closed #410 (clap dropped atty), #411 (rmcp updated quinn-udp), #413 (A2A body limit already present)

## [0.9.9] - 2026-02-17

### Added
- `zeph-gateway` crate: axum HTTP gateway with POST /webhook ingestion, bearer auth (blake3 + ct_eq), per-IP rate limiting, GET /health endpoint, feature-gated (`gateway`) (#379)
- `zeph-core::daemon` module: component supervisor with health monitoring, PID file management, graceful shutdown, feature-gated (`daemon`) (#380)
- `zeph-scheduler` crate: cron-based periodic task scheduler with SQLite persistence, built-in tasks (memory_cleanup, skill_refresh, health_check), TaskHandler trait, feature-gated (`scheduler`) (#381)
- New config sections: `[gateway]`, `[daemon]`, `[scheduler]` in config/default.toml (#367)
- New optional feature flags: `gateway`, `daemon`, `scheduler`
- Hybrid memory search: FTS5 keyword search combined with Qdrant vector similarity (#372, #373, #374)
- SQLite FTS5 virtual table with auto-sync triggers for full-text keyword search
- Configurable `vector_weight`/`keyword_weight` in `[memory.semantic]` for hybrid ranking
- FTS5-only fallback when Qdrant is unavailable (replaces empty results)
- `AutonomyLevel` enum (ReadOnly/Supervised/Full) for controlling tool access (#370)
- `autonomy_level` config key in `[security]` section (default: supervised)
- Read-only mode restricts agent to file_read, file_glob, file_grep, web_scrape
- Full mode allows all tools without confirmation prompts
- Documented `[telegram].allowed_users` allowlist in default config (#371)
- OpenTelemetry OTLP trace export with `tracing-opentelemetry` layer, feature-gated behind `otel` (#377)
- `[observability]` config section with exporter selection and OTLP endpoint
- Instrumentation spans for LLM calls (`llm_call`) and tool executions (`tool_exec`)
- `CostTracker` with per-model token pricing and configurable daily budget limits (#378)
- `[cost]` config section with `enabled` and `max_daily_cents` options
- `cost_spent_cents` field in `MetricsSnapshot` for TUI cost display
- Discord channel adapter with Gateway v10 WebSocket, slash commands, edit-in-place streaming (#382)
- Slack channel adapter with Events API webhook, HMAC-SHA256 signature verification, streaming (#383)
- Feature flags: `discord` and `slack` (opt-in) in zeph-channels and root crate
- `DiscordConfig` and `SlackConfig` with token redaction in Debug impls
- Slack timestamp replay protection (reject requests >5min old)
- Configurable Slack webhook bind address (`webhook_host`)

## [0.9.8] - 2026-02-16

### Added
- Graceful shutdown on Ctrl-C with farewell message and MCP server cleanup (#355)
- Cancel-aware LLM streaming via tokio::select on shutdown signal (#358)
- `McpManager::shutdown_all_shared()` with per-client 5s timeout (#356)
- Indexer progress logging with file count and per-file stats
- Skip code index for providers with native tool_use (#357)
- OpenAI prompt caching: parse and report cached token usage (#348)
- Syntax highlighting for TUI code blocks via tree-sitter-highlight (#345, #346, #347)
- Anthropic prompt caching with structured system content blocks (#337)
- Configurable summary provider for tool output summarization via local model (#338)
- Aggressive inline pruning of stale tool outputs in tool loops (#339)
- Cache usage metrics (cache_read_tokens, cache_creation_tokens) in MetricsSnapshot (#340)
- Native tool_use support for Claude provider (Anthropic API format) (#256)
- Native function calling support for OpenAI provider (#257)
- `ToolDefinition`, `ChatResponse`, `ToolUseRequest` types in zeph-llm (#254)
- `ToolUse`/`ToolResult` variants in `MessagePart` for structured tool flow (#255)
- Dual-mode agent loop: native structured path alongside legacy text extraction (#258)
- Dual system prompt: native tool_use instructions for capable providers, fenced-block instructions for legacy providers

### Changed
- Consolidate all SQLite migrations into root `migrations/` directory (#354)

## [0.9.7] - 2026-02-15

### Performance
- Token estimation uses `len() / 3` for improved accuracy (#328)
- Explicit tokio feature selection replacing broad feature gates (#326)
- Concurrent skill embedding for faster startup (#327)
- Pre-allocate strings in hot paths to reduce allocations (#329)
- Parallel context building via `try_join!` (#331)
- Criterion benchmark suite for core operations (#330)

### Security
- Path traversal protection in shell sandbox (#325)
- Canonical path validation in skill loader (#322)
- SSRF protection for MCP server connections (#323)
- Remove MySQL/RSA vulnerable transitive dependencies (#324)
- Secret redaction patterns for Google and GitLab tokens (#320)
- TTL-based eviction for rate limiter entries (#321)

### Changed
- `QdrantOps` shared helper trait for Qdrant collection operations (#304)
- `delegate_provider!` macro replacing boilerplate provider delegation (#303)
- Remove `TuiError` in favor of unified error handling (#302)
- Generic `recv_optional` replacing per-channel optional receive logic (#301)

### Dependencies
- Upgraded rmcp to 0.15, toml to 1.0, uuid to 1.21 (#296)
- Cleaned up deny.toml advisory and license configuration (#312)

## [0.9.6] - 2026-02-15

### Changed
- **BREAKING**: `ToolDef` schema field replaced `Vec<ParamDef>` with `schemars::Schema` auto-derived from Rust structs via `#[derive(JsonSchema)]`
- **BREAKING**: `ParamDef` and `ParamType` removed from `zeph-tools` public API
- **BREAKING**: `ToolRegistry::new()` replaced with `ToolRegistry::from_definitions()`; registry no longer hardcodes built-in tools — each executor owns its definitions via `tool_definitions()`
- **BREAKING**: `Channel` trait now requires `ChannelError` enum with typed error handling replacing `anyhow::Result`
- **BREAKING**: `Agent::new()` signature changed to accept new field grouping; agent struct refactored into 5 inner structs for improved organization
- **BREAKING**: `AgentError` enum introduced with 7 typed variants replacing scattered `anyhow::Error` handling
- `ToolDef` now includes `InvocationHint` (FencedBlock/ToolCall) so LLM prompt shows exact invocation format per tool
- `web_scrape` tool definition includes all parameters (`url`, `select`, `extract`, `limit`) auto-derived from `ScrapeInstruction`
- `ShellExecutor` and `WebScrapeExecutor` now implement `tool_definitions()` for single source of truth
- Replaced `tokio` "full" feature with granular features in zeph-core (async-io, macros, rt, sync, time)
- Removed `anyhow` dependency from zeph-channels
- Message persistence now uses `MessageKind` enum instead of `is_summary` bool for qdrant storage

### Added
- `ChannelError` enum with typed variants for channel operation failures
- `AgentError` enum with 7 typed variants for agent operation failures (streaming, persistence, configuration, etc.)
- Workspace-level `qdrant` feature flag for optional semantic memory support
- Type aliases consolidated into zeph-llm: `EmbedFuture` and `EmbedFn` with typed `LlmError`
- `streaming.rs` and `persistence.rs` modules extracted from agent module for improved code organization
- `MessageKind` enum for distinguishing summary and regular messages in storage

### Removed
- `anyhow::Result` from Channel trait (replaced with `ChannelError`)
- Direct `anyhow::Error` usage in agent module (replaced with `AgentError`)

## [0.9.5] - 2026-02-14

### Added
- Pattern-based permission policy with glob matching per tool (allow/ask/deny), first-match-wins evaluation (#248)
- Legacy blocked_commands and confirm_patterns auto-migrated to permission rules (#249)
- Denied tools excluded from LLM system prompt (#250)
- Tool output overflow: full output saved to file when truncated, path notice appended for LLM access (#251)
- Stale tool output overflow files cleaned up on startup (>24h TTL) (#252)
- `ToolRegistry` with typed `ToolDef` definitions for 7 built-in tools (bash, read, edit, write, glob, grep, web_scrape) (#239)
- `FileExecutor` for sandboxed file operations: read, write, edit, glob, grep (#242)
- `ToolCall` struct and `execute_tool_call()` on `ToolExecutor` trait for structured tool invocation (#241)
- `CompositeExecutor` routes structured tool calls to correct sub-executor by tool_id (#243)
- Tool catalog section in system prompt via `ToolRegistry::format_for_prompt()` (#244)
- Configurable `max_tool_iterations` (default 10, previously hardcoded 3) via TOML and `ZEPH_AGENT_MAX_TOOL_ITERATIONS` env var (#245)
- Doom-loop detection: breaks agent loop on 3 consecutive identical tool outputs
- Context budget check at 80% threshold stops iteration before context overflow
- `IndexWatcher` for incremental code index updates on file changes via `notify` file watcher (#233)
- `watch` config field in `[index]` section (default `true`) to enable/disable file watching
- Repo map cache with configurable TTL (`repo_map_ttl_secs`, default 300s) to avoid per-message filesystem traversal (#231)
- Cross-session memory score threshold (`cross_session_score_threshold`, default 0.35) to filter low-relevance results (#232)
- `embed_missing()` called on startup for embedding backfill when Qdrant available (#261)
- `AgentTaskProcessor` replaces `EchoTaskProcessor` for real A2A inference (#262)

### Changed
- ShellExecutor uses PermissionPolicy for all permission checks instead of legacy find_blocked_command/find_confirm_command
- Replaced unmaintained dirs-next 2.0 with dirs 6.x
- Batch messages retrieval in semantic recall: replaced N+1 query pattern with `messages_by_ids()` for improved performance

### Fixed
- Persist `MessagePart` data to SQLite via `remember_with_parts()` — pruning state now survives session restarts (#229)
- Clear tool output body from memory after Tier 1 pruning to reclaim heap (#230)
- TUI uptime display now updates from agent start time instead of always showing 0s (#259)
- `FileExecutor` `handle_write` now uses canonical path for security (TOCTOU prevention) (#260)
- `resolve_via_ancestors` trailing slash bug on macOS
- `vault.backend` from config now used as default backend; CLI `--vault` flag overrides config (#263)
- A2A error responses sanitized to prevent provider URL leakage

## [0.9.4] - 2026-02-14

### Added
- Bounded FIFO message queue (max 10) in agent loop: users can submit messages during inference, queued messages are delivered sequentially when response cycle completes
- Channel trait extended with `try_recv()` (non-blocking poll) and `send_queue_count()` with default no-op impls
- Consecutive user messages within 500ms merge window joined by newline
- TUI queue badge `[+N queued]` in input area, `Ctrl+K` to clear queue, `/clear-queue` command
- TelegramChannel `try_recv()` implementation via mpsc
- Deferred model warmup in TUI mode: interface renders immediately, Ollama warmup runs in background with status indicator ("warming up model..." → "model ready"), agent loop awaits completion via `watch::channel`
- `context_tokens` metric in TUI Resources panel showing current prompt estimate (vs cumulative session totals)
- `unsummarized_message_count` in `SemanticMemory` for precise summarization trigger
- `count_messages_after` in `SqliteStore` for counting messages beyond a given ID
- TUI status indicators for context compaction ("compacting context...") and summarization ("summarizing...")
- Debug tracing in `should_compact()` for context budget diagnostics (token estimate, threshold, decision)
- Config hot-reload: watch config file for changes via `notify_debouncer_mini` and apply runtime-safe fields (security, timeouts, memory limits, context budget, compaction, max_active_skills) without restart
- `ConfigWatcher` in zeph-core with 500ms debounced filesystem monitoring
- `with_config_reload()` builder method on Agent for wiring config file watcher
- `tool_name` field in `ToolOutput` for identifying tool type (bash, mcp, web-scrape) in persisted messages and TUI display
- Real-time status events for provider retries and orchestrator fallbacks surfaced as `[system]` messages across all channels (CLI stderr, TUI chat panel, Telegram)
- `StatusTx` type alias in `zeph-llm` for emitting status events from providers
- `Status` variant in TUI `AgentEvent` rendered as System-role messages (DarkGray)
- `set_status_tx()` on `AnyProvider`, `SubProvider`, and `ModelOrchestrator` for propagating status sender through the provider hierarchy
- Background forwarding tasks for immediate status delivery (bypasses agent loop for zero-latency display)
- TUI: toggle side panels with `d` key in Normal mode
- TUI: input history navigation (Up/Down in Insert mode)
- TUI: message separators and accent bars for visual structure
- TUI: tool output restored as expandable messages from conversation history
- TUI: collapsed tool output preview (3 lines) when restoring history
- `LlmProvider::context_window()` trait method for model context window size detection
- Ollama context window auto-detection via `/api/show` model info endpoint
- Context window sizes for Claude (200K) and OpenAI (128K/16K/1M) provider models
- `auto_budget` config field with `ZEPH_MEMORY_AUTO_BUDGET` env override for automatic context budget from model metadata
- `inject_summaries()` in Agent: injects SQLite conversation summaries into context (newest-first, budget-aware, with deduplication)
- Wire `zeph-index` Code RAG pipeline into agent loop (feature-gated `index`): `CodeRetriever` integration, `inject_code_rag()` in `prepare_context()`, repo map in system prompt, background project indexing on startup
- `IndexConfig` with `[index]` TOML section and `ZEPH_INDEX_*` env overrides (enabled, max_chunks, score_threshold, budget_ratio, repo_map_tokens)
- Two-tier context pruning strategy for granular token reclamation before full LLM compaction
  - Tier 1: selective `ToolOutput` part pruning with `compacted_at` timestamp on pruned parts
  - Tier 2: LLM-based compaction fallback when tier 1 is insufficient
  - `prune_protect_tokens` config field for token-based protection zone (shields recent context from pruning)
  - `tool_output_prunes` metric tracking tier 1 pruning operations
  - `compacted_at` field on `MessagePart::ToolOutput` for pruning audit trail
- `MessagePart` enum (Text, ToolOutput, Recall, CodeContext, Summary) for typed message content with independent lifecycle
- `Message::from_parts()` constructor with `to_llm_content()` flattening for LLM provider consumption
- `Message::from_legacy()` backward-compatible constructor for simple text messages
- SQLite migration 006: `parts` column for structured message storage (JSON-serialized)
- `save_message_with_parts()` in SqliteStore for persisting typed message parts
- inject_semantic_recall, inject_code_context, inject_summaries now create typed MessagePart variants

### Changed
- `index` feature enabled by default (Code RAG pipeline active out of the box)
- Agent error handler shows specific error context instead of generic message
- TUI inline code rendered as blue with dark background glow instead of bright yellow
- TUI header uses deep blue background (`Rgb(20, 40, 80)`) for improved contrast
- System prompt includes explicit `bash` block example and bans invented formats (`tool_code`, `tool_call`) for small model compatibility
- TUI Resources panel: replaced separate Prompt/Completion/Total with Context (current) and Session (cumulative) metrics
- Summarization trigger uses unsummarized message count instead of total, avoiding repeated no-op checks
- Empty `AgentEvent::Status` clears TUI spinner instead of showing blank throbber
- Status label cleared after summarization and compaction complete
- Default `summarization_threshold`: 100 → 50 messages
- Default `compaction_threshold`: 0.75 → 0.80
- Default `compaction_preserve_tail`: 4 → 6 messages
- Default `semantic.enabled`: false → true
- Default `summarize_output`: false → true
- Default `context_budget_tokens`: 0 (auto-detect from model)

### Fixed
- TUI chat line wrapping no longer eats 2 characters on word wrap (accent prefix width accounted for)
- TUI activity indicator moved to dedicated layout row (no longer overlaps content)
- Memory history loading now retrieves most recent messages instead of oldest
- Persisted tool output format includes tool name (`[tool output: bash]`) for proper display on restore
- `summarize_output` serde deserialization used `#[serde(default)]` yielding `false` instead of config default `true`

## [0.9.3] - 2026-02-12

### Added
- New `zeph-index` crate: AST-based code indexing and semantic retrieval pipeline
  - Language detection and grammar registry with feature-gated tree-sitter grammars (Rust, Python, JavaScript, TypeScript, Go, Bash, TOML, JSON, Markdown)
  - AST-based chunker with cAST-inspired greedy sibling merge and recursive decomposition (target 600 non-ws chars per chunk)
  - Contextualized embedding text generation for improved retrieval quality
  - Dual-write storage layer (Qdrant vector search + SQLite metadata) with INT8 scalar quantization
  - Incremental indexer with .gitignore-aware file walking and content-hash change detection
  - Hybrid retriever with query classification (Semantic/Grep/Hybrid) and budget-aware result packing
  - Lightweight repo map generation (tree-sitter signature extraction, budget-constrained output)
- `code_context` slot in `BudgetAllocation` for code RAG injection into agent context
- `inject_code_context()` method in Agent for transient code chunk injection before semantic recall

## [0.9.2] - 2026-02-12

### Added
- Runtime context compaction for long sessions: automatic LLM-based summarization of middle messages when context usage exceeds configurable threshold (default 75%)
- `with_context_budget()` builder method on Agent for wiring context budget and compaction settings
- Config fields: `compaction_threshold` (f32), `compaction_preserve_tail` (usize) with env var overrides
- `context_compactions` counter in MetricsSnapshot for observability
- Context budget integration: `ContextBudget::allocate()` wired into agent loop via `prepare_context()` orchestrator
- Semantic recall injection: `SemanticMemory::recall()` results injected as transient system messages with token budget control
- Message history trimming: oldest non-system messages evicted when history exceeds budget allocation
- Environment context injection: working directory, OS, git branch, and model name in system prompt via `<environment>` block
- Extended BASE_PROMPT with structured Tool Use, Guidelines, and Security sections
- Tool output truncation: head+tail split at 30K chars with UTF-8 safe boundaries
- Smart tool output summarization: optional LLM-based summarization for outputs exceeding 30K chars, with fallback to truncation on failure (disabled by default via `summarize_output` config)
- Progressive skill loading: matched skills get full body, remaining shown as description-only catalog via `<other_skills>`
- ZEPH.md project config discovery: walk up directory tree, inject into system prompt as `<project_context>`

## [0.9.1] - 2026-02-12

### Added
- Mouse scroll support for TUI chat widget (scroll up/down via mouse wheel)
- Splash screen with colored block-letter "ZEPH" banner on TUI startup
- Conversation history loading into chat on TUI startup
- Model thinking block rendering (`<think>` tags from Ollama DeepSeek/Qwen models) in distinct darker style
- Markdown rendering for all chat messages via `pulldown-cmark`: bold, italic, strikethrough, headings, code blocks, inline code, lists, blockquotes, horizontal rules
- Scrollbar track with proportional thumb indicator in chat widget

### Fixed
- Chat messages no longer overflow below the viewport when lines wrap
- Scroll no longer sticks at top after over-scrolling past content boundary

## [0.9.0] - 2026-02-12

### Added
- ratatui-based TUI dashboard with real-time agent metrics (feature-gated `tui`, opt-in)
- `TuiChannel` as new `Channel` implementation with bottom-up chat feed, input line, and status bar
- `MetricsSnapshot` and `MetricsCollector` in zeph-core via `tokio::sync::watch` for live metrics transport
- `with_metrics()` builder on Agent with instrumentation at 8 collection points: api_calls, latency, prompt/completion tokens, active skills, sqlite message count, qdrant status, summarization count
- Side panel widgets (skills, memory, resources) with live data from agent loop
- Confirmation modal dialog for destructive command approval in TUI (Y/Enter confirms, N/Escape cancels)
- Scroll indicators (▲/▼) in chat widget when content overflows viewport
- Responsive layout: side panels hidden on terminals narrower than 80 columns
- Multiline input via Shift+Enter in TUI insert mode
- Bottom-up chat layout with proper newline handling and per-message visual separation
- Panic hook for terminal state restoration on any panic during TUI execution
- Unicode-safe char-index cursor tracking for multi-byte input in TUI
- `--config <path>` CLI argument and `ZEPH_CONFIG` env var to override default config path
- OpenAI-compatible LLM provider with chat, streaming, and embeddings support
- Feature-gated `openai` feature (enabled by default)
- Support for OpenAI, Together AI, Groq, Fireworks, and any OpenAI-compatible API via configurable `base_url`
- `reasoning_effort` parameter for OpenAI reasoning models (low/medium/high)
- `/mcp add <id> <command> [args...]` for dynamic stdio MCP server connection at runtime
- `/mcp add <id> <url>` for HTTP transport (remote MCP servers in Docker/cloud)
- `/mcp list` command to show connected servers and tool counts
- `/mcp remove <id>` command to disconnect MCP servers
- `McpTransport` enum: `Stdio` (child process) and `Http` (Streamable HTTP) transports
- HTTP MCP server config via `url` field in `[[mcp.servers]]`
- `mcp.allowed_commands` config for command allowlist (security hardening)
- `mcp.max_dynamic_servers` config to limit concurrent dynamic servers (default 10)
- Qdrant registry sync after dynamic MCP add/remove for semantic tool matching

### Changed
- Docker images now include Node.js, npm, and Python 3 for MCP server runtime
- `ServerEntry` uses `McpTransport` enum instead of flat command/args/env fields

### Fixed
- Effective embedding model resolution: Qdrant subsystems now use the correct provider-specific embedding model name when provider is `openai` or orchestrator routes to OpenAI
- Skill watcher no longer loops in Docker containers (overlayfs phantom events)

## [0.8.2] - 2026-02-10

### Changed
- Enable all non-platform features by default: `orchestrator`, `self-learning`, `mcp`, `vault-age`, `candle`
- Features `metal` and `cuda` remain opt-in (platform-specific GPU accelerators)
- CI clippy uses default features instead of explicit feature list
- Docker images now include skill runtime dependencies: `curl`, `wget`, `git`, `jq`, `file`, `findutils`, `procps-ng`

## [0.8.1] - 2026-02-10

### Added
- Shell sandbox: configurable `allowed_paths` directory allowlist and `allow_network` toggle blocking curl/wget/nc in `ShellExecutor` (Issue #91)
- Sandbox validation before every shell command execution with path canonicalization
- `tools.shell.allowed_paths` config (empty = working directory only) with `ZEPH_TOOLS_SHELL_ALLOWED_PATHS` env override
- `tools.shell.allow_network` config (default: true) with `ZEPH_TOOLS_SHELL_ALLOW_NETWORK` env override
- Interactive confirmation for destructive commands (`rm`, `git push -f`, `DROP TABLE`, etc.) with CLI y/N prompt and Telegram inline keyboard (Issue #92)
- `tools.shell.confirm_patterns` config with default destructive command patterns
- `Channel::confirm()` trait method with default auto-confirm for headless/test scenarios
- `ToolError::ConfirmationRequired` and `ToolError::SandboxViolation` variants
- `execute_confirmed()` method on `ToolExecutor` for confirmation bypass after user approval
- A2A TLS enforcement: reject HTTP endpoints when `a2a.require_tls = true` (Issue #92)
- A2A SSRF protection: block private IP ranges (RFC 1918, loopback, link-local) with DNS resolution (Issue #92)
- Configurable A2A server payload size limit via `a2a.max_body_size` (default: 1 MiB)
- Structured JSON audit logging for all tool executions with stdout or file destination (Issue #93)
- `AuditLogger` with `AuditEntry` (timestamp, tool, command, result, duration) and `AuditResult` enum
- `[tools.audit]` config section with `ZEPH_TOOLS_AUDIT_ENABLED` and `ZEPH_TOOLS_AUDIT_DESTINATION` env overrides
- Secret redaction in LLM responses: detect API keys, tokens, passwords, private keys and replace with `[REDACTED]` (Issue #93)
- Whitespace-preserving `redact_secrets()` scanner with zero-allocation fast path via `Cow<str>`
- `[security]` config section with `redact_secrets` toggle (default: true)
- Configurable timeout policies for LLM, embedding, and A2A operations (Issue #93)
- `[timeouts]` config section with `llm_seconds`, `embedding_seconds`, `a2a_seconds`
- LLM calls wrapped with `tokio::time::timeout` in agent loop

## [0.8.0] - 2026-02-10

### Added
- `VaultProvider` trait with pluggable secret backends, `Secret` newtype with redacted debug output, `EnvVaultProvider` for environment variable secrets (Issue #70)
- `AgeVaultProvider`: age-encrypted JSON vault backend with x25519 identity key decryption (Issue #70)
- `Config::resolve_secrets()`: async secret resolution through vault provider for API keys and tokens
- CLI vault args: `--vault <backend>`, `--vault-key <path>`, `--vault-path <path>`
- `vault-age` feature flag on `zeph-core` and root binary
- `[vault]` config section with `backend` field (default: `env`)
- `docker-compose.vault.yml` overlay for containerized age vault deployment
- `CARGO_FEATURES` build arg in `Dockerfile.dev` for optional feature flags
- `CandleProvider`: local GGUF model inference via candle ML framework with chat templates (Llama3, ChatML, Mistral, Phi3, Raw), token generation with top-k/top-p sampling, and repeat penalty (Issue #125)
- `CandleProvider` embeddings: BERT-based embedding model loaded from HuggingFace Hub with mean pooling and L2 normalization (Issue #126)
- `ModelOrchestrator`: task-aware multi-model routing with keyword-based classification (coding, creative, analysis, translation, summarization, general) and provider fallback chains (Issue #127)
- `SubProvider` enum breaking recursive type cycle between `AnyProvider` and `ModelOrchestrator`
- Device auto-detection: Metal on macOS, CUDA on Linux with GPU, CPU fallback (Issue #128)
- Feature flags: `candle`, `metal`, `cuda`, `orchestrator` on workspace and zeph-llm crate
- `CandleConfig`, `GenerationParams`, `OrchestratorConfig` in zeph-core config
- Config examples for candle and orchestrator in `config/default.toml`
- Setup guide sections for candle local inference and model orchestrator
- 15 new unit tests for orchestrator, chat templates, generation config, and loader
- Progressive skill loading: lazy body loading via `OnceLock`, on-demand resource resolution for `scripts/`, `references/`, `assets/` directories, extended frontmatter (`compatibility`, `license`, `metadata`, `allowed-tools`), skill name validation per agentskills.io spec (Issue #115)
- `SkillMeta`/`Skill` composition pattern: metadata loaded at startup, body deferred until skill activation
- `SkillRegistry` replaces `Vec<Skill>` in Agent — lazy body access via `get_skill()`/`get_body()`
- `resource.rs` module: `discover_resources()` + `load_resource()` with path traversal protection via canonicalization
- Self-learning skill evolution system: automatic skill improvement through failure detection, self-reflection retry, and LLM-generated version updates (Issue #107)
- `SkillOutcome` enum and `SkillMetrics` for skill execution outcome tracking (Issue #108)
- Agent self-reflection retry on tool failure with 1-retry-per-message budget (Issue #109)
- Skill version generation and storage in SQLite with auto-activate and manual approval modes (Issue #110)
- Automatic rollback when skill version success rate drops below threshold (Issue #111)
- `/skill stats`, `/skill versions`, `/skill activate`, `/skill approve`, `/skill reset` commands for version management (Issue #111)
- `/feedback` command for explicit user feedback on skill quality (Issue #112)
- `LearningConfig` with TOML config section `[skills.learning]` and env var overrides
- `self-learning` feature flag on `zeph-skills`, `zeph-core`, and root binary
- SQLite migration 005: `skill_versions` and `skill_outcomes` tables
- Bundled `setup-guide` skill with configuration reference for all env vars, TOML keys, and operating modes
- Bundled `skill-audit` skill for spec compliance and security review of installed skills
- `allowed_commands` shell config to override default blocklist entries via `ZEPH_TOOLS_SHELL_ALLOWED_COMMANDS`
- `QdrantSkillMatcher`: persistent skill embeddings in Qdrant with BLAKE3 content-hash delta sync (Issue #104)
- `SkillMatcherBackend` enum dispatching between `InMemory` and `Qdrant` skill matching (Issue #105)
- `qdrant` feature flag on `zeph-skills` crate gating all Qdrant dependencies
- Graceful fallback to in-memory matcher when Qdrant is unavailable
- Skill matching tracing via `tracing::debug!` for diagnostics
- New `zeph-mcp` crate: MCP client via rmcp 0.14 with stdio transport (Issue #117)
- `McpClient` and `McpManager` for multi-server lifecycle management with concurrent connections
- `McpToolExecutor` implementing `ToolExecutor` for `` ```mcp `` block execution (Issue #120)
- `McpToolRegistry`: MCP tool embeddings in Qdrant `zeph_mcp_tools` collection with BLAKE3 delta sync (Issue #118)
- Unified matching: skills + MCP tools injected into system prompt by relevance (Issue #119)
- `mcp` feature flag on root binary and zeph-core gating all MCP functionality
- Bundled `mcp-generate` skill with instructions for MCP-to-skill generation via mcp-execution (Issue #121)
- `[[mcp.servers]]` TOML config section for MCP server connections

### Changed
- `Skill` struct refactored: split into `SkillMeta` (lightweight metadata) + `Skill` (meta + body), composition pattern
- `SkillRegistry` now uses `OnceLock<String>` for lazy body caching instead of eager loading
- Matcher APIs accept `&[&SkillMeta]` instead of `&[Skill]` — embeddings use description only
- `Agent` stores `SkillRegistry` directly instead of `Vec<Skill>`
- `Agent` field `matcher` type changed from `Option<SkillMatcher>` to `Option<SkillMatcherBackend>`
- Skill matcher creation extracted to `create_skill_matcher()` in `main.rs`

### Dependencies
- Added `age` 0.11.2 to workspace (optional, behind `vault-age` feature, `default-features = false`)
- Added `candle-core` 0.9, `candle-nn` 0.9, `candle-transformers` 0.9 to workspace (optional, behind `candle` feature)
- Added `hf-hub` 0.4 to workspace (HuggingFace model downloads with rustls-tls)
- Added `tokenizers` 0.22 to workspace (BPE tokenization with fancy-regex)
- Added `blake3` 1.8 to workspace
- Added `rmcp` 0.14 to workspace (MCP protocol SDK)

## [0.7.1] - 2026-02-09

### Added
- `WebScrapeExecutor`: safe HTML scraping via scrape-core with CSS selectors, SSRF protection, and HTTPS-only enforcement (Issue #57)
- `CompositeExecutor<A, B>`: generic executor chaining with first-match-wins dispatch
- Bundled `web-scrape` skill with CSS selector examples for structured data extraction
- `extract_fenced_blocks()` shared utility for fenced code block parsing (DRY refactor)
- `[tools.scrape]` config section with timeout and max body size settings

### Changed
- Agent tool output label from `[shell output]` to `[tool output]`
- `ShellExecutor` block extraction now uses shared `extract_fenced_blocks()`

## [0.7.0] - 2026-02-08

### Added
- A2A Server: axum-based HTTP server with JSON-RPC 2.0 routing for `message/send`, `tasks/get`, `tasks/cancel` (Issue #83)
- In-memory `TaskManager` with full task lifecycle: create, get, update status, add artifacts, append history, cancel (Issue #83)
- SSE streaming endpoint (`/a2a/stream`) with JSON-RPC response envelope wrapping per A2A spec (Issue #84)
- Bearer token authentication middleware with constant-time comparison via `subtle::ConstantTimeEq` (Issue #85)
- Per-IP rate limiting middleware with configurable 60-second sliding window (Issue #85)
- Request body size limit (1 MiB) via `tower-http::limit::RequestBodyLimitLayer` (Issue #85)
- `A2aServerConfig` with env var overrides: `ZEPH_A2A_ENABLED`, `ZEPH_A2A_HOST`, `ZEPH_A2A_PORT`, `ZEPH_A2A_PUBLIC_URL`, `ZEPH_A2A_AUTH_TOKEN`, `ZEPH_A2A_RATE_LIMIT`
- Agent card served at `/.well-known/agent-card.json` (public, no auth required)
- Graceful shutdown integration via tokio watch channel
- Server module gated behind `server` feature flag on `zeph-a2a` crate

### Changed
- `Part` type refactored from flat struct to tagged enum with `kind` discriminator (`text`, `file`, `data`) per A2A spec
- `TaskState::Pending` renamed to `TaskState::Submitted` with explicit per-variant `#[serde(rename)]` for kebab-case wire format
- Added `AuthRequired` and `Unknown` variants to `TaskState`
- `TaskStatusUpdateEvent` and `TaskArtifactUpdateEvent` gained `kind` field (`status-update`, `artifact-update`)

## [0.6.0] - 2026-02-08

### Added
- New `zeph-a2a` crate: A2A protocol implementation for agent-to-agent communication (Issue #78)
- A2A protocol types: `Task`, `TaskState`, `TaskStatus`, `Message`, `Part`, `Artifact`, `AgentCard`, `AgentSkill`, `AgentCapabilities` with full serde camelCase serialization (Issue #79)
- JSON-RPC 2.0 envelope types (`JsonRpcRequest`, `JsonRpcResponse`, `JsonRpcError`) with method constants for A2A operations (Issue #79)
- `AgentCardBuilder` for constructing A2A agent cards from runtime config and skills (Issue #79)
- `AgentRegistry` with well-known URI discovery (`/.well-known/agent.json`), TTL-based caching, and manual registration (Issue #80)
- `A2aClient` with `send_message`, `stream_message` (SSE), `get_task`, `cancel_task` via JSON-RPC 2.0 (Issue #81)
- Bearer token authentication support for all A2A client operations (Issue #81)
- SSE streaming via `eventsource-stream` with `TaskEvent` enum (`StatusUpdate`, `ArtifactUpdate`) (Issue #81)
- `A2aError` enum with variants for HTTP, JSON, JSON-RPC, discovery, and stream errors (Issue #79)
- Optional `a2a` feature flag (enabled by default) to gate A2A functionality
- 42 new unit tests for protocol types, JSON-RPC envelopes, agent card builder, discovery registry, and client operations

## [0.5.0] - 2026-02-08

### Added
- Embedding-based skill matcher: `SkillMatcher` with cosine similarity selects top-K relevant skills per query instead of injecting all skills into the system prompt (Issue #75)
- `max_active_skills` config field (default: 5) with `ZEPH_SKILLS_MAX_ACTIVE` env var override
- Skill hot-reload: filesystem watcher via `notify-debouncer-mini` detects SKILL.md changes and re-embeds without restart (Issue #76)
- Skill priority: earlier paths in `skills.paths` take precedence when skills share the same name (Issue #76)
- `SkillRegistry::reload()` and `SkillRegistry::into_skills()` methods
- SQLite `skill_usage` table tracking per-skill invocation counts and last-used timestamps (Issue #77)
- `/skills` command displaying available skills with usage statistics
- Three new bundled skills: `git`, `docker`, `api-request` (Issue #77)
- 17 new unit tests for matcher, registry priority, reload, and usage tracking

### Changed
- `Agent::new()` signature: accepts `Vec<Skill>`, `Option<SkillMatcher>`, `max_active_skills` instead of pre-formatted skills prompt string
- `format_skills_prompt` now generic over `Borrow<Skill>` to accept both `&[Skill]` and `&[&Skill]`
- `Skill` struct derives `Clone`
- `Agent` generic constraint: `P: LlmProvider + Clone + 'static` (required for embed_fn closures)
- System prompt rebuilt dynamically per user query with only matched skills

### Dependencies
- Added `notify` 8.0, `notify-debouncer-mini` 0.6
- `zeph-core` now depends on `zeph-skills`
- `zeph-skills` now depends on `tokio` (sync, rt) and `notify`

## [0.4.3] - 2026-02-08

### Fixed
- Telegram "Bad Request: text must be non-empty" error when LLM returns whitespace-only content. Added `is_empty()` guard after `markdown_to_telegram` conversion in both `send()` and `send_or_edit()` (Issue #73)

### Added
- `Dockerfile.dev`: multi-stage build from source with cargo registry/build cache layers for fast rebuilds
- `docker-compose.dev.yml`: full dev stack (Qdrant + Zeph) with debug tracing (`RUST_LOG`, `RUST_BACKTRACE=1`), uses host Ollama via `host.docker.internal`
- `docker-compose.deps.yml`: Qdrant-only compose for native zeph execution on macOS

## [0.4.2] - 2026-02-08

### Fixed
- Telegram MarkdownV2 parsing errors (Issue #69). Replaced manual character-by-character escaping with AST-based event-driven rendering using pulldown-cmark 0.13.0
- UTF-8 safe text chunking for messages exceeding Telegram's 4096-byte limit. Uses `str::is_char_boundary()` with newline preference to prevent splitting multi-byte characters (emoji, CJK)
- Link URL over-escaping. Dedicated `escape_url()` method only escapes `)` and `\` per Telegram MarkdownV2 spec, fixing broken URLs like `https://example\.com`

### Added
- `TelegramRenderer` state machine for context-aware escaping: 19 special characters in text, only `\` and `` ` `` in code blocks
- Markdown formatting support: bold, italic, strikethrough, headers, code blocks, links, lists, blockquotes
- Comprehensive benchmark suite with criterion: 7 scenario groups measuring latency (2.83µs for 500 chars) and throughput (121-970 MiB/s)
- Memory profiling test to measure escaping overhead (3-20% depending on content)
- 30 markdown unit tests covering formatting, escaping, edge cases, and UTF-8 chunking (99.32% line coverage)

### Changed
- `crates/zeph-channels/src/markdown.rs`: Complete rewrite with pulldown-cmark event-driven parser (449 lines)
- `crates/zeph-channels/src/telegram.rs`: Removed `has_unclosed_code_block()` pre-flight check (no longer needed with AST parsing), integrated UTF-8 safe chunking
- Dependencies: Added pulldown-cmark 0.13.0 (MIT) and criterion 0.8.0 (Apache-2.0/MIT) for benchmarking

## [0.4.1] - 2026-02-08

### Fixed
- Auto-create Qdrant collection on first use. Previously, the `zeph_conversations` collection had to be manually created using curl commands. Now, `ensure_collection()` is called automatically before all Qdrant operations (remember, recall, summarize) to initialize the collection with correct vector dimensions (896 for qwen3-embedding) and Cosine distance metric on first access, similar to SQL migrations.

### Changed
- Docker Compose: Added environment variables for semantic memory configuration (`ZEPH_MEMORY_SEMANTIC_ENABLED`, `ZEPH_MEMORY_SEMANTIC_RECALL_LIMIT`) and Qdrant URL override (`ZEPH_QDRANT_URL`) to enable full semantic memory stack via `.env` file

## [0.4.0] - 2026-02-08

### Added

#### M9 Phase 3: Conversation Summarization and Context Budget (Issue #62)
- New `SemanticMemory::summarize()` method for LLM-based conversation compression
- Automatic summarization triggered when message count exceeds threshold
- SQLite migration `003_summaries.sql` creates dedicated summaries table with CASCADE constraints
- `SqliteStore::save_summary()` stores summary with metadata (first/last message IDs, token estimate)
- `SqliteStore::load_summaries()` retrieves all summaries for a conversation ordered by ID
- `SqliteStore::load_messages_range()` fetches messages after specific ID with limit for batch processing
- `SqliteStore::count_messages()` counts total messages in conversation
- `SqliteStore::latest_summary_last_message_id()` gets last summarized message ID for resumption
- `ContextBudget` struct for proportional token allocation (15% summaries, 25% semantic recall, 60% recent history)
- `estimate_tokens()` helper using chars/4 heuristic (100x faster than tiktoken, ±25% accuracy)
- `Agent::check_summarization()` lazy trigger after persist_message() when threshold exceeded
- Batch size = threshold/2 to balance summary quality with LLM call frequency
- Configuration: `memory.summarization_threshold` (default: 100), `memory.context_budget_tokens` (default: 0 = unlimited)
- Environment overrides: `ZEPH_MEMORY_SUMMARIZATION_THRESHOLD`, `ZEPH_MEMORY_CONTEXT_BUDGET_TOKENS`
- Inline comments in `config/default.toml` documenting all configuration parameters
- 26 new unit tests for summarization and context budget (196 total tests, 75.31% coverage)
- Architecture Decision Records ADR-016 through ADR-019 for summarization design
- Foreign key constraint added to `messages.conversation_id` with ON DELETE CASCADE

#### M9 Phase 2: Semantic Memory Integration (Issue #61)
- `SemanticMemory<P: LlmProvider>` orchestrator coordinating SQLite, Qdrant, and LlmProvider
- `SemanticMemory::remember()` saves message to SQLite, generates embedding, stores in Qdrant
- `SemanticMemory::recall()` performs semantic search with query embedding and fetches messages from SQLite
- `SemanticMemory::has_embedding()` checks if message already embedded to prevent duplicates
- `SemanticMemory::embed_missing()` background task to embed old messages (with LIMIT parameter)
- `Agent<P, C, T>` now generic over LlmProvider to support SemanticMemory
- `Agent::with_memory()` replaces SqliteStore with SemanticMemory
- Graceful degradation: embedding failures logged but don't block message save
- Qdrant connection failures silently downgrade to SQLite-only mode (no semantic recall)
- Generic provider pattern: `SemanticMemory<P: LlmProvider>` instead of `Arc<dyn LlmProvider>` for Edition 2024 async trait compatibility
- `AnyProvider`, `OllamaProvider`, `ClaudeProvider` now derive/implement `Clone` for semantic memory integration
- Integration test updated for SemanticMemory API (with_memory now takes 5 parameters including recall_limit)
- Semantic memory config: `memory.semantic.enabled`, `memory.semantic.recall_limit` (default: 5)
- 18 new tests for semantic memory orchestration (recall, remember, embed_missing, graceful degradation)

#### M9 Phase 1: Qdrant Integration (Issue #60)
- New `QdrantStore` module in zeph-memory for vector storage and similarity search
- `QdrantStore::store()` persists embeddings to Qdrant and tracks metadata in SQLite
- `QdrantStore::search()` performs cosine similarity search with filtering by conversation_id and role
- `QdrantStore::has_embedding()` checks if message has associated embedding
- `QdrantStore::ensure_collection()` idempotently creates Qdrant collection with 768-dimensional vectors
- SQLite migration `002_embeddings_metadata.sql` for embedding metadata tracking
- `embeddings_metadata` table with foreign key constraint to messages (ON DELETE CASCADE)
- PRAGMA foreign_keys enabled in SqliteStore via SqliteConnectOptions
- `SearchFilter` and `SearchResult` types for flexible query construction
- `MemoryConfig.qdrant_url` field with `ZEPH_QDRANT_URL` environment variable override (default: http://localhost:6334)
- Docker Compose Qdrant service (qdrant/qdrant:v1.13.6) on ports 6333/6334 with persistent storage
- Integration tests for Qdrant operations (ignored by default, require running Qdrant instance)
- Unit tests for SQLite metadata operations with 98% coverage
- 12 new tests total (3 unit + 2 integration for QdrantStore, 1 CASCADE DELETE test for SqliteStore, 3 config tests)

#### M8: Embeddings support (Issue #54)
- `LlmProvider` trait extended with `embed(&str) -> Result<Vec<f32>>` for generating text embeddings
- `LlmProvider` trait extended with `supports_embeddings() -> bool` for capability detection
- `OllamaProvider` implements embeddings via ollama-rs `generate_embeddings()` API
- Default embedding model: `qwen3-embedding` (configurable via `llm.embedding_model`)
- `ZEPH_LLM_EMBEDDING_MODEL` environment variable for runtime override
- `ClaudeProvider::embed()` returns descriptive error (Claude API does not support embeddings)
- `AnyProvider` delegates embedding methods to active provider
- 10 new tests: unit tests for all providers, config tests for defaults/parsing/env override
- Integration test for real Ollama embedding generation (ignored by default)
- README documentation: model compatibility notes and `ollama pull` instructions for both LLM and embedding models
- Docker Compose configuration: added `ZEPH_LLM_EMBEDDING_MODEL` environment variable

### Changed

**BREAKING CHANGES** (pre-1.0.0):
- `SqliteStore::save_message()` now returns `Result<i64>` instead of `Result<()>` to enable embedding workflow
- `SqliteStore::new()` uses `sqlx::migrate!()` macro instead of INIT_SQL constant for proper migration management
- `QdrantStore::store()` requires `model: &str` parameter for multi-model support
- Config constant `LLM_ENV_KEYS` renamed to `ENV_KEYS` to reflect inclusion of non-LLM variables

**Migration:**
```rust
// Before:
let _ = store.save_message(conv_id, "user", "hello").await?;

// After:
let message_id = store.save_message(conv_id, "user", "hello").await?;
```

- `OllamaProvider::new()` now accepts `embedding_model` parameter (breaking change, pre-v1.0)
- Config schema: added `llm.embedding_model` field with serde default for backward compatibility

## [0.3.0] - 2026-02-07

### Added

#### M7 Phase 1: Tool Execution Framework - zeph-tools crate (Issue #39)
- New `zeph-tools` leaf crate for tool execution abstraction following ADR-014
- `ToolExecutor` trait with native async (Edition 2024 RPITIT): accepts full LLM response, returns `Option<ToolOutput>`
- `ShellExecutor` implementation with bash block parser and execution (30s timeout via `tokio::time::timeout`)
- `ToolOutput` struct with summary string and blocks_executed count
- `ToolError` enum with Blocked/Timeout/Execution variants (thiserror)
- `ToolsConfig` and `ShellConfig` configuration types with serde Deserialize and sensible defaults
- Workspace version consolidation: `version.workspace = true` across all crates
- Workspace inter-crate dependency references: `zeph-llm.workspace = true` pattern for all internal dependencies
- 22 unit tests with 99.25% line coverage, zero clippy warnings
- ADR-014: zeph-tools crate design rationale and architecture decisions

#### M7 Phase 2: Command safety (Issue #40)
- DEFAULT_BLOCKED patterns: 12 dangerous commands (rm -rf /, sudo, mkfs, dd if=, curl, wget, nc, ncat, netcat, shutdown, reboot, halt)
- Case-insensitive command filtering via to_lowercase() normalization
- Configurable timeout and blocked_commands in TOML via `[tools.shell]` section
- Custom blocked commands additive to defaults (cannot weaken security)
- 35+ comprehensive unit tests covering exact match, prefix match, multiline, case variations
- ToolsConfig integration with core Config struct

#### M7 Phase 3: Agent integration (Issue #41)
- Agent now uses `ShellExecutor` for all bash command execution with safety checks
- SEC-001 CRITICAL vulnerability fixed: unfiltered bash execution removed from agent.rs
- Removed 66 lines of duplicate code (extract_bash_blocks, execute_bash, extract_and_execute_bash)
- ToolError::Blocked properly handled with user-facing error message
- Four integration tests for blocked command behavior and error handling
- Performance validation: < 1% overhead for tool executor abstraction
- Security audit: all acceptance criteria met, zero vulnerabilities

### Security

- **CRITICAL fix for SEC-001**: Shell commands now filtered through ShellExecutor with DEFAULT_BLOCKED patterns (rm -rf /, sudo, mkfs, dd if=, curl, wget, nc, shutdown, reboot, halt). Resolves command injection vulnerability where agent.rs bypassed all security checks via inline bash execution.

### Fixed

- Shell command timeout now respects `config.tools.shell.timeout` (was hardcoded 30s in agent.rs)
- Removed duplicate bash parsing logic from agent.rs (now centralized in zeph-tools)
- Error message pattern leakage: blocked commands now show generic security policy message instead of leaking exact blocked pattern

### Changed

**BREAKING CHANGES** (pre-1.0.0):
- `Agent::new()` signature changed: now requires `tool_executor: T` as 4th parameter where `T: ToolExecutor`
- `Agent` struct now generic over three types: `Agent<P, C, T>` (provider, channel, tool_executor)
- Workspace `Cargo.toml` now defines `version = "0.3.0"` in `[workspace.package]` section
- All crate manifests use `version.workspace = true` instead of explicit versions
- Inter-crate dependencies now reference workspace definitions (e.g., `zeph-llm.workspace = true`)

**Migration:**
```rust
// Before:
let agent = Agent::new(provider, channel, &skills_prompt);

// After:
use zeph_tools::shell::ShellExecutor;
let executor = ShellExecutor::new(&config.tools.shell);
let agent = Agent::new(provider, channel, &skills_prompt, executor);
```

## [0.2.0] - 2026-02-06

### Added

#### M6 Phase 1: Streaming trait extension (Issue #35)
- `LlmProvider::chat_stream()` method returning `Pin<Box<dyn Stream<Item = Result<String>> + Send>>`
- `LlmProvider::supports_streaming()` capability query method
- `Channel::send_chunk()` method for incremental response delivery
- `Channel::flush_chunks()` method for buffered chunk flushing
- `ChatStream` type alias for `Pin<Box<dyn Stream<Item = anyhow::Result<String>> + Send>>`
- Streaming infrastructure in zeph-llm and zeph-core (dependencies: futures-core 0.3, tokio-stream 0.1)

#### M6 Phase 2: Ollama streaming backend (Issue #36)
- Native token-by-token streaming for `OllamaProvider` using `ollama-rs` streaming API
- `OllamaProvider::chat_stream()` implementation via `send_chat_messages_stream()`
- `OllamaProvider::supports_streaming()` now returns `true`
- Stream mapping from `Result<ChatMessageResponse, ()>` to `Result<String, anyhow::Error>`
- Integration tests for streaming happy path and equivalence with non-streaming `chat()` (ignored by default)
- ollama-rs `"stream"` feature enabled in workspace dependencies

#### M6 Phase 3: Claude SSE streaming backend (Issue #37)
- Native token-by-token streaming for `ClaudeProvider` using Anthropic Messages API with Server-Sent Events
- `ClaudeProvider::chat_stream()` implementation via SSE event parsing
- `ClaudeProvider::supports_streaming()` now returns `true`
- SSE event parsing via `eventsource-stream` 0.2.3 library
- Stream pipeline: `bytes_stream() -> eventsource() -> filter_map(parse_sse_event) -> Box::pin()`
- Handles SSE events: `content_block_delta` (text extraction), `error` (mid-stream errors), metadata events (skipped)
- Integration tests for streaming happy path and equivalence with non-streaming `chat()` (ignored by default)
- eventsource-stream dependency added to workspace dependencies
- reqwest `"stream"` feature enabled for `bytes_stream()` support

#### M6 Phase 4: Agent streaming integration (Issue #38)
- Agent automatically uses streaming when `provider.supports_streaming()` returns true (ADR-014)
- `Agent::process_response_streaming()` method for stream consumption and chunk accumulation
- CliChannel immediate streaming: `send_chunk()` prints each chunk instantly via `print!()` + `flush()`
- TelegramChannel batched streaming: debounce at 1 second OR 512 bytes, edit-in-place for progressive updates
- Response buffer pre-allocation with `String::with_capacity(2048)` for performance
- Error message sanitization: full errors logged via `tracing::error!()`, generic messages shown to users
- Telegram edit retry logic: recovers from stale message_id (message deleted, permissions lost)
- tokio-stream dependency added for `StreamExt` trait
- 6 new unit tests for channel streaming behavior

### Fixed

#### M6 Phase 3: Security improvements
- Manual `Debug` implementation for `ClaudeProvider` to prevent API key leakage in debug output
- Error message sanitization: full Claude API errors logged via `tracing::error!()`, generic messages returned to users

### Changed

**BREAKING CHANGES** (pre-1.0.0):
- `LlmProvider` trait now requires `chat_stream()` and `supports_streaming()` implementations (no default implementations per project policy)
- `Channel` trait now requires `send_chunk()` and `flush_chunks()` implementations (no default implementations per project policy)
- All existing providers (`OllamaProvider`, `ClaudeProvider`) updated with fallback implementations (Phase 1 non-streaming: calls `chat()` and wraps in single-item stream)
- All existing channels (`CliChannel`, `TelegramChannel`) updated with no-op implementations (Phase 1: streaming not yet wired into agent loop)

## [0.1.0] - 2026-02-05

### Added

#### M0: Workspace bootstrap
- Cargo workspace with 5 crates: zeph-core, zeph-llm, zeph-skills, zeph-memory, zeph-channels
- Binary entry point with version display
- Default configuration file
- Workspace-level dependency management and lints

#### M1: LLM + CLI agent loop
- LlmProvider trait with Message/Role types
- Ollama backend using ollama-rs
- Config loading from TOML with env var overrides
- Interactive CLI agent loop with multi-turn conversation

#### M2: Skills system
- SKILL.md parser with YAML frontmatter and markdown body (zeph-skills)
- Skill registry that scans directories for `*/SKILL.md` files
- Prompt formatter with XML-like skill injection into system prompt
- Bundled skills: web-search, file-ops, system-info
- Shell execution: agent extracts ```bash``` blocks from LLM responses and runs them
- Multi-step execution loop with 3-iteration limit
- 30-second timeout on shell commands
- Context builder that combines base system prompt with skill instructions

#### M3: Memory + Claude
- SQLite conversation persistence with sqlx (zeph-memory)
- Conversation history loading and message saving per session
- Claude backend via Anthropic Messages API with 429 retry (zeph-llm)
- AnyProvider enum dispatch for runtime provider selection
- CloudLlmConfig for Claude-specific settings (model, max_tokens)
- ZEPH_CLAUDE_API_KEY env var for API authentication
- ZEPH_SQLITE_PATH env var override for database location
- Provider factory in main.rs selecting Ollama or Claude from config
- Memory integration into Agent with optional SqliteStore

#### M4: Telegram channel
- Channel trait abstraction for agent I/O (recv, send, send_typing)
- CliChannel implementation reading stdin/stdout via tokio::task::spawn_blocking
- TelegramChannel adapter using teloxide with mpsc-based message routing
- Telegram user whitelist via `telegram.allowed_users` config
- ZEPH_TELEGRAM_TOKEN env var for Telegram bot activation
- Bot commands: /start (welcome), /reset, /skills forwarded as ChannelMessage
- AnyChannel enum dispatch for runtime channel selection
- zeph-channels crate with teloxide 0.17 dependency
- TelegramConfig in config.rs with TOML and env var support

#### M5: Integration tests + release
- Integration test suite: config, skills, memory, and agent end-to-end
- MockProvider and MockChannel for agent testing without external dependencies
- Graceful shutdown via tokio::sync::watch + tokio::signal (SIGINT/SIGTERM)
- Ollama startup health check (warn-only, non-blocking)
- README with installation, configuration, usage, and skills documentation
- GitHub Actions CI/CD: lint, clippy, test (ubuntu + macos), coverage, security, release
- Dependabot for Cargo and GitHub Actions with auto-merge for patch/minor updates
- Auto-labeler workflow for PRs by path, title prefix, and size
- Release workflow with cross-platform binary builds and checksums
- Issue templates (bug report, feature request)
- PR template with review checklist
- LICENSE (MIT), CONTRIBUTING.md, SECURITY.md

### Fixed
- Replace vulnerable `serde_yml`/`libyml` with manual frontmatter parser (GHSA high + medium)

### Changed
- Move dependency features from workspace root to individual crate manifests
- Update README with badges, architecture overview, and pre-built binaries section

- Agent is now generic over both LlmProvider and Channel (`Agent<P, C>`)
- Agent::new() accepts a Channel parameter instead of reading stdin directly
- Agent::run() uses channel.recv()/send() instead of direct I/O
- Agent calls channel.send_typing() before each LLM request
- Agent::run() uses tokio::select! to race channel messages against shutdown signal

[Unreleased]: https://github.com/bug-ops/zeph/compare/v0.9.9...HEAD
[0.9.9]: https://github.com/bug-ops/zeph/compare/v0.9.8...v0.9.9
[0.9.8]: https://github.com/bug-ops/zeph/compare/v0.9.7...v0.9.8
[0.9.7]: https://github.com/bug-ops/zeph/compare/v0.9.6...v0.9.7
[0.9.6]: https://github.com/bug-ops/zeph/compare/v0.9.5...v0.9.6
[0.9.5]: https://github.com/bug-ops/zeph/compare/v0.9.4...v0.9.5
[0.9.4]: https://github.com/bug-ops/zeph/compare/v0.9.3...v0.9.4
[0.9.3]: https://github.com/bug-ops/zeph/compare/v0.9.2...v0.9.3
[0.9.2]: https://github.com/bug-ops/zeph/compare/v0.9.1...v0.9.2
[0.9.1]: https://github.com/bug-ops/zeph/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/bug-ops/zeph/compare/v0.8.2...v0.9.0
[0.8.2]: https://github.com/bug-ops/zeph/compare/v0.8.1...v0.8.2
[0.8.1]: https://github.com/bug-ops/zeph/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/bug-ops/zeph/compare/v0.7.1...v0.8.0
[0.7.1]: https://github.com/bug-ops/zeph/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/bug-ops/zeph/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/bug-ops/zeph/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/bug-ops/zeph/compare/v0.4.3...v0.5.0
[0.4.3]: https://github.com/bug-ops/zeph/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/bug-ops/zeph/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/bug-ops/zeph/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/bug-ops/zeph/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/bug-ops/zeph/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/bug-ops/zeph/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/bug-ops/zeph/releases/tag/v0.1.0
