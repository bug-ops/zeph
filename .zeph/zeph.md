# Zeph Project Instructions

## Project

Zeph — lightweight Rust AI agent with hybrid inference (Ollama / Claude / OpenAI / HuggingFace via candle), skills-first architecture, semantic memory with Qdrant, MCP client, A2A protocol support, multi-model orchestration, self-learning skill evolution, TUI dashboard, and multi-channel I/O (CLI + Telegram + TUI).

## Architecture

Cargo workspace (Edition 2024, resolver 3):

```
zeph (binary) — bootstrap, AnyChannel dispatch, vault resolution
├── zeph-core       — agent loop, config, channel trait, context builder, metrics, vault, redact
├── zeph-llm        — LlmProvider trait, Ollama + Claude + OpenAI + Candle backends, orchestrator
├── zeph-skills     — SKILL.md parser, registry, embedding matcher, hot-reload, self-learning
├── zeph-memory     — SQLite + Qdrant, SemanticMemory orchestrator, summarization
├── zeph-channels   — Telegram adapter (teloxide) with streaming, CLI channel
├── zeph-tools      — ToolExecutor trait, ShellExecutor, WebScrapeExecutor, CompositeExecutor, audit
├── zeph-tui        — ratatui-based TUI dashboard with real-time metrics (feature-gated)
├── zeph-mcp        — MCP client via rmcp, multi-server lifecycle, Qdrant tool registry (optional)
├── zeph-a2a        — A2A protocol client + server, agent discovery, JSON-RPC 2.0 (optional)
├── zeph-index      — AST-based code indexing, semantic retrieval, repo map generation (optional)
├── zeph-gateway    — HTTP gateway for webhook ingestion with bearer auth (optional)
└── zeph-scheduler  — cron-based periodic task scheduler with SQLite persistence (optional)
```

`zeph-core` orchestrates all leaf crates. Optional (feature-gated): `zeph-a2a`, `zeph-mcp`, `zeph-tui`, `zeph-index`, `zeph-gateway`, `zeph-scheduler`. Check `[features]` in root `Cargo.toml` for the current default and optional feature set.

## Build & Test

```bash
cargo build                                                                    # build workspace
cargo +nightly fmt --check                                                     # check formatting
cargo clippy --workspace --features full -- -D warnings                        # lint (zero warnings)
cargo nextest run --config-file .github/nextest.toml --workspace --features full --lib --bins  # unit tests
cargo nextest run --config-file .github/nextest.toml -p zeph-core              # single crate
cargo nextest run --config-file .github/nextest.toml -p zeph-core -E 'test(name)'             # single test
cargo nextest run --config-file .github/nextest.toml -- --ignored              # integration (requires Qdrant)
cargo run                                                                      # CLI mode
cargo run -- --tui                                                             # TUI dashboard
cargo run -- --config path/to/config.toml                                      # custom config
```

Always use `--features full` for local checks to match CI exactly.

## Specifications

- Skills format (SKILL.md): https://agentskills.io/specification.md
- A2A protocol: https://raw.githubusercontent.com/a2aproject/A2A/main/docs/specification.md
- MCP protocol: https://modelcontextprotocol.io/specification/2025-11-25.md
- ACP protocol: https://agentclientprotocol.com/get-started/introduction
- Claude prompt caching: https://platform.claude.com/docs/en/build-with-claude/prompt-caching

## Rust Code Conventions

### Language & Toolchain

- Rust Edition 2024: native async traits, no `async-trait` crate. Check MSRV in root `Cargo.toml`
- Formatting: `cargo +nightly fmt`, edition 2024, `max_width = 100`
- Async runtime: `tokio` with `#[tokio::main]` for entry point, `#[tokio::test]` for tests
- Use `Pin<Box<dyn Future<...> + Send + '_>>` when trait object safety requires dynamic dispatch

### Lints & Error Handling

- Workspace-level `clippy::all` + `clippy::pedantic` as warnings
- `unsafe_code = "deny"` — never use unsafe code
- `unwrap_used` and `expect_used` are warned — use `?` operator with proper error propagation
- All crates: `thiserror` for typed error enums. `anyhow` only in `main.rs`
- Document errors: add `# Errors` section in doc comments for fallible public functions

### Code Style

- Builder methods: return `self`, annotate with `#[must_use]`
- Constructor generics: accept `impl Into<String>` for string parameters
- Feature-gated modules: `#[cfg(feature = "name")]` on `pub mod` declarations in `lib.rs`
- Re-export public API at crate root via `pub use` in `lib.rs`
- Structured logging via `tracing` crate (not `log`)
- Type aliases for complex pinned types: `type ChatStream = Pin<Box<dyn Stream<...> + Send>>`
- After adding new `.rs` files, run `./.github/scripts/add-spdx-headers.sh` to add SPDX headers

