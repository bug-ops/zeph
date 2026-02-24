<div align="center">
  <img src="asset/zeph_v8_github.png" alt="Zeph" width="800">

  **The AI agent that respects your resources.**

  Single binary. Minimal hardware. Maximum context efficiency.<br>
  Every token counts — Zeph makes sure none are wasted.

  [![Crates.io](https://img.shields.io/crates/v/zeph)](https://crates.io/crates/zeph)
  [![docs](https://img.shields.io/badge/docs-book-blue)](https://bug-ops.github.io/zeph/)
  [![CI](https://img.shields.io/github/actions/workflow/status/bug-ops/zeph/ci.yml?branch=main&label=CI)](https://github.com/bug-ops/zeph/actions)
  [![codecov](https://codecov.io/gh/bug-ops/zeph/graph/badge.svg?token=S5O0GR9U6G)](https://codecov.io/gh/bug-ops/zeph)
  [![Trivy](https://img.shields.io/badge/Trivy-0%20CVEs-success)](https://github.com/bug-ops/zeph/security)
  [![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)
  [![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
</div>

---

## Why Zeph

Most AI agent frameworks dump every tool description, skill, and raw output into the context window — and bill you for it. Zeph takes the opposite approach: **automated context engineering**. Only relevant data enters the context. The result — lower costs, faster responses, and an agent that runs on hardware you already have.

- **Semantic skill selection** — embeds skills as vectors, retrieves only top-K relevant per query instead of injecting all
- **Smart output filtering** — command-aware filters strip 70-99% of noise before context injection; oversized responses offloaded to filesystem
- **Resilient context compaction** — reactive retry on context overflow, middle-out progressive tool response removal, 9-section structured compaction prompt, LLM-free metadata fallback
- **Accurate token counting** — tiktoken-based cl100k_base tokenizer with DashMap cache replaces chars/4 heuristic
- **Proportional budget allocation** — context space distributed by purpose, not arrival order

## Installation

> [!TIP]
> ```bash
> curl -fsSL https://github.com/bug-ops/zeph/releases/latest/download/install.sh | sh
> ```

<details>
<summary>Other methods</summary>

```bash
cargo install zeph                                        # crates.io
cargo install --git https://github.com/bug-ops/zeph      # from source
docker pull ghcr.io/bug-ops/zeph:latest                  # Docker
```

Pre-built binaries: [GitHub Releases](https://github.com/bug-ops/zeph/releases/latest) · [Docker guide](https://bug-ops.github.io/zeph/guides/docker.html)

</details>

## Quick Start

```bash
zeph init          # interactive setup wizard
zeph               # run the agent
zeph --tui         # run with TUI dashboard
```

[Full setup guide →](https://bug-ops.github.io/zeph/getting-started/installation.html) · [Configuration reference →](https://bug-ops.github.io/zeph/reference/configuration.html)

## Key Features

| | |
|---|---|
| **Hybrid inference** | Ollama, Claude, OpenAI, Candle (GGUF), any OpenAI-compatible API. Multi-model orchestrator with fallback chains. Response cache with blake3 hashing and TTL |
| **Skills-first architecture** | YAML+Markdown skill files with semantic matching, self-learning evolution, 4-tier trust model, and compact prompt mode for small-context models |
| **Semantic memory** | SQLite + Qdrant (or embedded SQLite vector search) with MMR re-ranking, temporal decay scoring, resilient compaction (reactive retry, middle-out tool response removal, 9-section structured prompt, LLM-free fallback), durable compaction with message visibility control, credential scrubbing, cross-session recall, vector retrieval, autosave assistant responses, and snapshot export/import |
| **Multi-channel I/O** | CLI, Telegram, Discord, Slack, TUI — all with streaming. Vision and speech-to-text input |
| **Protocols** | MCP client (stdio + HTTP), A2A agent-to-agent communication, ACP server for IDE integration (multi-session, persistence, idle reaper, permission persistence, multi-modal prompts with image forwarding), sub-agent orchestration |
| **Defense-in-depth** | Shell sandbox, tool permissions, secret redaction, SSRF protection, skill trust quarantine, audit logging |
| **TUI dashboard** | ratatui-based with syntax highlighting, live metrics, file picker, command palette, daemon mode |
| **Single binary** | ~15 MB, no runtime dependencies, ~50ms startup, ~20 MB idle memory |

[Architecture →](https://bug-ops.github.io/zeph/architecture/overview.html) · [Feature flags →](https://bug-ops.github.io/zeph/reference/feature-flags.html) · [Security model →](https://bug-ops.github.io/zeph/reference/security.html)

## TUI Demo

<div align="center">
  <img src="asset/zeph.gif" alt="Zeph TUI Dashboard" width="800">
</div>

[TUI guide →](https://bug-ops.github.io/zeph/advanced/tui.html)

## Documentation

**[bug-ops.github.io/zeph](https://bug-ops.github.io/zeph/)** — installation, configuration, guides, architecture, and API reference.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development workflow and guidelines.

## Security

Found a vulnerability? Please use [GitHub Security Advisories](https://github.com/bug-ops/zeph/security/advisories/new) for responsible disclosure.

## License

[MIT](LICENSE)
