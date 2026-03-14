# Zeph Project Instructions

## Project

Zeph — Rust AI agent with hybrid inference (Ollama / Claude / OpenAI / HuggingFace via candle),
skills-first architecture, semantic + graph memory with Qdrant/SQLite, MCP client, A2A/ACP protocol
support, multi-model orchestration with Thompson Sampling, self-learning skill evolution, TUI
dashboard, untrusted content isolation, and multi-channel I/O (CLI + Telegram + TUI).

Current version: **v0.15.0**. MSRV: **1.88** (Edition 2024, resolver 3).

## Architecture

Cargo workspace with 14 crates:

```
zeph (binary) — bootstrap, AnyChannel dispatch, vault resolution
├── zeph-core       — agent loop, config, channel trait, context builder, metrics, vault, redact
├── zeph-llm        — LlmProvider trait, Ollama + Claude + OpenAI + Candle backends, orchestrator
├── zeph-skills     — SKILL.md parser, registry, embedding matcher, hot-reload, self-learning
├── zeph-memory     — SQLite + Qdrant, SemanticMemory orchestrator, graph memory, summarization
├── zeph-channels   — Telegram adapter (teloxide) with streaming, CLI channel
├── zeph-tools      — ToolExecutor trait, ShellExecutor, WebScrapeExecutor, CompositeExecutor, audit
├── zeph-tui        — ratatui-based TUI dashboard with real-time metrics (feature-gated)
├── zeph-mcp        — MCP client via rmcp, multi-server lifecycle, Qdrant tool registry
├── zeph-a2a        — A2A protocol client + server, agent discovery, JSON-RPC 2.0
├── zeph-acp        — Agent Client Protocol: stdio/HTTP+SSE/WebSocket, IDE integration (Zed, VS Code, Helix)
├── zeph-index      — AST-based code indexing, semantic retrieval, repo map generation
├── zeph-gateway    — HTTP gateway for webhook ingestion with bearer auth
└── zeph-scheduler  — cron-based periodic task scheduler with SQLite persistence
```

`zeph-core` orchestrates all leaf crates. Optional (feature-gated): `zeph-a2a`, `zeph-mcp`,
`zeph-tui`, `zeph-index`, `zeph-gateway`, `zeph-scheduler`, `zeph-acp`. Always-on: `openai`,
`compatible`, `orchestrator`, `router`, `self-learning`, `qdrant`, `vault-age`, `mcp`.
Check `[features]` in root `Cargo.toml` for the full feature set.

## Key Subsystems

- **Orchestration**: DAG-based task graphs, tick-based `DagScheduler`, `LlmPlanner` (structured
  output goal decomposition), `LlmAggregator` (per-task token budget), `AgentRouter` (rule-based
  3-step fallback + inline execution for single-agent setups). `/plan` CLI commands.
- **Graph memory**: SQLite schema (entities/edges/communities), LLM-powered fire-and-forget
  extraction, `EntityResolver` (embedding-based + LLM disambiguation), BFS traversal, FTS5 search,
  label propagation community detection, edge deduplication. Feature flag: `graph-memory`.
- **Untrusted content isolation**: `ContentSanitizer` pipeline (17 injection patterns), source
  boundaries, `QuarantinedSummarizer` (Dual LLM), `ExfiltrationGuard` (image pixel-tracking,
  tool URL validation, memory write suppression). TUI security panel + SEC status bar.
- **ACP**: stdio/HTTP+SSE/WebSocket transports, multi-session with LRU eviction, LSP diagnostics
  injection, model switching, tool call lifecycle, session fork/resume, MCP passthrough.
  Works in Zed, Helix, VS Code. Feature flag: `acp`.
- **Self-learning**: `FeedbackDetector` (regex + Jaccard + self-correction signal), Wilson score
  re-ranking, BM25+RRF hybrid search, provider EMA routing, 4-tier trust model, Bayesian re-ranking.
  Positive feedback skips skill rewrite. Always-on via `self-learning` feature.
- **Thompson Sampling router**: Beta-distribution exploration/exploitation for model selection,
  EMA latency routing.
- **Context engineering**: deferred tool-pair summaries (pre-computed eagerly, applied lazily at
  70% context), pruning at 80%, LLM compaction on overflow. `--debug-dump [PATH]` writes numbered
  LLM request/response/tool-output files.
- **Sub-agents**: scoped tools, skills, zero-trust secret delegation, `permission_mode`,
  persistent memory scopes, JSONL transcript storage, lifecycle hooks.

## Build & Test

```bash
cargo build                                                                    # build workspace
cargo +nightly fmt --check                                                     # check formatting
cargo clippy --workspace --features full -- -D warnings                        # lint (zero warnings)
cargo nextest run --config-file .github/nextest.toml --workspace --features full --lib --bins
cargo nextest run --config-file .github/nextest.toml -p zeph-core              # single crate
cargo nextest run --config-file .github/nextest.toml -p zeph-core -E 'test(name)'
cargo nextest run --config-file .github/nextest.toml -- --ignored              # integration (requires Qdrant)
cargo run                                                                      # CLI mode
cargo run -- --tui                                                             # TUI dashboard
cargo run -- --config path/to/config.toml                                      # custom config
```

