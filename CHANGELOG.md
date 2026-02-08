# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

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
