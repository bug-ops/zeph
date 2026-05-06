# Zeph

<div align="center">
  <img src="book/src/assets/zeph_v8_github.png" alt="Zeph" width="800">

  **A memory-first AI agent for long-running work on local, cloud, and decentralized inference.**

  [![Crates.io](https://img.shields.io/crates/v/zeph)](https://crates.io/crates/zeph)
  [![docs](https://img.shields.io/badge/docs-book-blue)](https://bug-ops.github.io/zeph/)
  [![CI](https://img.shields.io/github/actions/workflow/status/bug-ops/zeph/ci.yml?branch=main&label=CI)](https://github.com/bug-ops/zeph/actions)
  [![codecov](https://codecov.io/gh/bug-ops/zeph/graph/badge.svg?token=S5O0GR9U6G)](https://codecov.io/gh/bug-ops/zeph)
  [![MSRV](https://img.shields.io/badge/MSRV-1.95-blue)](https://www.rust-lang.org)
  [![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
</div>

Zeph is a Rust-native AI agent built for work that cannot fit into one chat window: coding sessions, operations, research loops, document RAG, scheduled jobs, and multi-agent workflows. It keeps short-term context sharp, persists long-term memory, builds a relationship graph from decisions and entities, and routes each task to the cheapest provider that can handle it.

Unlike single-session assistants, Zeph is designed to remember *why* a decision happened, not just the last messages around it.

## Why Try Zeph

| If you want... | Zeph gives you... |
|---|---|
| An agent that survives long projects | SQLite conversation history, semantic recall, graph memory, session digests, trajectory memory, and goal-aware compaction. |
| Lower infrastructure cost | A default SQLite vector backend, local Ollama defaults, feature-gated bundles, and provider routing for simple vs. hard tasks. |
| More than keyword memory | Typed graph facts, BFS recall, SYNAPSE spreading activation, MMR reranking, temporal decay, and write-quality gates. |
| Provider freedom | Ollama, Claude, OpenAI, Gemini, Candle, any OpenAI-compatible endpoint, and Gonka.ai via GonkaGate or the native signed-transport provider. |
| Agent-grade safety | Age-encrypted vault secrets, sandboxed tool execution, MCP injection detection, SSRF guards, PII filtering, and exfiltration checks. |
| Daily operator ergonomics | CLI, TUI dashboard, MCP tools, plugins, skills, sub-agents, ACP for IDEs, A2A, scheduler, and JSON output modes. |

## Quick Start

Install the latest release:

```bash
curl -fsSL https://github.com/bug-ops/zeph/releases/latest/download/install.sh | sh
```

Or install from crates.io:

```bash
cargo install zeph
```

Initialize and run:

```bash
zeph init
zeph doctor
zeph --tui
```

> [!IMPORTANT]
> Zeph requires Rust 1.95 or later when building from source. Pre-built binaries do not require a Rust toolchain.

For a local-first setup, run Ollama and pull the default lightweight models:

```bash
ollama pull qwen3:8b
ollama pull qwen3-embedding
zeph init
zeph
```

## Gonka.ai

Zeph supports Gonka.ai inference in two modes:

**GonkaGate (OpenAI-compatible gateway):** store the `gp-...` key in the age vault as `ZEPH_COMPATIBLE_GONKAGATE_API_KEY` and use the `compatible` provider type:

```toml
[[llm.providers]]
name = "gonkagate"
type = "compatible"
base_url = "https://api.gonkagate.com/v1"
model = "Qwen/Qwen3-235B-A22B-Instruct-2507-FP8"
default = true
```

**Native Gonka provider (signed transport):** use `type = "gonka"` with node endpoints. The native provider supports chat, streaming, embeddings, and tool calls over a signed request transport. Store the key as `ZEPH_GONKA_API_KEY` in the vault and declare nodes via `--init` wizard or directly in config:

```toml
[[llm.providers]]
name = "gonka"
type = "gonka"
model = "qwen3-235b"
default = true

[[llm.gonka_nodes]]
url = "https://node.example.gonka.ai"
weight = 1
```

Run `zeph init` and select the GonkaGate option in the wizard to configure either mode interactively.

## What Makes It Different

### Memory is the product

Zeph combines several memory layers instead of treating recall as a side feature:

| Layer | Purpose |
|---|---|
| Working context | Keeps the current task coherent under context pressure. |
| Semantic memory | Stores conversations, tool outputs, documents, and summaries for retrieval. |
| Graph memory | Records entities, decisions, relationships, causality, temporal links, and hierarchy. |
| Episodic memory | Preserves session-level scenes, digests, goals, and trajectories. |
| Quality gates | Reject noisy writes, validate compaction, and log retrieval failures for later improvement. |

Ask "Why did we choose PostgreSQL?" and Zeph can traverse decision edges instead of searching raw chat text.

### Built for low-resource setups

Zeph does not require a heavyweight stack to be useful:

- The default vector backend is embedded SQLite.
- Qdrant is optional for larger semantic and graph workloads.
- The default local chat model is `qwen3.6:8b` through Ollama.
- Feature bundles let you build only what you need: `desktop`, `ide`, `server`, `chat`, `ml`, or `full`.
- Release builds are optimized for small native binaries.

### Multi-model by design

Declare providers once in `[[llm.providers]]`, then route work by complexity, cost, latency, and reliability:

```toml
[[llm.providers]]
name = "fast"
type = "ollama"
model = "qwen3:8b"
embedding_model = "qwen3-embedding"
embed = true

[[llm.providers]]
name = "quality"
type = "claude"
model = "claude-sonnet-4-6"
default = true

[llm]
routing = "bandit"
```

Use local models for extraction, embeddings, routing, and summarization. Keep expensive models for planning, code generation, and expert reasoning.

### Tools without loose secrets

Secrets live in the Zeph age vault, not in `.env` files or shell profiles. Tool execution goes through trust gates, command filters, sandboxing, audit logs, and redaction paths. MCP tools are discovered and exposed without dropping the injection and authorization checks.

## Demo

<div align="center">
  <img src="book/src/assets/zeph.gif" alt="Zeph TUI Dashboard" width="800">
</div>

## Common Commands

```bash
zeph init                    # generate config through the wizard
zeph doctor                  # run preflight checks
zeph --tui                   # launch the dashboard
zeph ingest ./docs           # ingest documents into semantic memory
zeph skill list              # inspect installed skills
zeph plugin list --overlay   # inspect plugin config overlays
zeph router stats            # inspect adaptive provider routing
zeph memory export dump.json # export memory snapshot
zeph project purge --dry-run # preview local state cleanup
```

## Installation Options

### Pre-built Binary

```bash
curl -fsSL https://github.com/bug-ops/zeph/releases/latest/download/install.sh | sh
```

### Cargo

```bash
cargo install zeph
cargo install zeph --features desktop
```

### Docker

```bash
docker pull ghcr.io/bug-ops/zeph:latest
```

### From Source

```bash
git clone https://github.com/bug-ops/zeph.git
cd zeph
cargo build --release --features full
./target/release/zeph init
```

## Feature Highlights

| Area | Highlights |
|---|---|
| Memory | SQLite/PostgreSQL history, embedded SQLite vectors or Qdrant, graph memory, SYNAPSE, SleepGate, APEX-MEM write-quality gates, MemCoT Zoom-In/Out recall views, document RAG. |
| Context | Goal-aware compaction, TypedPage assembler pipeline, TACO output compression, tool-output archive, session recap, active-goal injection. |
| Skills | `SKILL.md` registry, hot reload, BM25 + embedding matching, trust levels, self-learning skill improvement. |
| Providers | Ollama, Claude, OpenAI, Gemini, OpenAI-compatible APIs, Gonka native inference, Candle local inference, adaptive routing. |
| Tools | Shell, file, web, MCP, tool quotas, approval gates, audit trail, sandboxing, output compression, speculative dispatch, TrajectorySentinel capability governance. |
| Interfaces | CLI, TUI, Telegram, Discord, Slack, ACP, A2A, HTTP gateway, scheduler daemon. |
| Code intelligence | Tree-sitter indexing, semantic repo map, LSP diagnostics and hover context through MCP. |
| Observability | Debug dumps, JSONL mode, Prometheus metrics, OpenTelemetry traces, profiling builds. |

## Architecture

```text
zeph
  src/                    CLI, bootstrap, init wizard, command handlers
  crates/zeph-core        agent loop and runtime orchestration
  crates/zeph-config      TOML schema, migration, provider registry
  crates/zeph-llm         provider abstraction and model backends
  crates/zeph-memory      semantic, graph, episodic, and document memory
  crates/zeph-skills      skill registry, matching, trust, learning
  crates/zeph-tools       tool executors, sandboxing, policy, audit
  crates/zeph-mcp         MCP client and tool lifecycle
  crates/zeph-tui         ratatui dashboard
  crates/zeph-acp         IDE integration through Agent Client Protocol
  crates/zeph-a2a         agent-to-agent protocol support
  crates/zeph-subagent    sub-agent definitions, spawning, transcripts
  crates/zeph-orchestration DAG planning, scheduling, verification
```

## Documentation

- [Full documentation](https://bug-ops.github.io/zeph/)
- [Installation guide](https://bug-ops.github.io/zeph/getting-started/installation.html)
- [Configuration recipes](https://bug-ops.github.io/zeph/guides/config-recipes.html)
- [Graph memory](https://bug-ops.github.io/zeph/concepts/graph-memory.html)
- [Security model](https://bug-ops.github.io/zeph/reference/security.html)
- [Feature flags](https://bug-ops.github.io/zeph/reference/feature-flags.html)

Zeph draws from published work on parallel tool execution, temporal knowledge graphs, agentic memory linking, failure-driven compression, retrieval quality, and multi-model routing. See [References & Inspirations](https://bug-ops.github.io/zeph/references.html) for the full list.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md), [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md), and [SECURITY.md](SECURITY.md).

## License

[MIT](LICENSE)
