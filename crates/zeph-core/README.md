# zeph-core

Agent loop, configuration loading, and context builder.

## Overview

Core orchestration crate for the Zeph agent. Manages the main agent loop, bootstraps the application from TOML configuration with environment variable overrides, and assembles the LLM context from conversation history, skills, and memory. All other workspace crates are coordinated through `zeph-core`.

## Key modules

| Module | Description |
|--------|-------------|
| `agent` | `Agent` — main loop driving inference and tool execution |
| `bootstrap` | `AppBuilder` — fluent builder for application startup |
| `channel` | `Channel` trait defining I/O adapters; `Attachment` / `AttachmentKind` for multimodal inputs (images, audio) |
| `config` | TOML config with `ZEPH_*` env overrides |
| `context` | LLM context assembly from history, skills, memory |
| `cost` | Token cost tracking and budgeting |
| `daemon` | Background daemon mode (optional feature) |
| `metrics` | Runtime metrics collection |
| `project` | Project-level context detection |
| `redact` | Sensitive data redaction |
| `vault` | Secret resolution via vault providers |
| `diff` | Diff rendering utilities |
| `pipeline` | Composable, type-safe step chains for multi-stage workflows |

**Re-exports:** `Agent`

## Usage

```toml
[dependencies]
zeph-core = { path = "../zeph-core" }
```

## License

MIT
