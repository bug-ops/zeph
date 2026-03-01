<div align="center">
  <img src="asset/zeph_v8_github.png" alt="Zeph" width="800">

  **The AI agent that respects your resources.**

  Single binary. Minimal hardware. Maximum context efficiency.

  [![Crates.io](https://img.shields.io/crates/v/zeph)](https://crates.io/crates/zeph)
  [![docs](https://img.shields.io/badge/docs-book-blue)](https://bug-ops.github.io/zeph/)
  [![CI](https://img.shields.io/github/actions/workflow/status/bug-ops/zeph/ci.yml?branch=main&label=CI)](https://github.com/bug-ops/zeph/actions)
  [![codecov](https://codecov.io/gh/bug-ops/zeph/graph/badge.svg?token=S5O0GR9U6G)](https://codecov.io/gh/bug-ops/zeph)
  [![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)
  [![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
</div>

---

Zeph is a Rust AI agent built around one principle: **every token in the context window must earn its place**. Skills are retrieved semantically, tool output is filtered before injection, and the context compacts automatically under pressure — keeping costs low and responses fast on hardware you already own.

```bash
curl -fsSL https://github.com/bug-ops/zeph/releases/latest/download/install.sh | sh
zeph init   # interactive setup wizard
zeph        # start the agent
```

> [!TIP]
> `cargo install zeph` also works. Pre-built binaries and Docker images are on the [releases page](https://github.com/bug-ops/zeph/releases).

---

## What's inside

| Feature | Description |
|---|---|
| **Hybrid inference** | Ollama, Claude, OpenAI, any OpenAI-compatible API, or fully local via Candle (GGUF). Multi-model orchestrator with fallback chains and EMA latency routing. [→ Providers](https://bug-ops.github.io/zeph/concepts/providers.html) |
| **Skills-first architecture** | YAML+Markdown skill files with BM25+cosine hybrid retrieval. Bayesian re-ranking, 4-tier trust model, and self-learning evolution — skills improve from real usage. [→ Skills](https://bug-ops.github.io/zeph/concepts/skills.html) · [→ Self-learning](https://bug-ops.github.io/zeph/advanced/self-learning.html) |
| **Context engineering** | Semantic skill selection, command-aware output filters, tool-pair summarization, and reactive middle-out compaction keep the window efficient under any load. [→ Context](https://bug-ops.github.io/zeph/advanced/context.html) |
| **Semantic memory** | SQLite + Qdrant with MMR re-ranking, temporal decay, cross-session recall, implicit correction detection, and credential scrubbing. [→ Memory](https://bug-ops.github.io/zeph/concepts/memory.html) |
| **IDE integration (ACP)** | Stdio, HTTP+SSE, or WebSocket transport. Session modes, live tool streaming, LSP diagnostics injection, file following, usage reporting. Works in Zed, Helix, VS Code. [→ ACP](https://bug-ops.github.io/zeph/advanced/acp.html) |
| **Multi-channel I/O** | CLI, Telegram, TUI dashboard — all with streaming. Voice and vision input supported. [→ Channels](https://bug-ops.github.io/zeph/advanced/channels.html) |
| **MCP & A2A** | MCP client with full tool exposure to the model. A2A agent-to-agent protocol for multi-agent orchestration. [→ MCP](https://bug-ops.github.io/zeph/guides/mcp.html) · [→ A2A](https://bug-ops.github.io/zeph/advanced/a2a.html) |
| **Sub-agents** | Spawn isolated agents with scoped tools, skills, and zero-trust secret delegation — defined as Markdown files. [→ Sub-agents](https://bug-ops.github.io/zeph/advanced/sub-agents.html) |
| **Instruction files** | Drop `zeph.md` (or `CLAUDE.md` / `AGENTS.md`) in your project root. Zeph auto-detects and injects them into every system prompt — project rules, conventions, and domain knowledge applied automatically. [→ Instruction Files](https://bug-ops.github.io/zeph/concepts/instruction-files.html) |
| **Defense-in-depth** | Shell sandbox, SSRF protection, skill trust quarantine, secret zeroization, audit logging, `unsafe_code = "deny"` workspace-wide. [→ Security](https://bug-ops.github.io/zeph/reference/security.html) |
| **Document RAG** | `zeph ingest <path>` indexes `.txt`, `.md`, `.pdf` into Qdrant. Relevant chunks surface automatically on each turn. [→ Document loaders](https://bug-ops.github.io/zeph/advanced/document-loaders.html) |
| **Daemon & scheduler** | HTTP webhook gateway with bearer auth, cron-based task scheduler with SQLite persistence, background mode. [→ Daemon](https://bug-ops.github.io/zeph/advanced/daemon.html) |
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
