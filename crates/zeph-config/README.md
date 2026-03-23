# zeph-config

[![Crates.io](https://img.shields.io/crates/v/zeph-config)](https://crates.io/crates/zeph-config)
[![docs.rs](https://img.shields.io/docsrs/zeph-config)](https://docs.rs/zeph-config)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)

Pure-data configuration types for Zeph — all TOML config structs with serde derive, validation, and migration support.

## Overview

Central repository for every typed configuration struct used across the Zeph workspace. Loading, parsing, and TOML migration live here; downstream crates pull the types they need without taking a dependency on the full agent stack. Configuration is sourced from a TOML file and can be overridden by `ZEPH_*` environment variables.

> [!WARNING]
> **v0.17.0 breaking change**: `[llm.cloud]`, `[llm.openai]`, `[llm.orchestrator]`, and `[llm.router]` are replaced by `[[llm.providers]]`. Run `zeph migrate-config --in-place` to upgrade automatically.

The minimal new format:

```toml
[llm]

[[llm.providers]]
type = "ollama"
model = "qwen3:8b"
embedding_model = "qwen3-embedding"
```

## Key types

| Type | TOML section | Description |
|------|-------------|-------------|
| `Config` | root | Top-level aggregate; resolved by `Config::load()` |
| `AgentConfig` | `[agent]` | Agent loop settings, instruction files, self-learning |
| `LlmConfig` | `[llm]` + `[[llm.providers]]` | Provider pool, routing strategy, summary provider |
| `MemoryConfig` | `[memory]` | SQLite pool, vector backend, compaction, graph memory |
| `SkillsConfig` | `[skills]` | Prompt mode, trust thresholds, hybrid search weights |
| `ToolsConfig` | `[tools]` | Shell executor, web scrape, audit logging, anomaly detection |
| `McpConfig` | `[mcp]` | MCP server list and transport settings |
| `OrchestrationConfig` | `[orchestration]` | DAG planner, aggregator, confirmation flow |
| `ComplexityRoutingConfig` | `[llm.complexity_routing]` | Per-tier provider pools for complexity triage routing |
| `SecurityConfig` | `[security]` | Content isolation, exfiltration guard, quarantine |
| `DebugConfig` | `[debug]` | Debug dump path and format |
| `LoggingConfig` | `[logging]` | Log file path, level, rotation, retention |
| `VaultConfig` | `[vault]` | Vault backend (`env`, `age`) and key path |

## Usage

```rust
use zeph_config::Config;

// Load config from the default path (~/.config/zeph/config.toml)
let config = Config::load(None)?;

println!("Provider: {}", config.llm.effective_provider());
println!("Max tool iterations: {}", config.agent.max_tool_iterations);
```

Custom path and environment overrides:

```rust
use std::path::Path;
use zeph_config::Config;

let config = Config::load(Some(Path::new("/etc/zeph/config.toml")))?;
// ZEPH_LLM_PROVIDER, ZEPH_LOG_LEVEL, etc. are applied automatically
```

Config migration (add missing sections from the canonical default):

```bash
zeph migrate-config            # print diff
zeph migrate-config --in-place # update file in place
```

## Features

| Feature | Description |
|---------|-------------|
| `guardrail` | Enables `GuardrailConfig` under `[security.guardrail]` |
| `lsp-context` | Enables `LspConfig` under `[agent.lsp]` for LSP context injection hooks |
| `compression-guidelines` | Enables `CompressionGuidelinesConfig` under `[memory.compression]` |
| `experiments` | Enables `ExperimentConfig` and `ExperimentSchedule` under `[experiments]` |
| `policy-enforcer` | Enables `PolicyEnforcerConfig` under `[tools.policy]` |

## Environment variable overrides

All configuration fields can be overridden with `ZEPH_`-prefixed environment variables. Key examples:

| Variable | Config field |
|----------|-------------|
| `ZEPH_LLM_PROVIDER` | `llm.providers[0].type` (overrides first entry type) |
| `ZEPH_LOG_LEVEL` | `logging.level` |
| `ZEPH_LOG_FILE` | `logging.file` |
| `ZEPH_AUTO_UPDATE_CHECK` | `agent.auto_update_check` |
| `ZEPH_ACP_MAX_SESSIONS` | `acp.max_sessions` |

## Installation

```bash
cargo add zeph-config
```

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

MIT
