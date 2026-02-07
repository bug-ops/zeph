# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

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

#### M7 Phase 3 (Issue #41): Agent integration with ToolExecutor trait
- Agent now uses `ShellExecutor` for all bash command execution with safety checks
- Four integration tests for blocked command behavior and error handling
- Security improvements: blocked commands no longer leak pattern details to users

### Security

- **CRITICAL fix for SEC-001**: Shell commands now filtered through ShellExecutor with DEFAULT_BLOCKED patterns (rm -rf /, sudo, mkfs, dd if=, curl, wget, nc, shutdown, reboot, halt, poweroff, init 0). Resolves command injection vulnerability.

### Fixed

- Shell command timeout now respects `config.tools.shell.timeout` (was hardcoded 30s)
- Removed duplicate bash parsing logic from agent.rs (now centralized in zeph-tools)
- Error message pattern leakage: blocked commands now show generic security policy message instead of leaking exact blocked pattern

### Changed

**BREAKING CHANGES** (pre-1.0.0):
- `Agent::new()` signature changed: now requires `tool_executor: T` as 4th parameter where `T: ToolExecutor`
- `Agent` struct now generic over three types: `Agent<P, C, T>` (provider, channel, tool_executor)
- Workspace `Cargo.toml` now defines `version = "0.2.0"` in `[workspace.package]` section
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
