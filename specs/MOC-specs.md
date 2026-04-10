---
aliases:
  - Specifications Index
  - Zeph Specs Overview
tags:
  - moc
  - sdd
  - specifications
created: 2026-04-10
status: moc
---

# Zeph Specifications

> [!abstract]
> Map of Content for all Zeph project specifications. Each entry links to
> a feature spec with its current phase and status. Read [[constitution]] for
> project-wide non-negotiable rules that apply to every change.

---

## Foundation & Architecture

### System Invariants
- [[001-system-invariants/spec|System Invariants]] — cross-cutting architectural contracts and constraints that all components must follow; includes channel, agent loop, LLM provider, memory, skill, configuration, feature flag, concurrency, error handling, database, and runtime layer contracts

### Constitution & Principles
- [[constitution]] — project principles, technology stack, testing standards, code style, security, performance, simplicity, and git workflow; non-negotiable and applies to all development

---

## Core Agent Systems

### Agent Loop & Lifecycle
- [[002-agent-loop/spec|Agent Loop]] — agent main loop, turn lifecycle, context pressure management, HiAgent subgoal-aware compaction; single-threaded async with message queue draining and provider hot-swap

### LLM Providers & Routing
- [[003-llm-providers/spec|LLM Providers]] — LlmProvider trait, AnyProvider enum, prompt caching, debug request serialization, multi-provider pooling, chat vs chat_stream vs chat_with_tools codepaths
- [[022-config-simplification/spec|Provider Registry Architecture]] — canonical `[[llm.providers]]` format, ProviderEntry schema, routing strategies, BaRP cost-weight dial, MAR memory-augmented routing; replaces inline provider configs
- [[023-complexity-triage-routing/spec|Complexity Triage Routing]] — pre-inference complexity classification routing via ComplexityTier and TriageRouter; context escalation for complex queries
- [[024-multi-model-design/spec|Multi-Model Design Principle]] — complexity tiers (simple/medium/complex/expert), `*_provider` subsystem reference pattern, STT unification; applies to all LLM-calling subsystems

### Memory Systems
- [[004-memory/spec|Memory Pipeline]] — SQLite + Qdrant dual backend, semantic response cache, anchored summarization, compaction probe, importance scoring, A-MAC admission control, MemScene consolidation, cost-sensitive store routing, temporal decay, multi-vector chunking, GAAMA episode nodes, BATS budget hints, Focus compression, SleepGate forgetting pass, persona/trajectory/category-aware memory, TiMem tree, microcompact, autoDream, MagicDocs, embed backfill batching
- [[012-graph-memory/spec|Entity Graph Memory]] — entity graph, BFS recall, community detection, MAGMA typed edges, SYNAPSE spreading activation; works with [[004-memory/spec|Memory Pipeline]]
  - [[004-memory/004-6-graph-memory|Graph Memory (memory sub-spec)]] — concise reference within the memory subsystem: data model overview, MAGMA edge types, SYNAPSE config, key invariants

### Configuration & Loading
- [[020-config-loading/spec|Config Loading]] — config resolution order, mode-agnostic defaults, environment overrides
- [[022-config-simplification/spec|Provider Registry]] — see LLM Providers section above

---

## Execution & Tools

### Skills System
- [[005-skills/spec|Skills System]] — SKILL.md format specification, registry, hot-reload with notify crate and 500ms debounce, matching algorithm (BM25 + embedding hybrid, pure embedding, keyword fallback), skill injection into system prompt, trust governance via Wilson score, self-learning feedback integration, disambiguation threshold and min injection score gates, max_active_skills hard cap
- [[015-self-learning/spec|Self-Learning & Feedback]] — FeedbackDetector (multi-language), Wilson score confidence intervals, trust model (Untrusted → Provisional → Trusted), SAGE RL cross-session reward, ARISE trace improvement, STEM pattern-to-skill migration, ERL experiential learning, skill ranking by confidence

