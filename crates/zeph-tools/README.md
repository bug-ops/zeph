# zeph-tools

[![Crates.io](https://img.shields.io/crates/v/zeph-tools)](https://crates.io/crates/zeph-tools)
[![docs.rs](https://img.shields.io/docsrs/zeph-tools)](https://docs.rs/zeph-tools)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)

Tool executor trait with shell, web scrape, and composite executors for Zeph.

## Overview

Defines the `ToolExecutor` trait for sandboxed tool invocation and ships concrete executors for shell commands, file operations, and web scraping. The `CompositeExecutor` chains multiple backends with output filtering, permission checks, trust gating, anomaly detection, and audit logging.

## Key modules

| Module | Description |
|--------|-------------|
| `executor` | `ToolExecutor` trait, `ToolOutput`, `ToolCall` |
| `shell` | Shell command executor with tokenizer-based command detection, escape normalization, and transparent wrapper skipping; receives skill-scoped env vars injected by the agent for active skills that declare `x-requires-secrets` |
| `file` | File operation executor |
| `scrape` | Web scraping executor with SSRF protection (post-DNS private IP validation, pinned address client) |
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

## Installation

```bash
cargo add zeph-tools
```

## License

MIT
