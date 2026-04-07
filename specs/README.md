# Zeph Specs

Feature principle documents — key invariants for coding agents.
See `constitution.md` for project-wide non-negotiable rules.

## Foundation

| Doc | Contents |
|---|---|
| `001-system-invariants/spec.md` | Cross-cutting architectural invariants |
| `constitution.md` | Project-wide non-negotiable rules |

## Core Agent

| Doc | Contents | Crate |
|---|---|---|
| `002-agent-loop/spec.md` | Agent loop, turn lifecycle, context pressure, HiAgent subgoal-aware compaction, MagicDocs auto-maintained markdown | `zeph-core` |
| `027-runtime-layer/spec.md` | RuntimeLayer middleware — before/after chat and tool hooks, catch_unwind guard | `zeph-core` |
| `028-hooks/spec.md` | Reactive hooks: cwd_changed / file_changed events, FileChangeWatcher, ZEPH_* env vars | `zeph-core` |
| `020-config-loading/spec.md` | Config resolution order, mode-agnostic defaults | `zeph-core` |

## LLM & Inference

| Doc | Contents | Crate |
|---|---|---|
| `003-llm-providers/spec.md` | LlmProvider trait, AnyProvider, prompt caching, ASI coherence tracking, quality gate, per-provider cost breakdown | `zeph-llm` |
| `022-config-simplification/spec.md` | Provider Registry Architecture: [[llm.providers]], PILOT LinUCB bandit routing, BaRP, MAR | `zeph-config`, `zeph-core` |
| `023-complexity-triage-routing/spec.md` | Pre-inference complexity classification: ComplexityTier, TriageRouter, context escalation | `zeph-llm`, `zeph-config` |
| `024-multi-model-design/spec.md` | Multi-model design principle: complexity tiers, *_provider subsystem reference pattern (19 subsystems) | cross-cutting |
| `025-classifiers/spec.md` | Candle ML classifiers: injection detection, PII detection, LlmClassifier for feedback | `zeph-classifiers` |

## Memory

| Doc | Contents | Crate |
|---|---|---|
| `004-memory/spec.md` | SQLite + Qdrant, compaction, A-MAC, MemScene, Kumiho, D-MEM, CraniMem, ACON, AsyncMemoryRouter, multi-vector chunking, GAAMA, BATS, Focus compression, SleepGate, persona memory, trajectory memory, category memory, TiMem memory tree, key facts dedup, microcompact, autoDream, multi-agent consistency | `zeph-memory` |
| `012-graph-memory/spec.md` | Entity graph, BFS recall, community detection, MAGMA typed edges, SYNAPSE spreading activation | `zeph-memory` |
| `database-abstraction/spec.md` | PostgreSQL backend: zeph-db crate, DatabaseDriver trait, Dialect trait, sql!() macro, migrations | `zeph-db` |

## Skills & Self-Learning

| Doc | Contents | Crate |
|---|---|---|
| `005-skills/spec.md` | SKILL.md format, registry, matching, hot-reload, skill trust governance, SkillOrchestra LinUCB, D2Skill, NL generation, GitHub mining, channel allowlist | `zeph-skills` |
| `015-self-learning/spec.md` | FeedbackDetector, Wilson score, SAGE RL, ARISE, STEM, ERL, learning.rs submodules | `zeph-skills` |

## Tools & Execution

| Doc | Contents | Crate |
|---|---|---|
| `006-tools/spec.md` | ToolExecutor, CompositeExecutor, TAFC, utility gate, adversarial policy gate, shell sandbox, file read sandbox, tool invocation phase taxonomy, per-session quota, OAP authorization | `zeph-tools` |
| `016-output-filtering/spec.md` | FilterPipeline, CommandMatcher, SecurityPatterns | `zeph-tools` |

## Channels & I/O

| Doc | Contents | Crate |
|---|---|---|
| `007-channels/spec.md` | Channel trait, AnyChannel dispatch, streaming, channel feature parity | `zeph-channels` |
| `011-tui/spec.md` | ratatui dashboard, spinner rule, TuiChannel | `zeph-tui` |
| `026-tui-subagent-management/spec.md` | Interactive TUI subagent sidebar, j/k navigation, JSONL transcript viewer | `zeph-tui` |
| `019-gateway/spec.md` | HTTP webhook ingestion, bearer auth | `zeph-gateway` |
| `018-scheduler/spec.md` | Cron scheduler, SQLite persistence, CLI subcommand, MissedTickBehavior::Skip | `zeph-scheduler` |

## Protocols

| Doc | Contents | Crate |
|---|---|---|
| `008-mcp/spec.md` | MCP client, server lifecycle, semantic tool discovery, elicitation, tool collision detection, intent-anchor nonce, MCP error codes, caller identity propagation | `zeph-mcp` |
| `013-acp/spec.md` | ACP transports, sessions, permissions, fork/resume, capability advertisement, /agent.json | `zeph-acp` |
| `014-a2a/spec.md` | A2A protocol, agent discovery, JSON-RPC 2.0, IBCT tokens | `zeph-a2a` |
| `handoff-skill-system/spec.md` | Skill-based YAML handoff protocol for inter-agent communication | `zeph-orchestration` |

## Orchestration & Sub-Agents

| Doc | Contents | Crate |
|---|---|---|
| `009-orchestration/spec.md` | DAG planner, DagScheduler, AgentRouter, /plan, VMAO adaptive replanning, cascade routing | `zeph-orchestration` |
| `subagent-context-propagation/report.md` | /agent spawn context gap analysis — 12 gaps, phase-based fix plan | `zeph-subagent` |

## Security

| Doc | Contents | Crate |
|---|---|---|
| `010-security/spec.md` | Vault, shell sandbox, content isolation, SSRF, IPI defense, PII NER, cross-tool injection correlation, AgentRFC audit, IBCT, credential scrubbing, OAP declarative authorization | cross-cutting |

## Code Intelligence

| Doc | Contents | Crate |
|---|---|---|
| `017-index/spec.md` | AST indexing, semantic retrieval, repo map | `zeph-index` |

## Infrastructure & Build

| Doc | Contents | Crate |
|---|---|---|
| `029-feature-flags/spec.md` | Feature flag decision rules, surviving flag inventory (22 flags), bundle definitions | `Cargo.toml` |