### Tool Execution
- [[006-tools/spec|Tool Execution]] — ToolExecutor trait, CompositeExecutor, TAFC, schema filter, result cache, dependency graph, compress_context, transactional ShellExecutor, utility-guided dispatch gate, adversarial policy gate, structured shell output envelope, per-path file read sandbox, claim_source audit, tool invocation phase taxonomy (Planner/Executor/Verifier/Autonomous), native `tool_use` path only
- [[016-output-filtering/spec|Output Filtering]] — FilterPipeline, CommandMatcher, SecurityPatterns; prevents sensitive data leaks in tool output

### MCP Integration
- [[008-mcp/spec|MCP Client & Server]] — MCP client via rmcp, multi-server lifecycle, semantic tool discovery, per-message pruning cache, Roots injection detection feedback, elicitation (Phase 1+2, bounded channel), tool collision detection, server instructions injection, caller identity propagation (`caller_id`), tool quota (`max_tool_calls_per_session`), structured error codes (`McpErrorCode`), OAP authorization (`[tools.authorization]`); per-server stdio env isolation

---

## Orchestration & Routing

### Planning & DAG
- [[009-orchestration/spec|Orchestration & Planning]] — DAG planner, DagScheduler, AgentRouter, /plan command, plan template cache, VMAO adaptive replanning, cascade-aware DAG routing with CascadeDetector, tree-optimized dispatch; defines strategy for multi-step task execution

---

## Security & Validation

### Security Framework
- [[010-security/spec|Security & Content Isolation]] — Vault secret management, shell sandbox, content isolation, SSRF protection, IPI defense (DeBERTa soft-signal, AlignSentinel 3-class, TurnCausalAnalyzer), PII NER circuit breaker + allowlist, cross-tool injection correlation, AgentRFC protocol audit, MCP→ACP confused-deputy boundary enforcement, SMCP lifecycle + IBCT tokens, credential env-var scrubbing, MCP tool input schema injection scan

### ML Classifiers
- [[025-classifiers/spec|Candle-backed ML Classifiers]] — injection detection (CandleClassifier), PII detection (CandlePiiClassifier), LlmClassifier for feedback, unified regex+NER sanitization pipeline; provides signals for [[010-security/spec|Security Framework]]

---

## User Interface & Channels

### Channel System
- [[007-channels/spec|Channel System]] — Channel trait, AnyChannel dispatch, streaming support, feature parity across channels (CLI, Telegram, TUI); single I/O boundary for all I/O modes

### TUI Dashboard
- [[011-tui/spec|TUI Dashboard]] — ratatui-based dashboard, spinner rule for all background operations, visible status indicators, RenderCache for memory efficiency, embed backfill progress in status bar, TuiChannel integration; `zeph-tui` crate
- [[026-tui-subagent-management/spec|TUI Subagent Sidebar]] — interactive TUI subagent sidebar (a key), j/k navigation, Enter loads JSONL transcript, Esc returns, Tab cycling; implemented in v0.18.0
- [[030-tui-slash-autocomplete/spec|TUI Slash Autocomplete]] — inline autocomplete dropdown in TUI Insert mode when user types /; reuses filter_commands registry, Tab/Enter accepts, Esc dismisses

---

## Protocol & Integration

### Agent Communication Protocols
- [[013-acp/spec|ACP (Agent Control Protocol)]] — ACP transports, session management, permissions, fork/resume, session/close handler, capability advertisement, /agent.json endpoint, agent-client-protocol 0.10.3, current_model in SessionInfoUpdate
- [[014-a2a/spec|A2A Protocol & Agent Discovery]] — A2A protocol, agent discovery, JSON-RPC 2.0, IBCT (Invocation-Bound Capability Tokens), HMAC-SHA256 signatures, key_id rotation, X-Zeph-IBCT header

### Interprocess & Hooks
- [[027-runtime-layer/spec|Runtime Layer & Hooks]] — RuntimeLayer middleware with before_chat/after_chat/before_tool/after_tool hooks, NoopLayer, LayerContext, hook failure non-fatality, turn_number tracking, unwind guards
- [[028-hooks/spec|File & Directory Hooks]] — reactive hooks for cwd_changed / file_changed events, set_working_directory tool, FileChangeWatcher, ZEPH_* env vars in hook shells

