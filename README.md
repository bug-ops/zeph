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
- **Tool-pair summarization** — when visible tool call/response pairs exceed a configurable cutoff, the oldest pair is summarized via LLM and originals hidden from context
- **Accurate token counting** — tiktoken-based cl100k_base tokenizer with DashMap cache replaces chars/4 heuristic
- **Proportional budget allocation** — context space distributed by purpose, not arrival order
- **Optimized agent loop hot-path** — compaction check is O(1) via cached token count; `EnvironmentContext` built once at bootstrap and partially refreshed on skill reload; doom-loop hashing done in-place with no intermediate allocations; token counting for tool output pruning reduced to a single call per part

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
| **Hybrid inference** | Ollama, Claude, OpenAI, Candle (GGUF), any OpenAI-compatible API. Multi-model orchestrator with fallback chains. Response cache with blake3 hashing and TTL. Ollama native tool calling via `llm.ollama.tool_use = true` |
| **Skills-first architecture** | YAML+Markdown skill files with semantic matching, self-learning evolution, 4-tier trust model, and compact prompt mode for small-context models |
| **Semantic memory** | SQLite + Qdrant (or embedded SQLite vector search) with MMR re-ranking, temporal decay scoring, resilient compaction (reactive retry, middle-out tool response removal, 9-section structured prompt, LLM-free fallback), durable compaction with message visibility control, tool-pair summarization (LLM-based, configurable cutoff), credential scrubbing, cross-session recall, vector retrieval, autosave assistant responses, snapshot export/import, configurable SQLite pool, background response-cache cleanup, and native `memory_search`/`memory_save` tools the model can invoke explicitly |
| **Multi-channel I/O** | CLI, Telegram, Discord, Slack, TUI — all with streaming. Vision and speech-to-text input |
| **Protocols** | MCP client (stdio + HTTP), A2A agent-to-agent communication, ACP server for IDE integration (stdio + HTTP+SSE + WebSocket, multi-session with LRU eviction, persistence, idle reaper, permission persistence, multi-modal prompts, runtime model switching, session modes (ask/architect/code), MCP server management via `ext_method`, session export/import, tool call lifecycle notifications, terminal command timeout with kill support, `UserMessageChunk` echo, `ext_notification` passthrough, `list`/`fork`/`resume` sessions behind unstable flags), sub-agent orchestration with zero-trust secret delegation. MCP tools exposed as native `ToolDefinition`s — used via structured tool_use with Claude and OpenAI |
| **Defense-in-depth** | Shell sandbox (blocklist + confirmation patterns for process substitution, here-strings, eval), tool permissions, secret redaction, SSRF protection (HTTPS-only, DNS validation, address pinning, redirect chain re-validation), skill trust quarantine, audit logging. Secrets held in memory as `Zeroizing<String>` — wiped on drop |
| **TUI dashboard** | ratatui-based with syntax highlighting, live metrics, file picker, command palette, daemon mode |
| **Single binary** | ~15 MB, no runtime dependencies, ~50ms startup, ~20 MB idle memory |