Always use `--features full` for local checks to match CI exactly.

## Snapshot Tests

- Snapshots live in `src/snapshots/` directories alongside the module
- CI runs `cargo insta test --workspace --features full --check --lib --bins`
- Accept locally: `INSTA_UPDATE=always cargo nextest run ... && cargo insta accept`
- Commit updated `.snap` files together with the code change that caused them

## File Layout

```
.zeph/
  skills/        — SKILL.md files loaded at startup (default skills directory)
  data/          — SQLite databases, audit logs, tool output
  debug/         — LLM request/response dump files (--debug-dump)
  agents/        — sub-agent definition files
  zeph.md        — this file (always loaded into system prompt)
config/
  default.toml   — config reference with all keys and defaults
src/             — binary entry point
crates/          — workspace member crates
docs/src/        — mdBook documentation
```

Skills are loaded from `.zeph/skills/` by default. The watcher monitors this directory for
hot-reload (500 ms debounce). Override via `[skills] directory = "path"` in config.

## Specifications

- Skills format (SKILL.md): https://agentskills.io/specification.md
- A2A protocol: https://raw.githubusercontent.com/a2aproject/A2A/main/docs/specification.md
- MCP protocol: https://modelcontextprotocol.io/specification/2025-11-25.md
- ACP protocol: https://agentclientprotocol.com/get-started/introduction
- Claude prompt caching: https://platform.claude.com/docs/en/build-with-claude/prompt-caching

## Rust Code Conventions

### Language & Toolchain

- Rust Edition 2024: native async traits, no `async-trait` crate
- Formatting: `cargo +nightly fmt`, edition 2024, `max_width = 100`
- Async runtime: `tokio` with `#[tokio::main]` / `#[tokio::test]`
- Use `Pin<Box<dyn Future<...> + Send + '_>>` for trait object safety

### Lints & Error Handling

- Workspace-level `clippy::all` + `clippy::pedantic` as warnings
- `unsafe_code = "deny"` — never use unsafe
- `unwrap_used` / `expect_used` warned — use `?` for error propagation
- `thiserror` in library crates, `anyhow` only in `main.rs`
- Fallible public functions: add `# Errors` doc section

### Code Style

- Builder methods: `#[must_use]`, return `self`
- Constructor generics: `impl Into<String>` for string params
- Feature-gated modules: `#[cfg(feature = "name")]` on `pub mod` in `lib.rs`
- Structured logging via `tracing` crate (not `log`)
- After adding new `.rs` files: run `./.github/scripts/add-spdx-headers.sh`
- TUI: log must NOT go to stdout — in TUI mode `init_tracing()` suppresses the stderr layer

### Code Quality

- Documentation, comments, plans in English only
- No redundant comments — only explain complex blocks
- DRY: check existing implementations before creating new ones
- Before v1.0.0: minimum necessary functionality, no premature abstractions
- Before v1.0.0: no backward compatibility — remove obsolete code, document breaking changes in CHANGELOG.md

## Dependency Management

- Versions defined only in root `[workspace.dependencies]`
- Crates inherit via `workspace = true`, specify features locally
- Dependencies sorted alphabetically
- TLS: `rustls` everywhere — never introduce `openssl-sys`
- `deny.toml` enforces license allowlist and advisory database
- Avoid `serde_yaml` / `serde_yml` (RUSTSEC-2025-0068) — use `serde_norway` for YAML

## Development Rules

When adding new functionality, always provide all applicable integration points:
1. `config.toml` section for configuration
2. CLI subcommand or argument for management
3. TUI command palette entry or `/` input command
4. Interactive configuration wizard (`--init`) update

Any background TUI operation **must** show a visible spinner with a short status message
(e.g., `Searching memory...`, `Executing tool: shell`, `Connecting to MCP server...`).

## Documentation

- User-facing changes → update `docs/src/` pages (use `/mdbook-tech-writer` skill)
- New pages → update `docs/src/SUMMARY.md`
- Config changes → update `config/default.toml` and `.zeph/skills/setup-guide/SKILL.md`
- Module-level: `//!` doc comments in `lib.rs`
- Public functions: `///` with `# Errors` for fallible functions

## Git & Release

- Branch naming: `feat/m{N}/{slug}`, `fix/{slug}`, `hotfix/{slug}`
- Commits and PRs: no AI mention, no emoji, concise and professional
- Before every commit: fmt + clippy + nextest with `--features full`
- Before pushing feature branch: `git fetch origin main && git merge origin/main`
- End each PR with CHANGELOG.md update (`[Unreleased]` section)
- **Before release PR**: update tests badge count in `README.md`
  (`[![Tests](https://img.shields.io/badge/tests-XXXX-brightgreen)]`)
  Get count: `cargo nextest run ... 2>&1 | grep "tests run"`
