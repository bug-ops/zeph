# zeph-core

Agent loop, configuration loading, and context builder.

## Overview

Core orchestration crate for the Zeph agent. Manages the main agent loop, bootstraps the application from TOML configuration with environment variable overrides, and assembles the LLM context from conversation history, skills, and memory. All other workspace crates are coordinated through `zeph-core`.

## Key modules

| Module | Description |
|--------|-------------|
| `agent` | `Agent<C>` — main loop driving inference and tool execution; ToolExecutor erased via `Box<dyn ErasedToolExecutor>` |
| `agent::tool_execution` | Tool call handling, redaction, and result processing |
| `agent::message_queue` | Message queue management |
| `agent::builder` | Agent builder API |
| `agent::commands` | Chat command dispatch (skills, feedback, etc.) |
| `agent::utils` | Shared agent utilities |
| `bootstrap` | `AppBuilder` — fluent builder for application startup |
| `channel` | `Channel` trait defining I/O adapters; `LoopbackChannel` / `LoopbackHandle` for headless daemon I/O; `Attachment` / `AttachmentKind` for multimodal inputs |
| `config` | TOML config with `ZEPH_*` env overrides; typed `ConfigError` (Io, Parse, Validation, Vault) |
| `context` | LLM context assembly from history, skills, memory |
| `cost` | Token cost tracking and budgeting |
| `daemon` | Background daemon mode with PID file lifecycle (optional feature) |
| `metrics` | Runtime metrics collection |
| `project` | Project-level context detection |
| `redact` | Regex-based secret redaction (AWS, OpenAI, Anthropic, Google, GitLab, HuggingFace, npm, Docker) |
| `vault` | Secret storage and resolution via vault providers (age-encrypted read/write) |
| `diff` | Diff rendering utilities |
| `pipeline` | Composable, type-safe step chains for multi-stage workflows |

**Re-exports:** `Agent`

## Configuration

Key `AgentConfig` fields (TOML section `[agent]`):

| Field | Type | Default | Env override | Description |
|-------|------|---------|--------------|-------------|
| `name` | string | `"zeph"` | — | Agent display name |
| `max_tool_iterations` | usize | `10` | — | Max tool calls per turn |
| `summary_model` | string? | `null` | — | Model used for context summarization |
| `auto_update_check` | bool | `true` | `ZEPH_AUTO_UPDATE_CHECK` | Check GitHub releases for a newer version on startup / via scheduler |

```toml
[agent]
auto_update_check = true   # set to false to disable update notifications
```

Set `ZEPH_AUTO_UPDATE_CHECK=false` to disable without changing the config file.

## Usage

```toml
[dependencies]
zeph-core = { path = "../zeph-core" }
```

## License

MIT