---

## Advanced Features

### Code Indexing
- [[017-index/spec|Code Indexing & Retrieval]] — AST-based code indexing, semantic retrieval, repo map generation; `zeph-index` crate enables code-aware context injection

### Scheduling
- [[018-scheduler/spec|Periodic Task Scheduler]] — cron-based scheduler, SQLite persistence, CLI subcommand (zeph schedule list/add/remove/show); `zeph-scheduler` crate

### Gateway & Webhooks
- [[019-gateway/spec|HTTP Gateway]] — webhook ingestion, bearer token authentication; `zeph-gateway` crate for incoming event integration

### Benchmarking
- [[034-zeph-bench/spec|Benchmark Harness]] — BenchmarkChannel, dataset loaders (LongMemEval, LOCOMO, FRAMES, tau-bench, GAIA), CLI `zeph bench run`, memory isolation, deterministic mode, baseline comparison; `zeph-bench` crate

---

## System-Wide Features

### Feature Flags & Dependencies
- [[029-feature-flags/spec|Feature Flags]] — feature flag decision rules, surviving flag inventory (22 flags), bundle definitions (desktop, ide, server, full), always-on capabilities (openai, compatible, orchestrator, router, self-learning, qdrant, vault-age, mcp); `default = []` in Cargo.toml

### Database Abstraction
- [[031-database-abstraction/spec|PostgreSQL Backend & Database Abstraction]] — zeph-db crate, DatabaseDriver trait, Dialect trait, sql!() macro, PostgreSQL migrations, MemoryConfig::database_url, zeph db migrate CLI, --init backend selection; mutually exclusive sqlite/postgres features

### Profiling & Tracing
- [[035-profiling/spec|Profiling and Tracing Instrumentation]] — two-tier telemetry backend (Tier 1: local chrome traces, Tier 2: OTLP + Pyroscope), per-span instrumentation via #[instrument] macros, allocation tracking (profiling-alloc), system metrics (sysinfo), InstrumentedChannel wrappers; zero-overhead when disabled; `profiling`, `profiling-alloc`, `profiling-pyroscope` feature flags

---

## Special Topics & Documentation

### Handoff Protocol
- [[032-handoff-skill-system/spec|Skill-Based Handoff Protocol]] — YAML handoff protocol for inter-agent communication, structured skill exchange format

### Subagent Context
- [[033-subagent-context-propagation/spec|Subagent Context Propagation]] — gap analysis of `/agent spawn` context vs Claude Code reference, 12 gaps (P1–P4), phase-based fix plan; documents GAP-07 (cwd) and GAP-08b (loop exits) resolution

---

## Status & Phase Tracking

