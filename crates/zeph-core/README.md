# zeph-core

[![Crates.io](https://img.shields.io/crates/v/zeph-core)](https://crates.io/crates/zeph-core)
[![docs.rs](https://img.shields.io/docsrs/zeph-core)](https://docs.rs/zeph-core)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)

Core agent loop, configuration, context builder, metrics, vault, and sub-agent orchestration for Zeph.

## Overview

Core orchestration crate for the Zeph agent. Manages the main agent loop, bootstraps the application from TOML configuration with environment variable overrides, and assembles the LLM context from conversation history, skills, and memory. Includes sub-agent orchestration with zero-trust permission grants, background execution, filtered tool/skill access, A2A-based in-process communication channels, and `/agent` CLI commands for runtime management. All other workspace crates are coordinated through `zeph-core`.

## Key modules

| Module | Description |
|--------|-------------|
| `agent` | `Agent<C>` — main loop driving inference and tool execution; ToolExecutor erased via `Box<dyn ErasedToolExecutor>`; supports external cancellation via `with_cancel_signal()` |
| `agent::tool_execution` | Tool call handling, redaction, and result processing |
| `agent::message_queue` | Message queue management |
| `agent::builder` | Agent builder API |
| `agent::commands` | Chat command dispatch (skills, feedback, skill management via `/skill install` and `/skill remove`, sub-agent management via `/agent`, etc.) |
| `agent::utils` | Shared agent utilities |
| `bootstrap` | `AppBuilder` — fluent builder for application startup |
| `channel` | `Channel` trait defining I/O adapters; `LoopbackChannel` / `LoopbackHandle` for headless daemon I/O (`LoopbackHandle` exposes `cancel_signal: Arc<Notify>` for session cancellation); `Attachment` / `AttachmentKind` for multimodal inputs |
| `config` | TOML config with `ZEPH_*` env overrides; typed `ConfigError` (Io, Parse, Validation, Vault) |
| `context` | LLM context assembly from history, skills, memory; adaptive chunked compaction with parallel summarization; uses shared `Arc<TokenCounter>` for accurate tiktoken-based budget tracking |
| `cost` | Token cost tracking and budgeting |
| `daemon` | Background daemon mode with PID file lifecycle (optional feature) |
| `metrics` | Runtime metrics collection |
| `project` | Project-level context detection |
| `redact` | Regex-based secret redaction (AWS, OpenAI, Anthropic, Google, GitLab, HuggingFace, npm, Docker) |
| `vault` | Secret storage and resolution via vault providers (age-encrypted read/write); scans `ZEPH_SECRET_*` keys to build the custom-secrets map used by skill env injection |
| `diff` | Diff rendering utilities |
| `pipeline` | Composable, type-safe step chains for multi-stage workflows |
| `subagent` | Sub-agent orchestration: `SubAgentManager` lifecycle with background execution, `SubAgentDef` TOML definitions, `PermissionGrants` zero-trust delegation, `FilteredToolExecutor` scoped tool access, A2A in-process channels, real-time status tracking |

**Re-exports:** `Agent`

## Configuration

Key `AgentConfig` fields (TOML section `[agent]`):

| Field | Type | Default | Env override | Description |
|-------|------|---------|--------------|-------------|
| `name` | string | `"zeph"` | — | Agent display name |
| `max_tool_iterations` | usize | `10` | — | Max tool calls per turn |
| `summary_model` | string? | `null` | — | Model used for context summarization |
| `auto_update_check` | bool | `true` | `ZEPH_AUTO_UPDATE_CHECK` | Check GitHub releases for a newer version on startup / via scheduler |

Key `MemoryConfig` fields (TOML section `[memory]`):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `vector_backend` | `"qdrant"` / `"sqlite"` | `"qdrant"` | Vector search backend |
| `token_safety_margin` | f32 | `1.0` | Safety multiplier for tiktoken-based token budget (validated: must be >= 1.0) |
| `redact_credentials` | bool | `true` | Scrub secrets and paths before LLM context injection |
| `autosave_assistant` | bool | `false` | Persist assistant responses to semantic memory automatically |
| `autosave_min_length` | usize | `20` | Minimum response length (chars) to trigger autosave |

```toml
[agent]
auto_update_check = true   # set to false to disable update notifications
```

Set `ZEPH_AUTO_UPDATE_CHECK=false` to disable without changing the config file.

## Sub-agent Commands

In-session commands for managing sub-agents:

| Command | Description |
|---------|-------------|
| `/agent list` | List available sub-agent definitions |
| `/agent spawn <name> <prompt>` | Spawn a sub-agent with a task prompt |
| `/agent bg <name> <prompt>` | Spawn a background sub-agent |
| `/agent status` | Show active sub-agents with state, turns, and elapsed time |
| `/agent cancel <id>` | Cancel a running sub-agent by ID prefix |
| `@agent_name <prompt>` | Mention shorthand for `/agent spawn` (disambiguated from file references) |

Sub-agents run as independent tokio tasks with their own LLM provider and filtered tool executor. Each sub-agent receives only explicitly granted tools, skills, and secrets via `PermissionGrants`.

## Installation

```bash
cargo add zeph-core
```

## License

MIT
