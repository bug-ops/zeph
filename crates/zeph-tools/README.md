# zeph-tools

Tool execution abstraction and shell backend.

## Overview

Defines the `ToolExecutor` trait for sandboxed tool invocation and ships concrete executors for shell commands, file operations, and web scraping. The `CompositeExecutor` chains multiple backends with output filtering, permission checks, trust gating, anomaly detection, and audit logging.

## Key modules

| Module | Description |
|--------|-------------|
| `executor` | `ToolExecutor` trait, `ToolOutput`, `ToolCall` |
| `shell` | Shell command executor |
| `file` | File operation executor |
| `scrape` | Web scraping executor |
| `composite` | `CompositeExecutor` — chains executors with middleware |
| `filter` | Output filtering pipeline |
| `permissions` | Permission checks for tool invocation |
| `audit` | `AuditLogger` — tool execution audit trail |
| `registry` | Tool registry and discovery |
| `trust_gate` | Trust-based tool access control |
| `anomaly` | `AnomalyDetector` — unusual execution pattern detection |
| `overflow` | Output overflow handling |
| `config` | Per-tool TOML configuration |

**Re-exports:** `CompositeExecutor`, `AuditLogger`, `AnomalyDetector`

## Usage

```toml
[dependencies]
zeph-tools = { path = "../zeph-tools" }
```

## License

MIT
