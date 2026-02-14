# Zeph

[![CI](https://img.shields.io/github/actions/workflow/status/bug-ops/zeph/ci.yml?branch=main&label=CI)](https://github.com/bug-ops/zeph/actions)
[![codecov](https://codecov.io/gh/bug-ops/zeph/graph/badge.svg?token=S5O0GR9U6G)](https://codecov.io/gh/bug-ops/zeph)
[![Trivy Scan](https://img.shields.io/badge/Trivy-0%20CVEs-success)](https://github.com/bug-ops/zeph/security)
![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20Windows-blue)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

Lightweight AI agent that routes tasks across **Ollama, Claude, OpenAI, and HuggingFace** models — with semantic skill matching, vector memory, MCP tooling, and agent-to-agent communication. Ships as a single binary for Linux, macOS, and Windows.

<div align="center">
  <img src="asset/zeph-logo.png" alt="Zeph" width="600">
</div>

## Why Zeph

**Token-efficient by design.** Most agent frameworks inject every tool and instruction into every prompt. Zeph embeds skills and MCP tools as vectors, then selects only the top-K relevant ones per query via cosine similarity. Prompt size stays O(K) — not O(N) — regardless of how many capabilities are installed.

**Intelligent context management.** Two-tier context pruning: Tier 1 selectively removes old tool outputs (clearing bodies from memory after persisting to SQLite) before falling back to Tier 2 LLM-based compaction, reducing unnecessary LLM calls. A token-based protection zone preserves recent context from pruning. Cross-session memory transfers knowledge between conversations with relevance filtering. Proportional budget allocation (8% summaries, 8% semantic recall, 4% cross-session, 30% code context, 50% recent history) keeps conversations efficient. Tool outputs are truncated at 30K chars with optional LLM-based summarization for large outputs. Doom-loop detection breaks runaway tool cycles after 3 identical consecutive outputs, with configurable iteration limits (default 10). ZEPH.md project config discovery walks up the directory tree and injects project-specific context when available. Config hot-reload applies runtime-safe fields (timeouts, security, memory limits) on file change without restart.

**Run anywhere.** Local models via Ollama or Candle (GGUF with Metal/CUDA), cloud APIs (Claude, OpenAI, GPT-compatible endpoints like Together AI and Groq), or all of them at once through the multi-model orchestrator with automatic fallback chains.

**Production-ready security.** Shell sandboxing with path restrictions, pattern-based permission policy per tool, destructive command confirmation, file operation sandbox with path traversal protection, tool output overflow-to-file (with LLM-accessible paths), secret redaction, audit logging, SSRF protection, and Trivy-scanned container images with 0 HIGH/CRITICAL CVEs.

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

# OpenAI (or any compatible API)
ZEPH_LLM_PROVIDER=openai ZEPH_OPENAI_API_KEY=sk-... ./target/release/zeph
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
| **Hybrid Inference** | Ollama, Claude, OpenAI, Candle (GGUF) — local, cloud, or both | [OpenAI](https://bug-ops.github.io/zeph/guide/openai.html) · [Candle](https://bug-ops.github.io/zeph/guide/candle.html) |
| **Skills-First Architecture** | Embedding-based top-K matching, progressive loading, hot-reload | [Skills](https://bug-ops.github.io/zeph/guide/skills.html) |
| **Code Indexing** | AST-based chunking (tree-sitter), semantic retrieval, repo map generation, incremental indexing | [Code Indexing](https://bug-ops.github.io/zeph/guide/code-indexing.html) |
| **Context Engineering** | Two-tier context pruning (selective tool-output pruning before LLM compaction), semantic recall injection, proportional budget allocation, token-based protection zone for recent context, config hot-reload | [Context](https://bug-ops.github.io/zeph/guide/context.html) · [Configuration](https://bug-ops.github.io/zeph/getting-started/configuration.html) |
| **Semantic Memory** | SQLite + Qdrant vector search for contextual recall | [Memory](https://bug-ops.github.io/zeph/guide/semantic-memory.html) |
| **Tool Permissions** | Pattern-based permission policy (allow/ask/deny) with glob matching per tool, excluded denied tools from prompts | [Tools](https://bug-ops.github.io/zeph/guide/tools.html) |
| **MCP Client** | Connect external tool servers (stdio + HTTP), unified matching | [MCP](https://bug-ops.github.io/zeph/guide/mcp.html) |
| **A2A Protocol** | Agent-to-agent communication via JSON-RPC 2.0 with SSE streaming, delegated task inference through agent pipeline | [A2A](https://bug-ops.github.io/zeph/guide/a2a.html) |
| **Model Orchestrator** | Route tasks to different providers with fallback chains | [Orchestrator](https://bug-ops.github.io/zeph/guide/orchestrator.html) |
| **Self-Learning** | Skills evolve via failure detection and LLM-generated improvements | [Self-Learning](https://bug-ops.github.io/zeph/guide/self-learning.html) |
| **TUI Dashboard** | ratatui terminal UI with markdown rendering, deferred model warmup, scrollbar, mouse scroll, thinking blocks, conversation history, splash screen, live metrics, message queueing (max 10, FIFO with Ctrl+K clear) | [TUI](https://bug-ops.github.io/zeph/guide/tui.html) |
| **Multi-Channel I/O** | CLI, Telegram, and TUI with streaming support | [Channels](https://bug-ops.github.io/zeph/guide/channels.html) |
| **Defense-in-Depth** | Shell sandbox, file sandbox with path traversal protection, command filter, secret redaction, audit log, SSRF protection, doom-loop detection | [Security](https://bug-ops.github.io/zeph/security.html) |

## Architecture

```
zeph (binary)
├── zeph-core       — agent loop, config, config hot-reload, context builder, metrics
├── zeph-llm        — LlmProvider: Ollama, Claude, OpenAI, Candle, orchestrator
├── zeph-skills     — SKILL.md parser, embedding matcher, hot-reload, self-learning
├── zeph-memory     — SQLite + Qdrant, semantic recall, summarization
├── zeph-index      — AST-based code indexing, semantic retrieval, repo map (optional)
├── zeph-channels   — Telegram adapter (teloxide) with streaming
├── zeph-tools      — schemars-driven tool registry (shell, file ops, web scrape), composite dispatch
├── zeph-mcp        — MCP client, multi-server lifecycle, unified tool matching
├── zeph-a2a        — A2A client + server, agent discovery, JSON-RPC 2.0
└── zeph-tui        — ratatui TUI dashboard with live agent metrics (optional)
```

> [!IMPORTANT]
> Requires Rust 1.88+ (Edition 2024). Native async traits — no `async-trait` crate dependency.

Deep dive: [Architecture overview](https://bug-ops.github.io/zeph/architecture/overview.html) · [Crate reference](https://bug-ops.github.io/zeph/architecture/crates.html) · [Token efficiency](https://bug-ops.github.io/zeph/architecture/token-efficiency.html)

## Feature Flags

| Feature | Default | Description |
|---------|---------|-------------|
| `a2a` | On | A2A protocol client and server |
| `openai` | On | OpenAI-compatible provider |
| `mcp` | On | MCP client for external tool servers |
| `candle` | On | Local HuggingFace inference (GGUF) |
| `orchestrator` | On | Multi-model routing with fallback |
| `self-learning` | On | Skill evolution system |
| `vault-age` | On | Age-encrypted secret storage |
| `index` | Off | AST-based code indexing and semantic retrieval |
| `metal` | Off | Metal GPU acceleration (macOS) |
| `tui` | Off | ratatui TUI dashboard with real-time metrics |
| `cuda` | Off | CUDA GPU acceleration (Linux) |

```bash
cargo build --release                        # all defaults
cargo build --release --features metal       # macOS Metal GPU
cargo build --release --no-default-features  # minimal binary
cargo build --release --features index       # with code indexing
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
