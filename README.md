<div align="center">
  <img src="book/src/assets/zeph_v8_github.png" alt="Zeph" width="800">

  **A Rust AI agent that makes every token in the context window earn its place.**

  [![Crates.io](https://img.shields.io/crates/v/zeph)](https://crates.io/crates/zeph)
  [![docs](https://img.shields.io/badge/docs-book-blue)](https://bug-ops.github.io/zeph/)
  [![CI](https://img.shields.io/github/actions/workflow/status/bug-ops/zeph/ci.yml?branch=main&label=CI)](https://github.com/bug-ops/zeph/actions)
  [![Tests](https://img.shields.io/badge/tests-7818-brightgreen)](https://github.com/bug-ops/zeph/actions)
  [![codecov](https://codecov.io/gh/bug-ops/zeph/graph/badge.svg?token=S5O0GR9U6G)](https://codecov.io/gh/bug-ops/zeph)
  [![Crates](https://img.shields.io/badge/crates-21-orange)](https://github.com/bug-ops/zeph/tree/main/crates)
  [![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)
  [![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
</div>

---

## Why Zeph

- **Adaptive skill routing** -- LinUCB contextual bandit and RL-based routing learn which skills and providers work best for each query type, improving over time without manual tuning.
- **Automatic memory management** -- SleepGate admission control, graph memory with SYNAPSE spreading activation, and goal-conditioned writes keep long-term memory relevant without user intervention.
- **Multi-model orchestration** -- declare providers once, route by complexity tier (Simple/Medium/Complex/Expert). Thompson Sampling, cascade routing, and PILOT bandit selection minimize cost while maximizing quality.
- **21-crate modular architecture** -- every subsystem is a standalone crate with a clean trait boundary. Feature-gate what you need: TUI, ACP, A2A, MCP, Telegram, Discord, Slack, gateway, scheduler, PDF, STT, Candle inference.
- **Context engineering, not prompt engineering** -- three-tier compaction pipeline, HiAgent subgoal-aware eviction, failure-driven compression guidelines (ACON), and Memex tool-output archival keep the context window efficient under any load.

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

- [x] **Hybrid inference** -- Ollama, Claude, OpenAI, Gemini, any OpenAI-compatible API, or fully local via Candle (GGUF)
- [x] **Skills-first architecture** -- YAML+Markdown skill files with BM25+cosine hybrid retrieval, Bayesian re-ranking, self-learning evolution
- [x] **Semantic memory** -- SQLite or PostgreSQL + Qdrant, MMR re-ranking, temporal decay, graph memory with SYNAPSE spreading activation
- [x] **MCP client** -- full tool exposure, 17-pattern injection detection, elicitation support, tool-list locking
- [x] **A2A protocol** -- agent-to-agent delegation over JSON-RPC 2.0 with IBCT capability tokens
- [x] **ACP server** -- stdio, HTTP+SSE, WebSocket transports for IDE integration (Zed, VS Code, Helix)
- [x] **TUI dashboard** -- ratatui-based with real-time metrics, security panel, plan view, command palette
- [x] **Multi-channel I/O** -- CLI, Telegram, TUI, Discord, Slack — all with streaming, voice, and vision input
- [x] **Multi-model orchestration** -- complexity triage routing, Thompson Sampling, cascade cost tiers, PILOT LinUCB bandit
- [x] **Security sandbox** -- shell sandbox with structured output, file read sandbox, SSRF protection, PII filter, rate limiter, exfiltration guards
- [x] **Self-learning** -- Agent-as-a-Judge feedback detection, skill evolution from real usage, RL admission control for memory writes
- [x] **Task orchestration** -- DAG-based task graphs with LLM goal decomposition, parallel execution, plan template caching
- [x] **Sub-agents** -- isolated agents with scoped tools, zero-trust secret delegation, persistent transcripts
- [x] **Code indexing** -- tree-sitter AST-based indexing (Rust, Python, JS, TS, Go), semantic search, repo map generation
- [x] **Document RAG** -- ingest `.txt`, `.md`, `.pdf` into Qdrant with automatic retrieval per turn
- [x] **Self-experimentation** -- autonomous LLM config tuning via grid sweep, random sampling, neighborhood search
- [x] **Config migration** -- `zeph migrate-config --diff` previews and applies config upgrades after version bumps
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
