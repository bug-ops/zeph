# Zeph Specs

Feature principle documents — key invariants for coding agents.
See `constitution.md` for project-wide non-negotiable rules.

## System Invariants

| Doc | Contents |
|---|---|
| `001-system-invariants/spec.md` | Cross-cutting architectural invariants |

## Feature Docs

| Doc | Feature | Crate |
|---|---|---|
| `002-agent-loop/spec.md` | Agent loop, turn lifecycle, context pressure, HiAgent subgoal-aware compaction | `zeph-core` |
| `003-llm-providers/spec.md` | LlmProvider trait, AnyProvider, prompt caching | `zeph-llm` |
| `004-memory/spec.md` | SQLite + Qdrant, compaction, semantic response cache, anchored summarization, compaction probe, importance scoring | `zeph-memory` |
| `005-skills/spec.md` | SKILL.md format, registry, matching, hot-reload | `zeph-skills` |
| `006-tools/spec.md` | ToolExecutor, CompositeExecutor, TAFC, schema filter, result cache, dependency graph | `zeph-tools` |
| `007-channels/spec.md` | Channel trait, AnyChannel dispatch, streaming, channel feature parity | `zeph-channels` |
| `008-mcp/spec.md` | MCP client, server lifecycle, tool discovery | `zeph-mcp` |
| `009-orchestration/spec.md` | DAG planner, DagScheduler, AgentRouter, /plan, plan template cache | `zeph-orchestration` |
| `010-security/spec.md` | Vault, shell sandbox, content isolation, SSRF | cross-cutting |
| `011-tui/spec.md` | ratatui dashboard, spinner rule, TuiChannel | `zeph-tui` |
| `012-graph-memory/spec.md` | Entity graph, BFS recall, community detection, MAGMA typed edges, SYNAPSE spreading activation | `zeph-memory` |
| `013-acp/spec.md` | ACP transports, sessions, permissions, fork/resume | `zeph-acp` |
| `014-a2a/spec.md` | A2A protocol, agent discovery, JSON-RPC 2.0 | `zeph-a2a` |
| `015-self-learning/spec.md` | FeedbackDetector (multi-language), Wilson score, trust model | `zeph-skills` |
| `016-output-filtering/spec.md` | FilterPipeline, CommandMatcher, SecurityPatterns | `zeph-tools` |
| `017-index/spec.md` | AST indexing, semantic retrieval, repo map | `zeph-index` |
| `018-scheduler/spec.md` | Cron scheduler, SQLite persistence (⚠️ PERF-SC-04 bug) | `zeph-scheduler` |
| `019-gateway/spec.md` | HTTP webhook ingestion, bearer auth | `zeph-gateway` |
| `020-config-loading/spec.md` | Config resolution order, mode-agnostic defaults | `zeph-core` |
| `021-architecture-audit/spec.md` | Post-PR#1972 comprehensive audit: type safety, DRY, dead code, abstractions, channels | cross-cutting |
| `handoff-skill-system/spec.md` | Skill-based YAML handoff protocol for inter-agent communication | `zeph-orchestration` |
