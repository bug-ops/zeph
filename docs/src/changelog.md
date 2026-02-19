# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

See the full [CHANGELOG.md](https://github.com/bug-ops/zeph/blob/main/CHANGELOG.md) in the repository for the complete version history.

## [Unreleased]

## [0.11.0] - 2026-02-19

### Added
- Vision (image input) support across Claude, OpenAI, and Ollama providers (#490)
- Interactive configuration wizard via `zeph init` subcommand with 5-step setup
- clap-based CLI argument parsing with `--help`, `--version` support
- Structured LLM output via `chat_typed<T>()` with JSON schema enforcement
- Pipeline API with composable `Step` trait, `Pipeline` builder, and `ParallelStep` combinator
- Structured intent classification for skill disambiguation
- DocumentLoader trait with text/markdown/PDF file loaders in zeph-memory
- Document ingestion pipeline: load, split, embed, store via Qdrant
- Audio input support with `SpeechToText` trait and OpenAI Whisper backend (feature: `stt`)
- Local Whisper backend via candle for offline STT
- Telegram voice/audio message handling with automatic file download
- Slack audio file upload handling with host validation and size limits
- Shell-based installation script with SHA256 verification and platform detection
- TUI test automation infrastructure with insta snapshots and proptest
- TUI word-jump, line-jump cursor navigation, keybinding help popup, clickable hyperlinks
- VectorStore trait abstraction in zeph-memory
- Operation-level cancellation for LLM requests and tool executions

### Changed
- Consolidate Docker files into `docker/` directory
- Typed deserialization for tool call params
- CI: replace oraclelinux base image with debian bookworm-slim

### Fixed
- Strip schema metadata and fix doom loop detection for native tool calls (#534)
- TUI freezes during fast LLM streaming and parallel tool execution (#500)
- Redundant syntax highlighting and markdown parsing on every TUI frame (#501)

## [0.10.0] - 2026-02-18

### Fixed
- TUI status spinner not cleared after model warmup completes (#517)
- Duplicate tool output rendering for shell-streamed tools in TUI (#516)
- `send_tool_output` not forwarded through `AppChannel`/`AnyChannel` enum dispatch (#508)
- Tool output and diff not sent atomically in native tool_use path (#498)
- Parallel tool_use calls: results processed sequentially for correct ordering (#486)
- Native `tool_result` format not recognized by TUI history loader (#484)
- Inline filter stats threshold based on char savings instead of line count (#483)
- Token metrics not propagated in native tool_use path (#482)
- Filter metrics not appearing in TUI Resources panel when using native tool_use providers (#480)
- Output filter matchers not matching compound shell commands like `cd /path && cargo test 2>&1 | tail` (#481)
- Duplicate `ToolEvent::Completed` emission in shell executor before filtering was applied (#480)
- TUI feature gate compilation errors (#435)

### Added
- GitHub CLI skill with token-saving patterns (#507)
- Parallel execution of native tool_use calls with configurable concurrency (#486)
- TUI compact/detailed tool output toggle with 'e' key binding (#479)
- TUI `[tui]` config section with `show_source_labels` option to hide `[user]`/`[zeph]`/`[tool]` prefixes (#505)
- Syntax-highlighted diff view for write/edit tool output in TUI (#455)
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
- Remove `P` generic from `Agent`, `SemanticMemory`, `CodeRetriever` — provider resolved at construction (#423)
- Architecture improvements, performance optimizations, security hardening (M24) (#417)
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
