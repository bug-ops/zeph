# Zeph

<div align="center">
  <img src="book/src/assets/zeph_v8_github.png" alt="Zeph" width="800">

  **A memory-first AI agent for long-running work on local, cloud, and decentralized inference.**

  [![Crates.io](https://img.shields.io/crates/v/zeph)](https://crates.io/crates/zeph)
  [![docs](https://img.shields.io/badge/docs-book-blue)](https://bug-ops.github.io/zeph/)
  [![CI](https://img.shields.io/github/actions/workflow/status/bug-ops/zeph/ci.yml?branch=main&label=CI)](https://github.com/bug-ops/zeph/actions)
  [![codecov](https://codecov.io/gh/bug-ops/zeph/graph/badge.svg?token=S5O0GR9U6G)](https://codecov.io/gh/bug-ops/zeph)
  [![MSRV](https://img.shields.io/badge/MSRV-1.95-blue)](https://www.rust-lang.org)
  [![Tests](https://img.shields.io/badge/tests-9201-brightgreen)](https://github.com/bug-ops/zeph/actions)
  [![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
</div>

Zeph is a Rust-native AI agent built for work that cannot fit into one chat window: coding sessions, operations, research loops, document RAG, scheduled jobs, and multi-agent workflows. It keeps short-term context sharp, persists long-term memory, builds a relationship graph from decisions and entities, and routes each task to the cheapest provider that can handle it.

Unlike single-session assistants, Zeph is designed to remember *why* a decision happened, not just the last messages around it.

## Why Try Zeph

| If you want... | Zeph gives you... |
|---|---|
| An agent that survives long projects | SQLite conversation history, [semantic recall](https://bug-ops.github.io/zeph/guides/semantic-memory.html), [graph memory](https://bug-ops.github.io/zeph/concepts/graph-memory.html), session digests, trajectory memory, and goal-aware compaction. |
| Lower infrastructure cost | A default SQLite vector backend, local [Ollama](https://ollama.ai) defaults, [feature-gated bundles](https://bug-ops.github.io/zeph/reference/feature-flags.html), and [provider routing](https://bug-ops.github.io/zeph/advanced/adaptive-inference.html) for simple vs. hard tasks. |
| More than keyword memory | Typed graph facts, BFS recall, SYNAPSE spreading activation, MMR reranking, temporal decay, and write-quality gates. See [graph memory concepts](https://bug-ops.github.io/zeph/concepts/graph-memory.html). |
| Provider freedom | [Ollama](https://ollama.ai), Claude, OpenAI, Gemini, [Candle](https://bug-ops.github.io/zeph/advanced/candle.html), any OpenAI-compatible endpoint, and distributed inference networks ([Gonka](https://bug-ops.github.io/zeph/guides/gonka.html), [Cocoon TEE](https://bug-ops.github.io/zeph/guides/cocoon.html)) for cost-sensitive or privacy-sensitive workloads. |
| Agent-grade safety | [Age-encrypted](https://github.com/FiloSottile/age) vault secrets, [sandboxed tool execution](https://bug-ops.github.io/zeph/reference/security/file-sandbox.html), [MCP injection detection](https://bug-ops.github.io/zeph/reference/security/mcp.html), SSRF guards, PII filtering, and exfiltration checks. |
| Daily operator ergonomics | CLI, [TUI](https://bug-ops.github.io/zeph/advanced/tui.html) dashboard, [MCP](https://bug-ops.github.io/zeph/guides/mcp.html) tools, plugins, [skills](https://bug-ops.github.io/zeph/concepts/skills.html), [sub-agents](https://bug-ops.github.io/zeph/advanced/sub-agents.html), [ACP](https://bug-ops.github.io/zeph/advanced/acp.html) for IDEs, [A2A](https://bug-ops.github.io/zeph/advanced/a2a.html), [scheduler](https://bug-ops.github.io/zeph/concepts/scheduler.html), and JSON output modes. |

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

For a local-first setup, run [Ollama](https://ollama.ai) and pull the default lightweight models:

```bash
ollama pull qwen3:8b
ollama pull qwen3-embedding
zeph init
zeph
```

## Distributed Inference

Long-running agents are the worst-case workload for centralized API providers: thousands of calls per session, rate limits that pause mid-task, and costs that compound across every tool loop, memory retrieval, and sub-agent spawn.

Distributed inference networks change the economics. Compute is supplied by independent nodes rather than a single data center — which means no shared rate ceiling, no single vendor dependency, and in hardware-attested networks, provable isolation of your prompts from the node operator.

Zeph treats distributed networks as first-class providers alongside [Ollama](https://ollama.ai) and cloud APIs, participating in the same [adaptive routing](https://bug-ops.github.io/zeph/advanced/adaptive-inference.html) — you can send cheap extraction and embedding work to a distributed node while reserving TEE-isolated compute for steps that touch sensitive context.

| Network | Provider type | Characteristic |
|---|---|---|
| [Gonka](https://bug-ops.github.io/zeph/guides/gonka.html) | `gonka` / `compatible` | High-capacity distributed nodes, signed transport, OpenAI-compatible gateway |
| [Cocoon](https://bug-ops.github.io/zeph/guides/cocoon.html) | `cocoon` | Hardware TEE isolation — node operators cannot read prompts or weights |

Both plug into the standard provider declaration:

```toml
[[llm.providers]]
name = "distributed"
type = "gonka"   # or "cocoon", or "compatible" for gateway mode
model = "qwen3-235b"
default = true
```

Run `zeph init` to configure either network interactively through the setup wizard.

## Messenger as Agent Infrastructure

Most agents treat messaging apps as a thin input channel — user sends text, agent replies. Zeph's Telegram integration flips that model: the messenger becomes a coordination layer where agents serve public audiences, accept tasks from orchestrators, and talk to other bots.

**Guest Mode** removes the assumption that every user is a registered Telegram account. A transparent local proxy intercepts guest queries from the [Bot API 10.0](https://core.telegram.org/bots/api) and routes them to the agent without opening a second `getUpdates` connection (no 409 conflicts). The agent responds via [`answerGuestQuery`](https://core.telegram.org/bots/api#answerguestquery) — one call, no extra infra. This makes it practical to deploy public-facing agents that handle anonymous or unauthenticated requests.

**Bot-to-Bot communication** lets Zeph register as a managed bot via [`setManagedBotAccessSettings`](https://core.telegram.org/bots/api#setmanagedbotaccesssettings) and accept tasks from other bots in a controlled chain. Consecutive bot replies are tracked per-chat, depth is capped at `max_bot_chain_depth`, and each inbound bot is validated against an allowlist — so the agent participates in multi-agent pipelines without becoming a relay for arbitrary bots.

**Voice input via [Cocoon](https://bug-ops.github.io/zeph/guides/cocoon.html) STT.** The Telegram adapter detects voice and audio messages, downloads the file, and passes it to the configured speech-to-text provider. With `type = "cocoon"` and `stt_model` set, transcription runs inside a hardware TEE — audio bytes never leave the isolated enclave unencrypted. This makes voice-driven agentic workflows practical for sensitive use cases: a voice note becomes a task, without the audio touching a third-party transcription API.

**Configurable streaming interval** (`stream_interval_ms`, default 3 s, minimum 500 ms) fixes a silent data-loss bug in the original hardcoded delay: responses that completed within a single interval window were discarded before Telegram saw them. Now the agent flushes on completion regardless of the timer.

```toml
[telegram]
guest_mode          = true
bot_to_bot          = true
allowed_bots        = ["orchestrator_bot", "scheduler_bot"]
max_bot_chain_depth = 3
stream_interval_ms  = 1500

[[llm.providers]]
name      = "stt"
type      = "cocoon"
stt_model = "whisper-large-v3"   # transcribes Telegram voice messages inside TEE
```

See the [Telegram guide](https://bug-ops.github.io/zeph/guides/telegram.html) for full configuration and Bot API 10.0 details.

## What Makes It Different

### Memory is the product

Zeph combines several memory layers instead of treating recall as a side feature:

| Layer | Purpose |
|---|---|
| Working context | Keeps the current task coherent under context pressure. See [context budgets](https://bug-ops.github.io/zeph/concepts/context-budgets.html). |
| Semantic memory | Stores conversations, tool outputs, documents, and summaries for retrieval. See [semantic memory guide](https://bug-ops.github.io/zeph/guides/semantic-memory.html). |
| Graph memory | Records entities, decisions, relationships, causality, temporal links, and hierarchy. See [graph memory](https://bug-ops.github.io/zeph/concepts/graph-memory.html). |
| Episodic memory | Preserves session-level scenes, digests, goals, and trajectories. |
| Quality gates | Reject noisy writes, validate compaction, and log retrieval failures for later improvement. See [quality self-check](https://bug-ops.github.io/zeph/advanced/quality-self-check.html). |

Ask "Why did we choose PostgreSQL?" and Zeph can traverse decision edges instead of searching raw chat text.

### Built for low-resource setups

Zeph does not require a heavyweight stack to be useful:

- The default vector backend is embedded [SQLite](https://www.sqlite.org).
- [Qdrant](https://qdrant.tech) is optional for larger semantic and graph workloads.
- The default local chat model is `qwen3:8b` through [Ollama](https://ollama.ai).
- [Feature bundles](https://bug-ops.github.io/zeph/reference/feature-flags.html) let you build only what you need: `desktop`, `ide`, `server`, `chat`, `ml`, or `full`.
- Release builds are optimized for small native binaries.

### Multi-model by design

Declare providers once in `[[llm.providers]]`, then [route work](https://bug-ops.github.io/zeph/advanced/adaptive-inference.html) by complexity, cost, latency, and reliability:

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

Secrets live in the Zeph [age](https://github.com/FiloSottile/age) vault, not in `.env` files or shell profiles. Tool execution goes through trust gates, command filters, [sandboxing](https://bug-ops.github.io/zeph/reference/security/file-sandbox.html), audit logs, and redaction paths. [MCP](https://bug-ops.github.io/zeph/guides/mcp.html) tools are discovered and exposed without dropping the [injection and authorization checks](https://bug-ops.github.io/zeph/reference/security/mcp.html).

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
| Memory | SQLite/PostgreSQL history, embedded SQLite vectors or [Qdrant](https://qdrant.tech), [graph memory](https://bug-ops.github.io/zeph/concepts/graph-memory.html), SYNAPSE, [SleepGate](https://bug-ops.github.io/zeph/advanced/sleep-gate.html), APEX-MEM write-quality gates, BeliefMem probabilistic edge layer, MemCoT Zoom-In/Out recall views, document RAG. |
| Context | [Goal-aware compaction](https://bug-ops.github.io/zeph/advanced/context.html), TypedPage assembler pipeline, TACO output compression, tool-output archive, session recap, active-goal injection. |
| Skills | `SKILL.md` registry, hot reload, BM25 + embedding matching, [trust levels](https://bug-ops.github.io/zeph/advanced/skill-trust.html), [self-learning skill improvement](https://bug-ops.github.io/zeph/guides/self-learning.html). |
| Providers | [Ollama](https://ollama.ai), Claude, OpenAI, Gemini, OpenAI-compatible APIs, [Gonka](https://bug-ops.github.io/zeph/guides/gonka.html) native inference, [Cocoon](https://bug-ops.github.io/zeph/guides/cocoon.html) decentralized TEE inference, [Candle](https://bug-ops.github.io/zeph/advanced/candle.html) local inference, [adaptive routing](https://bug-ops.github.io/zeph/advanced/adaptive-inference.html). |
| Tools | Shell, file, web, [MCP](https://bug-ops.github.io/zeph/guides/mcp.html), tool quotas, approval gates, audit trail, [sandboxing](https://bug-ops.github.io/zeph/reference/security/file-sandbox.html), output compression, speculative dispatch, [ShadowSentinel](https://bug-ops.github.io/zeph/reference/security/shadow-sentinel.html) safety probes, TrajectorySentinel capability governance. |
| Interfaces | CLI, [TUI](https://bug-ops.github.io/zeph/advanced/tui.html), [Telegram](https://bug-ops.github.io/zeph/guides/telegram.html) (with Guest Mode and Bot-to-Bot), Discord, Slack, [ACP](https://bug-ops.github.io/zeph/advanced/acp.html), [A2A](https://bug-ops.github.io/zeph/advanced/a2a.html), HTTP gateway, [scheduler daemon](https://bug-ops.github.io/zeph/concepts/scheduler.html). |
| Code intelligence | [Tree-sitter](https://tree-sitter.github.io/tree-sitter/) indexing, semantic repo map, [LSP](https://bug-ops.github.io/zeph/guides/lsp.html) diagnostics and hover context through [MCP](https://bug-ops.github.io/zeph/guides/mcp.html). |
| Observability | [Debug dumps](https://bug-ops.github.io/zeph/advanced/debug-dump.html), JSONL mode, [Prometheus](https://bug-ops.github.io/zeph/guides/prometheus.html) metrics, [OpenTelemetry](https://opentelemetry.io) traces, profiling builds. |

## Architecture

See the [architecture overview](https://bug-ops.github.io/zeph/architecture/overview.html) and [crates reference](https://bug-ops.github.io/zeph/architecture/crates.html) for full details.

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
- [Memory concepts](https://bug-ops.github.io/zeph/concepts/memory.html) — graph, semantic, episodic layers
- [Graph memory](https://bug-ops.github.io/zeph/concepts/graph-memory.html)
- [Adaptive inference routing](https://bug-ops.github.io/zeph/advanced/adaptive-inference.html)
- [MCP integration](https://bug-ops.github.io/zeph/guides/mcp.html)
- [Security model](https://bug-ops.github.io/zeph/reference/security.html)
- [Feature flags](https://bug-ops.github.io/zeph/reference/feature-flags.html)
- [CLI reference](https://bug-ops.github.io/zeph/reference/cli.html)

Zeph draws from published work on parallel tool execution, temporal knowledge graphs, agentic memory linking, failure-driven compression, retrieval quality, and multi-model routing. See [References & Inspirations](https://bug-ops.github.io/zeph/references.html) for the full list.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md), [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md), and [SECURITY.md](SECURITY.md).

## License

[MIT](LICENSE)
