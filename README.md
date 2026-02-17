# Zeph

[![CI](https://img.shields.io/github/actions/workflow/status/bug-ops/zeph/ci.yml?branch=main&label=CI)](https://github.com/bug-ops/zeph/actions)
[![codecov](https://codecov.io/gh/bug-ops/zeph/graph/badge.svg?token=S5O0GR9U6G)](https://codecov.io/gh/bug-ops/zeph)
[![Trivy Scan](https://img.shields.io/badge/Trivy-0%20CVEs-success)](https://github.com/bug-ops/zeph/security)
![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20Windows-blue)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

Lightweight AI agent that routes tasks across **Ollama, Claude, OpenAI, HuggingFace, and OpenAI-compatible endpoints** (Together AI, Groq, etc.) — with semantic skill matching, vector memory, MCP tooling, and agent-to-agent communication. Ships as a single binary for Linux, macOS, and Windows.

<div align="center">
  <img src="asset/zeph-logo.png" alt="Zeph" width="600">
</div>

## Why Zeph

**Token-efficient by design.** Most agent frameworks inject every tool and instruction into every prompt. Zeph embeds skills and MCP tools as vectors (with concurrent embedding via `buffer_unordered`), then selects only the top-K relevant ones per query via cosine similarity. Prompt size stays O(K) -- not O(N) -- regardless of how many capabilities are installed. Smart output filtering further reduces token consumption by 70-99% for common tool outputs (test results, git logs, clippy diagnostics, directory listings, log deduplication) — per-command filter stats are shown inline in CLI chat and aggregated in the TUI dashboard.

**Intelligent context management.** Two-tier context pruning: Tier 1 selectively removes old tool outputs (clearing bodies from memory after persisting to SQLite) before falling back to Tier 2 LLM-based compaction, reducing unnecessary LLM calls. A token-based protection zone preserves recent context from pruning. Parallel context preparation via `try_join!` and optimized byte-length token estimation. Cross-session memory transfers knowledge between conversations with relevance filtering. Proportional budget allocation (8% summaries, 8% semantic recall, 4% cross-session, 30% code context, 50% recent history) keeps conversations efficient. Tool outputs are truncated at 30K chars with optional LLM-based summarization for large outputs. Doom-loop detection breaks runaway tool cycles after 3 identical consecutive outputs, with configurable iteration limits (default 10). ZEPH.md project config discovery walks up the directory tree and injects project-specific context when available. Config hot-reload applies runtime-safe fields (timeouts, security, memory limits) on file change without restart.

**Run anywhere.** Local models via Ollama or Candle (GGUF with Metal/CUDA), cloud APIs (Claude, OpenAI), OpenAI-compatible endpoints (Together AI, Groq, Fireworks) via `CompatibleProvider`, or all of them at once through the multi-model orchestrator with automatic fallback chains and `RouterProvider` for prompt-based model selection.

**Production-ready security.** Shell sandboxing with path restrictions and relative path traversal detection, pattern-based permission policy per tool, destructive command confirmation, file operation sandbox with path traversal protection, tool output overflow-to-file (with LLM-accessible paths), secret redaction (AWS, OpenAI, Anthropic, Google, GitLab), audit logging, SSRF protection (including MCP client), rate limiter with TTL-based eviction, and Trivy-scanned container images with 0 HIGH/CRITICAL CVEs.

**Observable.** Optional OpenTelemetry OTLP export (feature-gated behind `otel`) for Prometheus/Grafana integration, with per-model cost tracking and configurable daily budget limits.

**Self-improving.** Skills evolve through failure detection, self-reflection, and LLM-generated improvements — with optional manual approval before activation.

## Installation

### From source

```bash
git clone https://github.com/bug-ops/zeph
cd zeph
cargo build --release
```

### Pre-built binaries

Download from [GitHub Releases](https://github.com/bug-ops/zeph/releases/latest) for Linux (x86_64, aarch64), macOS (x86_64, aarch64), and Windows (x86_64).

### Docker

```bash
docker pull ghcr.io/bug-ops/zeph:latest
```

Multi-platform images (linux/amd64, linux/arm64) scanned with Trivy in CI. See [Docker deployment guide](https://bug-ops.github.io/zeph/guide/docker.html) for GPU, Compose, and vault options.

## Quick Start

```bash
# Pull models for Ollama
ollama pull mistral:7b
ollama pull qwen3-embedding

# Run
./target/release/zeph
```

For Telegram bot mode:

```bash
ZEPH_TELEGRAM_TOKEN="123:ABC" ./target/release/zeph
```

For cloud providers:

```bash
# Claude
ZEPH_LLM_PROVIDER=claude ZEPH_CLAUDE_API_KEY=sk-ant-... ./target/release/zeph

# OpenAI
ZEPH_LLM_PROVIDER=openai ZEPH_OPENAI_API_KEY=sk-... ./target/release/zeph

# OpenAI-compatible endpoint (Together AI, Groq, Fireworks, etc.)
ZEPH_LLM_PROVIDER=compatible ZEPH_COMPATIBLE_BASE_URL=https://api.together.xyz/v1 \
  ZEPH_COMPATIBLE_API_KEY=... ./target/release/zeph
```

For Discord or Slack bot mode (requires respective feature):

```bash
cargo build --release --features discord
ZEPH_DISCORD_TOKEN="..." ZEPH_DISCORD_APP_ID="..." ./target/release/zeph

cargo build --release --features slack
ZEPH_SLACK_BOT_TOKEN="xoxb-..." ZEPH_SLACK_SIGNING_SECRET="..." ./target/release/zeph
```

For TUI dashboard (requires `tui` feature):

```bash
cargo build --release --features tui
./target/release/zeph --tui
```

> [!TIP]
> Use `--config /path/to/config.toml` or `ZEPH_CONFIG=...` to override the default config path. Configure secret backends (env, age) via `vault.backend` in config or CLI flags (`--vault`, `--vault-key`, `--vault-path`). Full reference: [Configuration](https://bug-ops.github.io/zeph/getting-started/configuration.html)

## Key Features

| Feature | Description | Docs |
|---------|-------------|------|
| **Native Tool Use** | Structured tool calling via Claude tool_use and OpenAI function calling APIs; automatic fallback to text extraction for local models | [Tools](https://bug-ops.github.io/zeph/guide/tools.html) |
| **Hybrid Inference** | Ollama, Claude, OpenAI, Candle (GGUF), Compatible (any OpenAI-compatible API) — local, cloud, or both | [OpenAI](https://bug-ops.github.io/zeph/guide/openai.html) · [Candle](https://bug-ops.github.io/zeph/guide/candle.html) |
| **Skills-First Architecture** | Embedding-based top-K matching, progressive loading, hot-reload | [Skills](https://bug-ops.github.io/zeph/guide/skills.html) |
| **Code Indexing** | AST-based chunking (tree-sitter), semantic retrieval, repo map generation, incremental indexing | [Code Indexing](https://bug-ops.github.io/zeph/guide/code-indexing.html) |
| **Context Engineering** | Two-tier context pruning (selective tool-output pruning before LLM compaction), semantic recall injection, proportional budget allocation, token-based protection zone for recent context, config hot-reload | [Context](https://bug-ops.github.io/zeph/guide/context.html) · [Configuration](https://bug-ops.github.io/zeph/getting-started/configuration.html) |
| **Semantic Memory** | SQLite + Qdrant vector search for contextual recall | [Memory](https://bug-ops.github.io/zeph/guide/semantic-memory.html) |
| **Tool Permissions** | Pattern-based permission policy (allow/ask/deny) with glob matching per tool, excluded denied tools from prompts | [Tools](https://bug-ops.github.io/zeph/guide/tools.html) |
| **MCP Client** | Connect external tool servers (stdio + HTTP), unified matching, SSRF protection | [MCP](https://bug-ops.github.io/zeph/guide/mcp.html) |
| **A2A Protocol** | Agent-to-agent communication via JSON-RPC 2.0 with SSE streaming, delegated task inference through agent pipeline | [A2A](https://bug-ops.github.io/zeph/guide/a2a.html) |
| **Model Orchestrator** | Route tasks to different providers with fallback chains | [Orchestrator](https://bug-ops.github.io/zeph/guide/orchestrator.html) |
| **Self-Learning** | Skills evolve via failure detection and LLM-generated improvements | [Self-Learning](https://bug-ops.github.io/zeph/guide/self-learning.html) |
| **Skill Trust & Quarantine** | 4-tier trust model (Trusted/Verified/Quarantined/Blocked) with blake3 integrity verification, anomaly detection with automatic blocking, and restricted tool access for untrusted skills | |
| **Prompt Caching** | Automatic prompt caching for Anthropic and OpenAI providers, reducing latency and cost on repeated context | |
| **Graceful Shutdown** | Ctrl-C triggers ordered teardown with MCP server cleanup and pending task draining | |
| **TUI Dashboard** | ratatui terminal UI with tree-sitter syntax highlighting, markdown rendering, syntax-highlighted diff view for write/edit tool output (compact/expanded toggle), deferred model warmup, scrollbar, mouse scroll, thinking blocks, conversation history, splash screen, live metrics (including filter savings), message queueing (max 10, FIFO with Ctrl+K clear) | [TUI](https://bug-ops.github.io/zeph/guide/tui.html) |
| **Multi-Channel I/O** | CLI, Discord, Slack, Telegram, and TUI with streaming support | [Channels](https://bug-ops.github.io/zeph/guide/channels.html) |
| **Defense-in-Depth** | Shell sandbox with relative path traversal detection, file sandbox, command filter, secret redaction (Google/GitLab patterns), audit log, SSRF protection (agent + MCP), rate limiter TTL eviction, doom-loop detection, skill trust quarantine | [Security](https://bug-ops.github.io/zeph/security.html) |

## Architecture

```
zeph (binary) — thin CLI/channel dispatch (anyhow for top-level errors)
├── zeph-core       — bootstrap/AppBuilder, Agent split into 7 submodules (context, streaming,
│                     persistence, learning, mcp, index), daemon supervisor, typed AgentError/ChannelError, config hot-reload
├── zeph-llm        — LlmProvider: Ollama, Claude, OpenAI, Candle, Compatible, orchestrator,
│                     RouterProvider, native tool_use (Claude/OpenAI), typed LlmError
├── zeph-skills     — SKILL.md parser, embedding matcher, hot-reload, self-learning, typed SkillError
├── zeph-memory     — SQLite + Qdrant, semantic recall, summarization, typed MemoryError
├── zeph-index      — AST-based code indexing, semantic retrieval, repo map (optional)
├── zeph-channels   — AnyChannel dispatch, Discord, Slack, Telegram adapters with streaming
├── zeph-tools      — schemars-driven tool registry (shell, file ops, web scrape), composite dispatch
├── zeph-mcp        — MCP client, multi-server lifecycle, unified tool matching
├── zeph-a2a        — A2A client + server, agent discovery, JSON-RPC 2.0
├── zeph-gateway    — HTTP gateway for webhook ingestion with bearer auth (optional)
├── zeph-scheduler  — Cron-based periodic task scheduler with SQLite persistence (optional)
└── zeph-tui        — ratatui TUI dashboard with live agent metrics (optional)
```

**Error handling:** Typed errors throughout all library crates -- `AgentError` (7 variants), `ChannelError` (4 variants), `LlmError`, `MemoryError`, `SkillError`. `anyhow` is used only in `main.rs` for top-level orchestration. Shared Qdrant operations consolidated via `QdrantOps` helper. `AnyProvider` dispatch deduplicated via `delegate_provider!` macro. `AnyChannel` enum dispatch lives in `zeph-channels` for reuse across binaries.

**Agent decomposition:** The agent module in `zeph-core` is split into 7 submodules (`mod.rs`, `context.rs`, `streaming.rs`, `persistence.rs`, `learning.rs`, `mcp.rs`, `index.rs`) with 5 inner field-grouping structs (`MemoryState`, `SkillState`, `ContextState`, `McpState`, `IndexState`).

**MessageKind enum:** Replaces the previous `is_summary` boolean with an explicit variant-based message classification.

> [!IMPORTANT]
> Requires Rust 1.88+ (Edition 2024). Native async traits — no `async-trait` crate dependency.

Deep dive: [Architecture overview](https://bug-ops.github.io/zeph/architecture/overview.html) · [Crate reference](https://bug-ops.github.io/zeph/architecture/crates.html) · [Token efficiency](https://bug-ops.github.io/zeph/architecture/token-efficiency.html)

## Feature Flags

The following features are always compiled in (no flag needed): `openai`, `compatible`, `orchestrator`, `router`, `self-learning`, `qdrant`, `vault-age`, `mcp`.

| Feature | Description |
|---------|-------------|
| `a2a` | A2A protocol client and server |
| `candle` | Local HuggingFace inference (GGUF) |
| `index` | AST-based code indexing and semantic retrieval |
| `discord` | Discord bot with Gateway v10 WebSocket |
| `slack` | Slack bot with Events API webhook |
| `gateway` | HTTP gateway for webhook ingestion |
| `daemon` | Daemon supervisor for component lifecycle |
| `scheduler` | Cron-based periodic task scheduler |
| `otel` | OpenTelemetry OTLP export for Prometheus/Grafana |
| `metal` | Metal GPU acceleration (macOS) |
| `tui` | ratatui TUI dashboard with real-time metrics |
| `cuda` | CUDA GPU acceleration (Linux) |

```bash
cargo build --release                        # default build (all always-on features included)
cargo build --release --features full        # all optional features
cargo build --release --features metal       # macOS Metal GPU
cargo build --release --features tui         # with TUI dashboard
```

Full details: [Feature Flags](https://bug-ops.github.io/zeph/feature-flags.html)

## Documentation

Full documentation is available at **[bug-ops.github.io/zeph](https://bug-ops.github.io/zeph/)**.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development workflow and guidelines.

## Security

Found a vulnerability? Do not open a public issue. Use [GitHub Security Advisories](https://github.com/bug-ops/zeph/security/advisories/new) for responsible disclosure.

## License

[MIT](LICENSE)