[Architecture →](https://bug-ops.github.io/zeph/architecture/overview.html) · [Feature flags →](https://bug-ops.github.io/zeph/reference/feature-flags.html) · [Security model →](https://bug-ops.github.io/zeph/reference/security.html)

## IDE Integration (ACP)

Zeph implements the [Agent Client Protocol](https://agentclientprotocol.com/) — use it as an AI backend in Zed, Helix, VS Code, or any ACP-compatible editor via stdio, HTTP+SSE, or WebSocket transport.

```bash
zeph acp                    # stdio (editor spawns as subprocess)
zeph acp --http :8080       # HTTP+SSE (shared/remote)
zeph acp --ws :8080         # WebSocket
```

**ACP capabilities:**

- Session modes: `ask`, `code`, `architect` — switch at runtime via `set_session_mode`; editors receive `current_mode_update` notifications
- Tool call lifecycle: `InProgress` → `Completed` updates with `ToolCallContent::Terminal` for shell calls; terminal release deferred until after the `tool_call_update` notification so IDE can display tool output
- Terminal command timeout (default 120 s, configurable via `terminal_timeout_secs`) with `kill_terminal_command` support
- `UserMessageChunk` echo notification after each user prompt
- `ext_notification` passthrough to running sessions
- `AgentCapabilities` advertises `session_capabilities`: `list`, `fork`, `resume`
- MCP HTTP transport support in the MCP bridge
- Unsupported content blocks (Audio, ResourceLink) produce structured log warnings instead of silent drops
- **Usage reporting** — token counts (input, output, cache) streamed back to the IDE as `UsageUpdate` session notifications after each turn; IDEs that support `UsageUpdate` render this as a context percentage badge (`unstable-session-usage`, enabled by default)
- **Loaded rules reporting** — skill paths and system rule files discovered at session start are sent to the IDE via `_meta.projectRules` in `NewSessionResponse`; IDEs that implement this extension display an **N project rules** badge
- **IDE model picker** — `SetSessionModel` lets the editor switch the active model via a native dropdown without a custom `session/configure` call (`unstable-session-model`)
- **Auto session title** — `SessionInfoUpdate` notifies the IDE of an agent-generated session title after the first turn (`unstable-session-info-update`)
- **Plan updates** — `SessionUpdate::Plan` events emitted during orchestrator runs so the editor can display intermediate planning steps
- **Slash commands** — `AvailableCommandsUpdate` advertises built-in slash commands (`/help`, `/model`, `/mode`, `/clear`, `/compact`); user input starting with `/` is dispatched to the matching handler
- **LSP diagnostics injection** — `@diagnostics` mention in a Zed prompt triggers LSP diagnostic context injection, providing the agent with current editor diagnostics
- **Session history** — `GET /sessions` lists persisted sessions with title, timestamp, and message count; `GET /sessions/{id}/messages` returns the full event log; sending an existing `session_id` resumes the conversation from stored context; title auto-inferred from the first user message
- **Subagent nesting** — sub-agent output is nested under the parent tool call in the IDE via `_meta.claudeCode.parentToolUseId` carried on every `session_update`, so multi-agent runs appear as collapsible trees in Zed and VS Code ACP tool cards
- **Terminal streaming** — `AcpShellExecutor` streams bash output in real time via `_meta.terminal_output` chunks with a final `_meta.terminal_exit` event; IDEs display live output inside the tool card as commands execute
- **File following** — `ToolCall.location` carries `filePath` for file read/write operations; the IDE editor cursor tracks the agent across files automatically

> [!NOTE]
> `list_sessions`, `fork_session`, and `resume_session` are gated behind the `unstable` feature flag. `UsageUpdate`, `SetSessionModel`, and `SessionInfoUpdate` are gated behind their respective `unstable-session-usage`, `unstable-session-model`, and `unstable-session-info-update` flags.

### WebSocket transport hardening

The WebSocket transport is hardened against a range of protocol and concurrency issues:

| Property | Value |
|---|---|
| Max concurrent sessions | Configurable; enforced with atomic slot reservation (eliminates TOCTOU race) |
| Keepalive | 30 s ping interval / 90 s pong timeout — idle connections are closed |
| Max message size | 1 MiB |
| Binary frames | Rejected with close code `1003 Unsupported Data` (text-only protocol) |
| Disconnect drain | Write task given 1 s to deliver the RFC 6455 close frame before the socket is dropped |

### Bearer token authentication

Protect the `/acp` (HTTP+SSE) and `/acp/ws` (WebSocket) endpoints with a static bearer token. The discovery endpoint is always exempt.

```toml
# config.toml
[acp]
auth_bearer_token = "your-secret-token"
```

| Method | Value |
|---|---|
| Config key | `acp.auth_bearer_token` |
| Environment variable | `ZEPH_ACP_AUTH_TOKEN` |
| CLI flag | `--acp-auth-token <token>` |

Requests to guarded routes without a valid `Authorization: Bearer <token>` header receive `401 Unauthorized`. When no token is configured, the server runs in open mode — no authentication is enforced. stdio transport is always unaffected.

> [!TIP]
> Always set `auth_bearer_token` when exposing the ACP server on a network interface. For local-only stdio or single-user setups no token is required.

> [!CAUTION]
> Open mode (no token configured) is suitable only for trusted local use. Any process on the same host can issue agent commands without authentication.

### Agent discovery

Zeph publishes a machine-readable agent manifest at `GET /.well-known/acp.json` that ACP-compatible clients use for capability discovery. The manifest includes the agent name, version, supported transports, and authentication type.

```toml
# config.toml
[acp]
discovery_enabled = true   # default: true
```

| Method | Value |
|---|---|
| Config key | `acp.discovery_enabled` |
| Environment variable | `ZEPH_ACP_DISCOVERY_ENABLED=false` to disable |

> [!NOTE]
> The discovery endpoint is intentionally unauthenticated so clients can discover the agent and its auth requirements before presenting credentials. Do not include sensitive data in the manifest. Set `discovery_enabled = false` if the endpoint must not be publicly reachable.

[ACP setup guide →](https://bug-ops.github.io/zeph/advanced/acp.html)

## Sub-Agents

Zeph supports spawning sub-agents — isolated agent instances with their own LLM provider, filtered tool access, and injected skills. Sub-agents are defined as Markdown files with TOML frontmatter and loaded from `.zeph/agents/` (project scope) or `~/.config/zeph/agents/` (user scope).

### Definition format

```markdown
+++
name = "code-reviewer"
description = "Reviews code changes for correctness and style"
model = "claude-sonnet-4-20250514"

[tools]
allow = ["shell", "web_scrape"]

[permissions]
network = true
filesystem = "read"
secrets = ["GITHUB_TOKEN"]
ttl_secs = 120

[skills]
include = ["git-*", "rust-*"]
exclude = ["deploy-*"]
+++

You are a code reviewer. Report findings with severity.
```

### CLI commands

| Command | Description |
|---------|-------------|
| `/agent list` | List available sub-agent definitions |
| `/agent spawn <name> <prompt>` | Spawn a foreground sub-agent |
| `/agent bg <name> <prompt>` | Spawn a background sub-agent |
| `/agent status` | Show active sub-agents with state, turns, and elapsed time |
| `/agent cancel <id>` | Cancel a running sub-agent by ID prefix |
| `/agent approve <id>` | Approve a pending secret request |
| `/agent deny <id>` | Deny a pending secret request |

### Configuration

```toml
[agents]
enabled = true
max_concurrent = 4
extra_dirs = ["/path/to/shared/agents"]
```

> [!NOTE]
> Sub-agents are disabled by default. Set `agents.enabled = true` to activate. Each sub-agent receives only explicitly granted tools, skills, and secrets via zero-trust `PermissionGrants`.

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

The workspace enforces `unsafe_code = "deny"` at the Cargo workspace lint level — no unsafe Rust is permitted in any crate without an explicit override.

Found a vulnerability? Please use [GitHub Security Advisories](https://github.com/bug-ops/zeph/security/advisories/new) for responsible disclosure.

## License

[MIT](LICENSE)
