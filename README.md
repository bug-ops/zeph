<div align="center">
  <img src="asset/zeph_v8_github.png" alt="Zeph" width="800">

  **The AI agent that respects your resources.**

  Single binary. Minimal hardware. Maximum context efficiency.

  [![Crates.io](https://img.shields.io/crates/v/zeph)](https://crates.io/crates/zeph)
  [![docs](https://img.shields.io/badge/docs-book-blue)](https://bug-ops.github.io/zeph/)
  [![CI](https://img.shields.io/github/actions/workflow/status/bug-ops/zeph/ci.yml?branch=main&label=CI)](https://github.com/bug-ops/zeph/actions)
  [![Tests](https://img.shields.io/badge/tests-4993-brightgreen)](https://github.com/bug-ops/zeph/actions)
  [![codecov](https://codecov.io/gh/bug-ops/zeph/graph/badge.svg?token=S5O0GR9U6G)](https://codecov.io/gh/bug-ops/zeph)
  [![Crates](https://img.shields.io/badge/crates-14-orange)](https://github.com/bug-ops/zeph/tree/main/crates)
  [![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)
  [![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
</div>

---

Zeph is a Rust AI agent built around one principle: **every token in the context window must earn its place**. Skills are retrieved semantically, tool output is filtered before injection, and the context compacts automatically under pressure — keeping costs low and responses fast on hardware you already own.

```bash
curl -fsSL https://github.com/bug-ops/zeph/releases/latest/download/install.sh | sh
zeph init                                            # interactive setup wizard
zeph migrate-config --config config.toml --diff      # check for new options after upgrade
zeph                                                 # start the agent
```

> [!TIP]
> `cargo install zeph` also works. Pre-built binaries and Docker images are on the [releases page](https://github.com/bug-ops/zeph/releases).

---

## What's inside

| Feature | Description |
|---|---|
| **Hybrid inference** | Ollama, Claude, OpenAI, any OpenAI-compatible API, or fully local via Candle (GGUF). Multi-model orchestrator with fallback chains, EMA latency routing, and adaptive Thompson Sampling for exploration/exploitation-balanced model selection. [→ Providers](https://bug-ops.github.io/zeph/concepts/providers.html) |
| **Skills-first architecture** | YAML+Markdown skill files with BM25+cosine hybrid retrieval. Bayesian re-ranking, 4-tier trust model, and self-learning evolution — skills improve from real usage. Agent-as-a-Judge feedback detection with adaptive regex/LLM hybrid analysis. The `load_skill` tool lets the LLM fetch the full body of any skill outside the active TOP-N set on demand. [→ Skills](https://bug-ops.github.io/zeph/concepts/skills.html) · [→ Self-learning](https://bug-ops.github.io/zeph/advanced/self-learning.html) |
| **Context engineering** | Semantic skill selection, command-aware output filters, tool-pair summarization with deferred application (pre-computed eagerly, applied lazily to stabilize the Claude API prompt cache prefix), proactive context compression (reactive + proactive strategies), and reactive middle-out compaction keep the window efficient under any load. Three-tier compaction pipeline: deferred summary application at 70% context usage → pruning at 80% → LLM compaction on overflow. `--debug-dump [PATH]` writes every LLM request, response, and raw tool output to numbered files for context debugging. [→ Context](https://bug-ops.github.io/zeph/advanced/context.html) · [→ Debug Dump](https://bug-ops.github.io/zeph/advanced/debug-dump.html) |
| **Semantic memory** | SQLite + Qdrant with MMR re-ranking, temporal decay, query-aware memory routing (keyword/semantic/hybrid), cross-session recall, implicit correction detection, and credential scrubbing. Optional **graph memory** adds entity-relationship tracking with FTS5-accelerated entity search, BFS traversal for multi-hop reasoning, temporal fact tracking, and embedding-based entity resolution for semantic deduplication. Background LLM extraction runs fire-and-forget on each turn; graph facts are injected into the context window alongside semantic recall. [→ Memory](https://bug-ops.github.io/zeph/concepts/memory.html) · [→ Graph Memory](https://bug-ops.github.io/zeph/concepts/graph-memory.html) |
| **IDE integration (ACP)** | Stdio, HTTP+SSE, or WebSocket transport. Multi-session isolation with per-session conversation history and SQLite persistence. Session modes, live tool streaming, LSP diagnostics injection, file following, usage reporting. Works in Zed, Helix, VS Code. [→ ACP](https://bug-ops.github.io/zeph/advanced/acp.html) |
| **Multi-channel I/O** | CLI, Telegram, TUI dashboard — all with streaming. Voice and vision input supported. [→ Channels](https://bug-ops.github.io/zeph/advanced/channels.html) |
| **MCP & A2A** | MCP client with full tool exposure to the model. Configure [mcpls](https://github.com/bug-ops/mcpls) as an MCP server for compiler-level code intelligence: hover, definition, references, diagnostics, call hierarchy, and safe rename via rust-analyzer, pyright, gopls, and 30+ other LSP servers. A2A agent-to-agent protocol for multi-agent orchestration. [→ MCP](https://bug-ops.github.io/zeph/guides/mcp.html) · [→ LSP](https://bug-ops.github.io/zeph/guides/lsp.html) · [→ A2A](https://bug-ops.github.io/zeph/advanced/a2a.html) |
| **LSP context injection** | Automatically injects LSP-derived context into the agent after tool calls — no explicit tool invocation needed. Three hooks: diagnostics after `write_file` (compiler errors surfaced as the next turn's context), hover info after `read_file` (pre-fetched for key symbols), and reference listing before `rename_symbol` (shows all call sites). Operates through the existing mcpls MCP server with graceful degradation when mcpls is unavailable. Token budget enforced per turn. Enabled by the `lsp-context` feature flag (included in `full`). [→ LSP Context Injection](https://bug-ops.github.io/zeph/concepts/lsp-context-injection.html) |
| **Sub-agents** | Spawn isolated agents with scoped tools, skills, and zero-trust secret delegation — defined as Markdown files. 4-level resolution priority (CLI > project > user > config), `permission_mode` (`default`/`accept_edits`/`dont_ask`/`bypass_permissions`/`plan`), fine-grained `tools.except` denylists, `background` fire-and-forget execution, `max_turns` limits, persistent memory scopes (`user`/`project`/`local`) with MEMORY.md injection, persistent JSONL transcript storage with `/agent resume` for continuing completed sessions, and lifecycle hooks (`SubagentStart`/`SubagentStop` at config level, `PreToolUse`/`PostToolUse` per agent with pipe-separated matchers). Manage definitions with `zeph agents list|show|create|edit|delete` (CLI) or the interactive agents panel in the TUI. [→ Sub-agents](https://bug-ops.github.io/zeph/advanced/sub-agents.html) |
| **Instruction files** | Drop `zeph.md` (or `CLAUDE.md` / `AGENTS.md`) in your project root. Zeph auto-detects and injects them into every system prompt — project rules, conventions, and domain knowledge applied automatically. Changes are picked up live via filesystem watching (500 ms debounce) — no restart required. [→ Instruction Files](https://bug-ops.github.io/zeph/concepts/instruction-files.html) |
| **Defense-in-depth** | Shell sandbox, SSRF protection, skill trust quarantine, secret zeroization, audit logging, `unsafe_code = "deny"` workspace-wide. Untrusted content isolation: all tool results, web scrape output, MCP responses, A2A messages, and memory retrieval pass through a `ContentSanitizer` pipeline that truncates, strips control characters, detects 17 injection patterns, and wraps content in spotlighting XML delimiters before it enters the LLM context. Optional **quarantined summarizer** (Dual LLM pattern) routes high-risk sources through an isolated, tool-less LLM extraction call for defense-in-depth against indirect prompt injection. **Exfiltration guards** block markdown image pixel-tracking, validate tool call URLs against flagged untrusted sources, and suppress memory writes for injection-flagged content. TUI security panel with real-time event feed, SEC status bar indicator, and `security:events` command palette entry. [→ Security](https://bug-ops.github.io/zeph/reference/security.html) · [→ Untrusted Content Isolation](https://bug-ops.github.io/zeph/reference/security/untrusted-content-isolation.html) |
| **Document RAG** | `zeph ingest <path>` indexes `.txt`, `.md`, `.pdf` into Qdrant. Relevant chunks surface automatically on each turn. [→ Document loaders](https://bug-ops.github.io/zeph/advanced/document-loaders.html) |
| **Task orchestration** | DAG-based task graphs with dependency tracking, parallel execution, configurable failure strategies (abort/retry/skip/ask), timeout enforcement, and SQLite persistence. LLM-powered goal decomposition via `Planner` trait with structured output. Tick-based `DagScheduler` execution engine with command pattern, `AgentRouter` trait for task-to-agent routing, cross-task context injection with `ContentSanitizer` integration, and stale event guards. LLM-backed result aggregation (`Aggregator` trait, `LlmAggregator`) synthesizes completed task outputs into a single coherent response with per-task token budgeting and raw-concatenation fallback. `/plan` CLI commands: `/plan <goal>` (decompose + confirm + execute), `/plan status`, `/plan list`, `/plan cancel`, `/plan confirm`, `/plan resume [id]` (resume a paused Ask-strategy graph), `/plan retry [id]` (re-run failed tasks). TUI Plan View side panel (press `p`) shows live task status with per-row spinners and status colors; five `plan:*` command palette entries available. `OrchestrationMetrics` tracked in `MetricsSnapshot`. [→ Orchestration](https://bug-ops.github.io/zeph/concepts/task-orchestration.html) |
| **Benchmark & evaluation** | TOML benchmark datasets with LLM-as-Judge scoring. Run any subject model against a `BenchmarkSet`, score responses in parallel via a separate judge model with structured JSON output (`JudgeOutput` schema), and collect aggregate `EvalReport` metrics (mean score, p50/p95 latency, per-case token tracking). Budget enforcement via atomic token counter, semaphore-based concurrency, and XML boundary tags for subject response isolation. Enabled by the `experiments` feature flag (included in `full`). |
| **Daemon & scheduler** | HTTP webhook gateway with bearer auth. Cron-based periodic tasks and one-shot deferred tasks with SQLite persistence — add, update, or cancel tasks at runtime via natural language using the built-in `scheduler` skill. Experiment sessions can run on a cron schedule via `TaskKind::Experiment`, combining `scheduler` and `experiments` feature flags. Background mode. [→ Daemon](https://bug-ops.github.io/zeph/advanced/daemon.html) |
| **Self-experimentation** | Autonomous LLM config experimentation engine (inspired by [autoresearch](https://github.com/karpathy/autoresearch)). Parameter variation engine with pluggable strategies (grid sweep, random sampling, neighborhood search) explores temperature, top-p, top-k, frequency/presence penalty one parameter at a time, evaluates each variant via LLM-as-judge scoring, and keeps improvements that pass a configurable threshold. `SearchSpace` defines per-parameter ranges and step sizes; `ConfigSnapshot` captures the full LLM config for reproducible rollback. Scheduled runs via cron with `ExperimentSchedule` config. CLI flags: `--experiment-run` (run a single experiment session and exit), `--experiment-report` (print results summary and exit). TUI `/experiment` commands: `start`, `stop`, `status`, `report`, `best`. Interactive setup via `zeph init` wizard. Enabled by the `experiments` feature flag (opt-in, included in `full`). |
| **Config migration** | `zeph migrate-config [--config PATH] [--in-place] [--diff]` — upgrades an existing config file after a version bump. Missing sections are appended as commented-out blocks with documentation; existing values are never touched. Output is idempotent and can be previewed with `--diff` before applying. [→ Migrate Config](https://bug-ops.github.io/zeph/guides/migrate-config.html) |
| **Single binary** | ~15 MB, no runtime dependencies, ~50 ms startup, ~20 MB idle memory. |

```text
┌─ Skills (3/12) ────────────────────┐┌─ MCP Tools ─────────────────────────┐
│  web-search  [████████░░] 82% (117)││  - filesystem/read_file             │
│  git-commit  [███████░░░] 73%  (42)││  - filesystem/write_file            │
│  code-review [████░░░░░░] 41%   (8)││  - github/create_pr                 │
└────────────────────────────────────┘└─────────────────────────────────────┘
```

<div align="center">
  <img src="asset/zeph.gif" alt="Zeph TUI Dashboard" width="800">
</div>

## Documentation

Full documentation — installation, configuration, guides, and architecture reference — at **[bug-ops.github.io/zeph](https://bug-ops.github.io/zeph/)**.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Found a vulnerability? Use [GitHub Security Advisories](https://github.com/bug-ops/zeph/security/advisories/new).

## License

[MIT](LICENSE)
