<div align="center">
  <img src="book/src/assets/zeph_v8_github.png" alt="Zeph" width="800">

  **A Rust AI agent that learns from every session and remembers the reasoning behind every decision.**

  [![Crates.io](https://img.shields.io/crates/v/zeph)](https://crates.io/crates/zeph)
  [![docs](https://img.shields.io/badge/docs-book-blue)](https://bug-ops.github.io/zeph/)
  [![CI](https://img.shields.io/github/actions/workflow/status/bug-ops/zeph/ci.yml?branch=main&label=CI)](https://github.com/bug-ops/zeph/actions)
  [![Tests](https://img.shields.io/badge/tests-7973-brightgreen)](https://github.com/bug-ops/zeph/actions)
  [![codecov](https://codecov.io/gh/bug-ops/zeph/graph/badge.svg?token=S5O0GR9U6G)](https://codecov.io/gh/bug-ops/zeph)
  [![Crates](https://img.shields.io/badge/crates-25-orange)](https://github.com/bug-ops/zeph/tree/main/crates)
  [![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)
  [![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
</div>

---

## Why Zeph

- **Gets smarter with every task** — Wilson score Bayesian confidence tracks which skills actually work in practice. Underperforming skills lose priority; successful ones surface first. When repeated failures cluster around a skill, the agent generates an improved version autonomously — no configuration required.
- **Remembers why, not just what** — Five-typed graph memory (Causal, Temporal, Semantic, CoOccurrence, Hierarchical) with SYNAPSE spreading activation. "Why did we choose PostgreSQL?" traverses causal edges through the decision graph — not keyword search through chat logs.
- **Preserves working memory mid-task** — HiAgent subgoal-aware compaction identifies the current task goal before evicting context. Unlike FIFO trimming, information relevant to the active subgoal is never dropped mid-execution.
- **Routes smarter, spends less** — declare providers once, route by complexity tier (Simple/Medium/Complex/Expert). Thompson Sampling and LinUCB bandit learn which provider wins per query type. Plan template caching reuses successful DAG plans to cut repeated-task cost.
- **Security-first architecture** — age-encrypted vault for all secrets, 17-pattern MCP injection detection, OAP tool authorization, per-session tool quota, SSRF guards, and exfiltration detection — built into the core, not bolted on.


---

## Quick Start

```bash
cargo install zeph
zeph init          # interactive setup wizard — picks provider, model, features
zeph               # start the agent
```

**Tip:** Pre-built binaries and Docker images are on the [releases page](https://github.com/bug-ops/zeph/releases). `curl -fsSL https://github.com/bug-ops/zeph/releases/latest/download/install.sh | sh` also works.

**Tip:** Copy-paste configs for all common setups — local, cloud, hybrid, coding assistant, Telegram bot — are in the **[Configuration Recipes](https://bug-ops.github.io/zeph/guides/config-recipes.html)** guide.

---

## Feature Highlights

- [x] **[Self-learning skills](https://bug-ops.github.io/zeph/advanced/self-learning.html)** — Agent-as-a-Judge feedback detection (fast regex path + rate-limited LLM path), Wilson score Bayesian ranking promotes skills that actually work, autonomous skill evolution triggered by clustered failures, RL-based SleepGate admission control prevents noise from polluting long-term memory
- [x] **[Graph memory with SYNAPSE](https://bug-ops.github.io/zeph/concepts/graph-memory.html)** — five typed edge categories (Causal, Temporal, Semantic, CoOccurrence, Hierarchical) via MAGMA; spreading activation retrieval with hop-by-hop decay and lateral inhibition surfaces multi-hop connections; community detection clusters entities by topic; BFS recall injected alongside vector results each turn
- [x] **[Skills-first architecture](https://bug-ops.github.io/zeph/concepts/skills.html)** — YAML+Markdown skill files, hot-reload on edit, BM25+cosine hybrid retrieval with RRF fusion, Bayesian re-ranking
- [x] **[Context engineering](https://bug-ops.github.io/zeph/advanced/context.html)** — three-tier compaction pipeline, HiAgent subgoal-aware eviction, failure-driven compression guidelines (ACON, ICLR 2026), Memex tool-output archival
- [x] **[Semantic memory](https://bug-ops.github.io/zeph/concepts/memory.html)** — SQLite or PostgreSQL + Qdrant, MMR re-ranking, temporal decay, semantic response cache
- [x] **[Multi-model orchestration](https://bug-ops.github.io/zeph/advanced/complexity-triage.html)** — complexity triage routing (Simple/Medium/Complex/Expert), Thompson Sampling, cascade cost tiers, PILOT LinUCB bandit
- [x] **[Hybrid inference](https://bug-ops.github.io/zeph/concepts/providers.html)** — Ollama, Claude, OpenAI, Gemini, any OpenAI-compatible API, or fully local via Candle (GGUF)
- [x] **[Task orchestration](https://bug-ops.github.io/zeph/concepts/task-orchestration.html)** — DAG-based task graphs with LLM goal decomposition, parallel execution, plan template caching
- [x] **[MCP client](https://bug-ops.github.io/zeph/guides/mcp.html)** — full tool exposure, OAuth 2.1 + PKCE for remote servers, 17-pattern injection detection, per-session tool quota, OAP authorization
- [x] **[Security sandbox](https://bug-ops.github.io/zeph/reference/security.html)** — age-encrypted vault, shell sandbox, file read sandbox, SSRF protection, PII filter, exfiltration guards
- [x] **[ACP server](https://bug-ops.github.io/zeph/advanced/acp.html)** — stdio, HTTP+SSE, WebSocket transports for IDE integration (Zed, VS Code, Helix)
- [x] **[A2A protocol](https://bug-ops.github.io/zeph/advanced/a2a.html)** — agent-to-agent delegation over JSON-RPC 2.0 with IBCT capability tokens
- [x] **[Sub-agents](https://bug-ops.github.io/zeph/advanced/sub-agents.html)** — isolated agents with scoped tools, zero-trust secret delegation, persistent transcripts
- [x] **[TUI dashboard](https://bug-ops.github.io/zeph/advanced/tui.html)** — ratatui-based with real-time metrics, security panel, plan view, command palette
- [x] **[Multi-channel I/O](https://bug-ops.github.io/zeph/advanced/channels.html)** — CLI, Telegram, TUI, Discord, Slack — all with streaming, voice, and vision input
- [x] **[LSP integration](https://bug-ops.github.io/zeph/guides/lsp.html)** — compiler-level code intelligence via rust-analyzer, pyright, gopls and others: type info, diagnostics, call hierarchy, safe rename, references — injected automatically into context after file writes and reads
- [x] **[Code indexing](https://bug-ops.github.io/zeph/advanced/code-indexing.html)** — tree-sitter AST-based indexing (Rust, Python, JS, TS, Go), semantic search, repo map generation
- [x] **[Document RAG](https://bug-ops.github.io/zeph/advanced/document-loaders.html)** — ingest `.txt`, `.md`, `.pdf` into Qdrant with automatic retrieval per turn
- [x] **[Self-experimentation](https://bug-ops.github.io/zeph/concepts/experiments.html)** — autonomous LLM config tuning via grid sweep, random sampling, neighborhood search
- [x] **[Config migration](https://bug-ops.github.io/zeph/guides/migrate-config.html)** — `zeph migrate-config --diff` previews and applies config upgrades after version bumps
- [x] **Single binary** -- ~15 MB, no runtime dependencies, ~50 ms startup, ~20 MB idle memory

---

## Architecture

```text
zeph (binary)
 |
 +-- zeph-core            agent loop, context builder, metrics, channel trait
 |    |
 |    +-- zeph-config     TOML config, env overrides, migration, init wizard
 |    +-- zeph-db         SQLite/PostgreSQL pool, migrations, store trait
 |    +-- zeph-vault      age-encrypted secret storage, vault resolution
 |    +-- zeph-common     shared types, error utilities, tracing helpers
 |    +-- zeph-sanitizer  content sanitization, injection detection, PII filter
 |    |
 |    +-- zeph-llm        LlmProvider trait, Ollama/Claude/OpenAI/Gemini/Candle backends
 |    +-- zeph-skills     SKILL.md parser, registry, embedding matcher, self-learning
 |    +-- zeph-memory     semantic memory orchestrator, graph memory, SYNAPSE
 |    +-- zeph-tools      ToolExecutor trait, shell/web/file/composite executors, audit
 |    +-- zeph-mcp        MCP client, multi-server lifecycle, tool registry
 |    +-- zeph-orchestration  DAG task graphs, planner, scheduler, aggregator
 |    +-- zeph-subagent   sub-agent spawner, transcript persistence, lifecycle hooks
 |    +-- zeph-index      AST code indexing, semantic retrieval, repo map
 |
 +-- zeph-channels        CLI, Telegram, Discord, Slack adapters
 +-- zeph-tui             ratatui TUI dashboard (feature-gated)
 +-- zeph-acp             ACP server: stdio/HTTP+SSE/WebSocket (feature-gated)
 +-- zeph-a2a             A2A client + server, agent discovery (feature-gated)
 +-- zeph-gateway         HTTP webhook gateway with bearer auth (feature-gated)
 +-- zeph-scheduler       cron-based periodic tasks (feature-gated)
 +-- zeph-experiments     autonomous LLM config experimentation engine
```

Optional features are grouped into use-case bundles: `desktop` (TUI), `ide` (ACP), `server` (gateway + A2A + otel), `chat` (Discord + Slack), `ml` (Candle + PDF). Use `--features full` for everything except hardware-specific GPU flags. See [Feature Flags](https://bug-ops.github.io/zeph/reference/feature-flags.html).

---

```text
┌─ Skills (3/12) ────────────────────┐┌─ MCP Tools ─────────────────────────┐
│  web-search  [████████░░] 82% (117)││  - filesystem/read_file             │
│  git-commit  [███████░░░] 73%  (42)││  - filesystem/write_file            │
│  code-review [████░░░░░░] 41%   (8)││  - github/create_pr                 │
└────────────────────────────────────┘└─────────────────────────────────────┘
```

<div align="center">
  <img src="book/src/assets/zeph.gif" alt="Zeph TUI Dashboard" width="800">
</div>

## Documentation

Full documentation — installation, configuration, guides, and architecture reference — at **[bug-ops.github.io/zeph](https://bug-ops.github.io/zeph/)**.

Zeph's design draws from a broad range of published research: parallel tool execution ([LLMCompiler, ICML 2024](https://arxiv.org/abs/2312.04511)), failure-driven context compression ([ACON, ICLR 2026](https://arxiv.org/abs/2510.00615)), temporal knowledge graphs ([Zep/Graphiti, 2025](https://arxiv.org/abs/2501.13956)), agentic memory linking ([A-MEM, NeurIPS 2025](https://arxiv.org/abs/2502.12110)), observation masking and schema-based summarization ([Manus, 2025](https://rlancemartin.github.io/2025/10/15/manus/)), and more. The full list of papers, blog posts, and specifications that shaped Zeph is at **[References & Inspirations](https://bug-ops.github.io/zeph/references.html)**.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Found a vulnerability? Use [GitHub Security Advisories](https://github.com/bug-ops/zeph/security/advisories/new).

## License

[MIT](LICENSE)