### Code Quality

- All documentation, comments, and plans must be written in English
- Do not add redundant comments — only explain cyclomatically and/or cognitively complex blocks
- Follow DRY: check for existing implementations before creating new functionality, study existing patterns and reuse them
- Before v1.0.0: implement only the minimum necessary functionality. Avoid excessive code, additional abstractions, and premature optimization
- Before v1.0.0: do not worry about backward compatibility — write clean code, remove obsolete constructs without deprecation warnings. Document all breaking changes in CHANGELOG.md

## Dependency Management

- Versions defined only in root `[workspace.dependencies]` (no features at workspace level)
- Crates inherit via `workspace = true` and specify features locally in their own `Cargo.toml`
- All dependencies sorted alphabetically
- TLS: `rustls` everywhere — never introduce `openssl-sys` dependency
- Supply chain: `deny.toml` enforces license allowlist and advisory database checks
- Workspace lints inherited via `[lints] workspace = true` in each crate
- In Rust projects: avoid `serde_yaml` (deprecated), `serde_yml` (RUSTSEC-2025-0068, archived), prefer `serde_norway` if runtime YAML parsing is required

## Testing

### Structure

- Unit tests: inline `#[cfg(test)] mod tests` blocks at the end of each module
- Integration tests: `tests/` directories in crates that need external services
- Mock types (e.g., `MockProvider`, `MockChannel`) inside `#[cfg(test)]` blocks — implement the real trait, not a mocking framework
- Use `tempfile` for filesystem fixtures, `testcontainers` for Qdrant integration tests
- Tests that touch the filesystem must use per-test unique paths to avoid races

### Snapshot Tests (insta)

- Snapshots live in `src/snapshots/` directories alongside the module
- CI runs `cargo insta test --workspace --features full --check --lib --bins`
- Accept locally: `cargo insta test --workspace --features full --lib --bins && cargo insta accept`
- Commit updated `.snap` files together with the code change that caused them

### CI Pipeline

- `cla` -> `lint-fmt` -> `lint-clippy` -> `snapshots` -> `build-tests` -> `test` (matrix: ubuntu, macos, windows) -> `doc-test` -> `integration` -> `coverage` -> `docker-build-and-scan`
- Gate job `ci-status` requires all checks to pass
- Coverage via `cargo-llvm-cov` uploaded to codecov

## Development Rules

- When adding new functionality, always provide all applicable integration points:
  1. `config.toml` section for configuration
  2. CLI subcommand or argument for management
  3. TUI command palette entry or `/` input command
  4. Interactive configuration wizard (`--init`) update for new options, feature flags, or CLI arguments

- Any background or implicit TUI operation (LLM inference, skill loading, memory search, tool execution, MCP connection, etc.) **must** be accompanied by a visible system status indicator with a spinner. Status messages must be short and descriptive (e.g., `Searching memory...`, `Executing tool: shell`, `Connecting to MCP server...`).

## Documentation

- Any code change affecting user-facing behavior, configuration, or architecture must be reflected in `docs/src/` pages
- Update `docs/src/SUMMARY.md` when adding new pages
- Config: TOML (`config/default.toml`) with env var overrides (`ZEPH_*` prefix)
- When adding or changing env vars / config keys, update both `config/default.toml` and `skills/setup-guide/SKILL.md`
- SKILL.md files (YAML frontmatter + markdown body) in `skills/` directory, injected into system prompt at startup
- Module-level: `//!` doc comments in `lib.rs` describing crate purpose
- Public functions: `///` with `# Errors` section for fallible functions

## Git & Branching

- Branch naming: features `feat/m{N}/{feature-slug}`, bug fixes `fix/{short-slug}`, hotfixes `hotfix/{short-slug}`
- Commit messages and PRs: never mention co-authored or AI tools, never use emoji, keep concise and professional
- Before every commit, push, or PR: run fmt, clippy, and tests with `--features full`
- Before pushing to a feature branch: sync with main via `git fetch origin main && git merge origin/main`, resolve all conflicts, ensure build and tests pass
- Update `CHANGELOG.md` at the end of each implementation phase (per PR). Use `[Unreleased]` section if version is not yet assigned
- Update `docs/src/` pages if user-facing behavior changed
- Update root `README.md` and affected `crates/*/README.md` if project-level features or crate APIs changed
