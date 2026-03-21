# Crates Overview

Zeph is a Cargo workspace (Edition 2024, resolver 3) composed of 21 crates plus the root binary. Each crate has a focused responsibility; all leaf crates are independently testable in isolation.

## Full Workspace Layout

```text
zeph (binary)
├── Layer 0 — Primitives (no workspace deps)
│   └── zeph-common         Shared primitives: Secret, VaultError, common types
│
├── Layer 1 — Configuration & Secrets
│   ├── zeph-config         Pure-data configuration types, TOML loader, env overrides, migration
│   └── zeph-vault          VaultProvider trait + env and age-encrypted backends
│
├── Layer 2 — Core Domain Crates
│   ├── zeph-llm            LlmProvider trait, Ollama/Claude/OpenAI/Candle backends, orchestrator
│   ├── zeph-memory         SQLite + Qdrant, SemanticMemory, summarization, document loaders
│   ├── zeph-tools          ToolExecutor trait, ShellExecutor, FileExecutor, TrustLevel
│   └── zeph-skills         SKILL.md parser, registry, embedding matcher, hot-reload
│
├── Layer 3 — Agent Subsystems
│   ├── zeph-sanitizer      Content sanitization pipeline, PII filter, exfiltration guard
│   ├── zeph-experiments    Autonomous experiment engine, hyperparameter tuning, LLM-as-judge
│   ├── zeph-subagent       Subagent lifecycle, grants, transcripts, lifecycle hooks
│   └── zeph-orchestration  DAG-based task orchestration, planner, router, aggregator
│
├── Layer 4 — Agent Core
│   └── zeph-core           Agent loop, AppBuilder bootstrap, context builder, metrics
│
└── Layer 5 — I/O & Optional Extensions
    ├── zeph-channels       Telegram + CLI + Discord + Slack channel adapters
    ├── zeph-index          AST-based code indexing, semantic retrieval, repo map (always-on)
    ├── zeph-mcp            MCP client via rmcp, multi-server lifecycle (optional)
    ├── zeph-a2a            A2A protocol client + server, agent discovery (optional)
    ├── zeph-acp            Agent Client Protocol server — IDE integration (optional)
    ├── zeph-tui            ratatui TUI dashboard with real-time metrics (optional)
    ├── zeph-gateway        HTTP gateway for webhook ingestion (optional)
    └── zeph-scheduler      Cron-based periodic task scheduler (optional)
```

## Dependency Graph

```text
zeph (binary)
  ├── zeph-core (orchestrates everything)
  │     ├── zeph-config (Layer 1)
  │     ├── zeph-vault  (Layer 1)
  │     ├── zeph-llm    (leaf)
  │     ├── zeph-skills (leaf)
  │     ├── zeph-memory (leaf)
  │     ├── zeph-channels (leaf)
  │     ├── zeph-tools  (leaf)
  │     ├── zeph-sanitizer (leaf)
  │     ├── zeph-experiments (optional, leaf)
  │     ├── zeph-subagent (leaf)
  │     ├── zeph-orchestration (leaf)
  │     ├── zeph-index  (leaf, always-on)
  │     ├── zeph-mcp    (optional, leaf)
  │     └── zeph-tui    (optional, leaf)
  └── zeph-a2a  (optional, wired by binary, not by zeph-core)
```

`zeph-core` is the only crate that depends on other workspace crates. All leaf crates are independent and can be tested in isolation. `zeph-a2a` is feature-gated and wired directly by the binary.

## Crate Responsibilities

| Crate | Layer | Description |
|-------|-------|-------------|
| `zeph-common` | 0 | `Secret`, `VaultError`, and shared primitive types |
| `zeph-config` | 1 | All configuration structs, TOML loader, env overrides, migration |
| `zeph-vault` | 1 | `VaultProvider` trait + `EnvVaultProvider` and `AgeVaultProvider` backends |
| `zeph-llm` | 2 | `LlmProvider` trait, all LLM backends, model orchestrator, embeddings |
| `zeph-memory` | 2 | SQLite persistence, Qdrant vector search, document loaders, token counter, semantic response cache, anchored summarization, MAGMA typed edges, SYNAPSE spreading activation, write-time importance scoring |
| `zeph-tools` | 2 | Tool execution framework, shell sandbox, file executor, trust model, TAFC schema augmentation, tool result cache, tool dependency graph, tool schema filtering |
| `zeph-skills` | 2 | SKILL.md parser, skill registry, embedding matcher, hot-reload |
| `zeph-sanitizer` | 3 | Content sanitization, injection detection, PII filtering, exfiltration guard |
| `zeph-experiments` | 3 | Autonomous experiment engine, hyperparameter search, LLM-as-judge evaluation |
| `zeph-subagent` | 3 | Subagent spawning, capability grants, transcripts, lifecycle hooks |
| `zeph-orchestration` | 3 | DAG task graph, DagScheduler, AgentRouter, LlmPlanner, LlmAggregator, plan template caching |
| `zeph-core` | 4 | Agent loop, `AppBuilder`, context engineering, metrics, channel trait, multi-language FeedbackDetector, subgoal-aware compaction |
| `zeph-channels` | 5 | Telegram, CLI, Discord, Slack channel adapters |
| `zeph-index` | 5 | AST-based code indexing, hybrid retrieval, repo map generation |
| `zeph-mcp` | 5 | MCP client for external tool servers (optional) |
| `zeph-a2a` | 5 | A2A protocol client and server (optional) |
| `zeph-acp` | 5 | ACP server for IDE integration (optional) |
| `zeph-tui` | 5 | ratatui TUI dashboard (optional) |
| `zeph-gateway` | 5 | HTTP gateway for webhook ingestion (optional) |
| `zeph-scheduler` | 5 | Cron-based periodic task scheduler (optional) |

## Design Principles

- **Single responsibility**: each crate owns one domain; cross-cutting concerns are split into dedicated crates rather than accumulated in `zeph-core`
- **Always testable in isolation**: leaf crates carry no workspace peer dependencies; unit tests run without a running agent
- **Feature-gated extensions**: optional crates are compiled only when the corresponding feature flag is active — see [Feature Flags](../reference/feature-flags.md)
- **No `async-trait`**: native async trait methods (Edition 2024) throughout; `Pin<Box<dyn Future>>` for object-safe dynamic dispatch
- **TLS**: rustls everywhere — no openssl-sys dependency
- **Error handling**: `thiserror` for typed error enums in every crate; `anyhow` only in the top-level `runner.rs`