| ID | Feature | Phase | Status |
|----|---------|-------|--------|
| 001 | [[001-system-invariants/spec\|System Invariants]] | specify | approved |
| 002 | [[002-agent-loop/spec\|Agent Loop]] | specify | approved |
| 003 | [[003-llm-providers/spec\|LLM Providers]] | specify | approved |
| 004 | [[004-memory/spec\|Memory Pipeline]] | specify | approved |
| 005 | [[005-skills/spec\|Skills System]] | specify | approved |
| 006 | [[006-tools/spec\|Tool Execution]] | specify | approved |
| 007 | [[007-channels/spec\|Channel System]] | specify | approved |
| 008 | [[008-mcp/spec\|MCP Client]] | specify | approved |
| 009 | [[009-orchestration/spec\|Orchestration]] | specify | approved |
| 010 | [[010-security/spec\|Security]] | specify | approved |
| 011 | [[011-tui/spec\|TUI Dashboard]] | specify | approved |
| 012 | [[012-graph-memory/spec\|Entity Graph]] | specify | approved |
| 013 | [[013-acp/spec\|ACP Protocol]] | specify | approved |
| 014 | [[014-a2a/spec\|A2A Protocol]] | specify | approved |
| 015 | [[015-self-learning/spec\|Self-Learning]] | specify | approved |
| 016 | [[016-output-filtering/spec\|Output Filtering]] | specify | approved |
| 017 | [[017-index/spec\|Code Indexing]] | specify | approved |
| 018 | [[018-scheduler/spec\|Scheduler]] | specify | approved |
| 019 | [[019-gateway/spec\|Gateway]] | specify | approved |
| 020 | [[020-config-loading/spec\|Config Loading]] | specify | approved |
| 022 | [[022-config-simplification/spec\|Provider Registry]] | specify | approved |
| 023 | [[023-complexity-triage-routing/spec\|Complexity Triage]] | specify | approved |
| 024 | [[024-multi-model-design/spec\|Multi-Model Design]] | specify | approved |
| 025 | [[025-classifiers/spec\|ML Classifiers]] | specify | approved |
| 026 | [[026-tui-subagent-management/spec\|TUI Subagents]] | specify | approved |
| 026 | [[026-tui-subagent-management/plan\|TUI Subagents]] | plan | approved |
| 027 | [[027-runtime-layer/spec\|Runtime Layer]] | specify | approved |
| 028 | [[028-hooks/spec\|Hooks]] | specify | approved |
| 029 | [[029-feature-flags/spec\|Feature Flags]] | specify | approved |
| 030 | [[030-tui-slash-autocomplete/spec\|TUI Slash Autocomplete]] | specify | approved |
| 031 | [[031-database-abstraction/spec\|Database Abstraction]] | specify | approved |
| 032 | [[032-handoff-skill-system/spec\|Handoff Protocol]] | specify | approved |
| 033 | [[033-subagent-context-propagation/spec\|Subagent Context]] | research | approved |
| 034 | [[034-zeph-bench/spec\|Benchmark Harness]] | specify | approved |
| 035 | [[035-profiling/spec\|Profiling & Tracing]] | specify | draft |

---

## Decomposed Specifications

The following large specs have been broken into atomic child specs for focused study. Parent specs serve as indices:

### Memory System (004)
| Spec | Topic |
|------|-------|
| [[004-1-architecture]] | Core memory pipeline (SQLite, Qdrant, ResponseCache) |
| [[004-2-compaction]] | Deferred summaries, compaction probe, context pressure |
| [[004-3-admission-control]] | A-MAC admission control, five-factor importance scoring |
| [[004-4-embeddings]] | Embedding backfill, batch strategies, TUI integration |
| [[004-5-temporal-decay]] | Ebbinghaus forgetting curve, retention scoring |

### MCP Client (008)
| Spec | Topic |
|------|-------|
| [[008-1-lifecycle]] | Server startup, connection management, graceful shutdown |
| [[008-2-discovery]] | Tool discovery, semantic pruning, collision detection |
| [[008-3-security]] | Elicitation phases, injection detection, OAP authorization |

### Security Framework (010)
| Spec | Topic |
|------|-------|
| [[010-1-vault]] | Age encryption, credential resolution, vault access control |
| [[010-2-injection-defense]] | IPI detection (regex + DeBERTa), PII NER redaction |
| [[010-3-authorization]] | Capability-based RBAC, shell sandbox, SSRF protection |
| [[010-4-audit]] | Immutable audit trail, correlation analysis, env scrubbing |

---

## Navigation

- **By Layer**: [[#Foundation & Architecture]] → [[#Core Agent Systems]] → [[#Execution & Tools]] → [[#User Interface & Channels]]
- **By Phase**: Specs 001–030 are Phase 1 (specification); only 026 has Phase 2 (plan)
- **By Crate**: See crate field in README.md for crate mapping
- **Search**: Use Obsidian search by tag (e.g., `tag:sdd`) or filter by status

---

## Legend

- **Phase**: specify (requirements) | plan (technical design) | tasks (implementation) | research (investigation)
- **Status**: draft | approved | deprecated | research
- **Related**: See `related` field in each spec's frontmatter for explicit cross-references
