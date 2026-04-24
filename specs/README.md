---
aliases:
  - Specs Readme
  - Specs Navigation
  - Zeph Specifications Index
tags:
  - sdd
  - index
  - reference
created: 2026-04-08
status: permanent
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
---

# Zeph Specs

Feature principle documents — key invariants for coding agents.
See `[[constitution]]` for project-wide non-negotiable rules.

**See `[[MOC-specs]]` for the comprehensive index of all specifications organized by topic.**

---

## Numbering Scheme

Spec IDs (001–044) follow a logical grouping:

- **001–010**: Foundational contracts and core systems (invariants, loop, providers, memory, skills, tools, channels, mcp, orchestration, security)
- **011–020**: User-facing features and operational integration (TUI, graph memory, protocols, self-learning, filtering, indexing, scheduler, gateway, config loading)
- **021**: `zeph-context` crate — context budget, compaction state machine, context assembler
- **022–034**: Architectural extensions and specialized features (provider registry, complexity routing, multi-model design, classifiers, TUI enhancements, hooks, database abstraction, handoff, feature flags, subagent context, benchmark harness)
- **035–037**: Observability and configuration (profiling/tracing instrumentation, Prometheus metrics export, config schema)
- **038–041**: Infrastructure and security (vault, background task supervisor, content sanitizer, experiments)
- **042–044**: Foundation crates (slash command registry, shared primitives, subagent lifecycle)
- **045**: Agent interoperability protocol gap analysis
- **046**: MARCH Proposer+Checker quality pipeline
- **047**: CLI execution modes (--bare, --json, -y, /loop, /recap)

---

## Business and Requirements Documentation

| Doc | Contents |
|---|---|
| `BRD.md` | Business Requirements Document — what Zeph is, why it exists, target personas, business constraints, success criteria |
| `SRS.md` | Software Requirements Specification (ISO/IEC/IEEE 29148:2018) — all functional requirements grouped by subsystem, EARS notation, traceability to BRD |
| `NFR.md` | Non-Functional Requirements (ISO/IEC 25010:2011) — measurable quality targets for performance, reliability, security, maintainability, portability, usability, compatibility, and safety |

## System Invariants

| Doc | Contents |
|---|---|
| `001-system-invariants/spec.md` | Cross-cutting architectural invariants |

## Feature Docs

| Doc | Feature | Crate |
|---|---|---|
| `002-agent-loop/spec.md` | Agent loop, turn lifecycle, context pressure, HiAgent subgoal-aware compaction | `zeph-core` |
| `003-llm-providers/spec.md` | LlmProvider trait, AnyProvider, prompt caching, configurable `CacheTtl` (ephemeral/1h) | `zeph-llm` |
| `004-memory/spec.md` | SQLite + Qdrant, compaction, semantic response cache, anchored summarization, compaction probe, importance scoring, A-MAC admission control, MemScene consolidation, multi-vector chunking, GAAMA episode nodes, BATS budget hints, Focus compression, SleepGate forgetting pass, persona memory, trajectory memory, category-aware memory, TiMem tree, microcompact, autoDream, MagicDocs, embed backfill batching | `zeph-memory` |
| `004-memory/004-7-memory-apex-magma.md` | APEX-MEM append-only MAGMA: edge supersession, ontology normalization, SYNAPSE conflict resolution (#3223) | `zeph-memory` |
| `004-memory/004-8-memory-typed-pages.md` | ClawVM typed page compaction: PageType classification, minimum-fidelity invariants, compaction audit log (#3221) | `zeph-context`, `zeph-memory` |
| `004-memory/004-9-memory-write-gate.md` | MemReader write quality gate: three-signal scorer, rule-based MVP, optional LLM scoring (#3222) | `zeph-memory` |
| `004-memory/004-10-memory-memmachine-retrieval.md` | MemMachine retrieval-depth-first memory: retrieval depth config, search prompt templates, query bias correction, episode preservation (#3325) | `zeph-memory` |
| `004-memory/004-11-memory-hela-mem.md` | HeLa-Mem Hebbian learning: edge weight reinforcement, periodic consolidation, spreading activation retrieval (#3324) | `zeph-memory` |
| `004-memory/004-12-memory-reasoning-bank.md` | ReasoningBank: self-judge + distillation pipeline, strategy embedding store, context preamble injection (#3312) | `zeph-memory`, `zeph-core` |
| `005-skills/spec.md` | SKILL.md format, registry, matching, hot-reload, skill trust governance, two-stage matching, Wilson score confidence intervals, hub install pipeline, agent-invocable skills (`invoke_skill`) | `zeph-skills` |
| `006-tools/spec.md` | ToolExecutor, CompositeExecutor, TAFC, schema filter, result cache, dependency graph, tool invocation phase taxonomy, native `tool_use` only; `invoke_skill`/`load_skill` utility-gate exemption | `zeph-tools` |
| `007-channels/spec.md` | Channel trait, AnyChannel dispatch, streaming, channel feature parity | `zeph-channels` |
| `008-mcp/spec.md` | MCP client, server lifecycle, semantic tool discovery, per-message pruning cache, injection detection, tool collision detection, caller identity propagation, tool quota, structured error codes, OAP authorization, elicitation (2025-06-18) | `zeph-mcp` |
| `009-orchestration/spec.md` | DAG planner, DagScheduler, AgentRouter, /plan command, plan template cache, VMAO adaptive replanning, cascade-aware DAG routing, VeriMAP predicate gate, AdaptOrch topology advisor, CoE entropy routing, graph persistence in scheduler loop | `zeph-orchestration` |
| `010-security/spec.md` | Vault, shell sandbox, content isolation, SSRF protection, IPI defense, PII NER circuit breaker, cross-tool injection correlation, AgentRFC protocol audit, MCP→ACP boundary enforcement, credential env-var scrubbing, file permission hardening (`fs_secure`), Seatbelt deny-first secret-path rules | cross-cutting |
| `010-security/010-5-egress-logging.md` | Egress logging sub-spec: `EgressEvent` per outbound HTTP call, `AuditEntry.correlation_id`, bounded mpsc telemetry (256 + drop counter), TUI Security panel surface | `zeph-tools`, `zeph-core`, `zeph-tui` |
| `010-security/010-6-vigil-intent-anchoring.md` | VIGIL verify-before-commit sub-spec: pre-sanitizer regex tripwire with Block/Sanitize action, per-turn `current_turn_intent`, subagent exemption, non-retryable blocks via `error_category="vigil_blocked"` | `zeph-core`, `zeph-tools`, `zeph-config` |
| `011-tui/spec.md` | ratatui dashboard, spinner rule for background operations, TuiChannel, RenderCache, embed backfill progress, multi-session `SessionRegistry`, `/session` commands, compact paste indicator | `zeph-tui` |
| `012-graph-memory/spec.md` | Entity graph, BFS recall, community detection, MAGMA typed edges, SYNAPSE spreading activation | `zeph-memory` |
| `004-memory/004-6-graph-memory.md` | Graph memory sub-spec (concise reference within 004-memory): MAGMA typed edges, SYNAPSE config, A-MEM link weights, key invariants | `zeph-memory` |
| `013-acp/spec.md` | ACP transports, session management, permissions, fork/resume, session/close handlers, capability advertisement, /agent.json endpoint | `zeph-acp` |
| `014-a2a/spec.md` | A2A protocol, agent discovery, JSON-RPC 2.0, IBCT (Invocation-Bound Capability Tokens), HMAC-SHA256 signatures | `zeph-a2a` |
| `015-self-learning/spec.md` | FeedbackDetector (multi-language), Wilson score, trust model, SAGE RL cross-session reward, ARISE trace improvement, STEM pattern-to-skill migration, ERL experiential learning | `zeph-skills` |
| `016-output-filtering/spec.md` | FilterPipeline, CommandMatcher, SecurityPatterns | `zeph-tools` |
| `017-index/spec.md` | AST indexing, semantic retrieval, repo map generation | `zeph-index` |
| `018-scheduler/spec.md` | Cron scheduler, SQLite persistence, CLI subcommands (list/add/remove/show) | `zeph-scheduler` |
| `019-gateway/spec.md` | HTTP webhook ingestion, bearer token authentication | `zeph-gateway` |
| `020-config-loading/spec.md` | Config resolution order, mode-agnostic defaults, environment overrides, `--migrate-config --in-place` idempotency | `zeph-core` |
| `021-zeph-context/spec.md` | `zeph-context` crate: `ContextBudget` token arithmetic, `CompactionState` state machine, `ContextAssembler` parallel fetch, `PreparedContext` output; extracted from `zeph-core` | `zeph-context` |
| `022-config-simplification/spec.md` | Provider Registry Architecture: canonical `[[llm.providers]]` format, ProviderEntry schema, routing strategies, LinUCB bandit routing, cost-weight dial, memory-augmented routing | `zeph-config`, `zeph-core` |
| `023-complexity-triage-routing/spec.md` | Pre-inference complexity classification routing, ComplexityTier, TriageRouter, context escalation, metrics | `zeph-llm`, `zeph-config`, `zeph-core` |
| `024-multi-model-design/spec.md` | Multi-model design principle: complexity tiers, `*_provider` subsystem reference pattern, STT unification | cross-cutting |
| `025-classifiers/spec.md` | Candle-backed ML classifiers: injection detection, PII detection, LlmClassifier for feedback, unified regex+NER sanitization pipeline | `zeph-classifiers` |
| `026-tui-subagent-management/spec.md` | Interactive TUI subagent sidebar (a key), j/k navigation, Enter loads transcript, Esc returns, Tab cycling | `zeph-tui` |
| `027-runtime-layer/spec.md` | RuntimeLayer middleware with before_chat/after_chat/before_tool/after_tool hooks, NoopLayer, LayerContext, unwind guards; plugin config overlay merge (tighten-only) | `zeph-core` |
| `028-hooks/spec.md` | Reactive hooks: cwd_changed / file_changed events, set_working_directory tool, FileChangeWatcher, ZEPH_* env vars | `zeph-core` |
| `029-feature-flags/spec.md` | Feature flag decision rules, surviving flag inventory (22 flags), bundle definitions (desktop, ide, server, full) | `Cargo.toml`, cross-cutting |
| `030-tui-slash-autocomplete/spec.md` | Inline autocomplete dropdown in TUI Insert mode, reuses filter_commands registry, Tab/Enter accepts, Esc dismisses | `zeph-tui` |
| `031-database-abstraction/spec.md` | PostgreSQL backend, zeph-db crate, DatabaseDriver trait, Dialect trait, sql!() macro, migrations, CLI subcommands | `zeph-db`, cross-cutting |
| `032-handoff-skill-system/spec.md` | Skill-based YAML handoff protocol for inter-agent communication, structured skill exchange | `zeph-orchestration` |
| `033-subagent-context-propagation/spec.md` | Gap analysis and resolution plan for `/agent spawn` context propagation | `zeph-subagent`, `zeph-core` |
| `034-zeph-bench/spec.md` | Benchmark harness: BenchmarkChannel, dataset loaders, CLI `zeph bench run`, memory isolation, deterministic mode, baseline comparison | `zeph-bench` |
| `035-profiling/spec.md` | Two-tier telemetry (Tier 1: local chrome traces, Tier 2: OTLP + Pyroscope), per-span `#[instrument]` macros, allocation tracking, InstrumentedChannel wrappers, system metrics; zero-overhead when disabled | cross-cutting |
| `036-prometheus-metrics/spec.md` | Prometheus `/metrics` endpoint, OpenMetrics export, ~25 gauge/counter metrics from MetricsSnapshot, feature-gated with gateway | `zeph-gateway`, binary |
| `037-config-schema/spec.md` | Canonical TOML schema reference: all top-level sections, validation rules, env-var override table, `--migrate-config` migration mechanism | `zeph-config` |
| `038-vault/spec.md` | Vault & Secret Management: VaultProvider trait, age encryption backend, env backend (testing), zeroize-on-drop guarantee, vault config schema, key invariants, multi-recipient vaults | `zeph-vault` |
| `039-background-task-supervisor/spec.md` | (Proposed) Supervised Background Task Manager: AgentTaskSupervisor, task priority classes, queue depth limits, turn-boundary cleanup, metrics (`bg_inflight`, `bg_dropped`) | `zeph-core` |
| `040-sanitizer/spec.md` | Content Sanitizer: spotlighting pipeline, regex injection detection, PII scrubber, guardrail filter, quarantined summarizer, response verification, exfiltration guards, memory validation, causal analysis (eight-layer defense-in-depth) | `zeph-sanitizer` |
| `041-experiments/spec.md` | Experiments & Runtime Feature Gating: `[experiments]` config section, ExperimentConfig, rollout percentage, experiment results reporting, CLI subcommands; distinct from compile-time feature flags | `zeph-experiments` |
| `042-zeph-commands/spec.md` | Slash command registry, `CommandHandler<Ctx>` object-safe trait, `CommandRegistry` with longest-word-boundary dispatch, `ChannelSink` abstraction, static `COMMANDS` list; `/recap` command, `/session` TUI commands; no dependency on `zeph-core` | `zeph-commands` |
| `043-zeph-common/spec.md` | Shared primitives: `Secret` (zeroize-on-drop), `ToolName` (Arc<str>), `SessionId` (UUID v4), `ToolDefinition`, `SkillTrustLevel`, `PolicyLlmClient`; no `zeph-*` peer dependencies | `zeph-common` |
| `044-subagent-lifecycle/spec.md` | Full `zeph-subagent` crate: `SubAgentDef` parsing, `SubAgentManager` spawning and concurrency cap, `PermissionGrants` TTL, `FilteredToolExecutor` policy gate, transcript JSONL persistence, lifecycle hooks, memory injection | `zeph-subagent` |
| `045-interop-protocol-gaps/spec.md` | Agent interoperability protocol gap analysis (arXiv:2505.02279): capability matrix for MCP, ACP, A2A, ANP vs. Zeph; protocol selection guidance; ANP as P4 research; ACP re-negotiation as P3 follow-up | cross-cutting |
| `046-march-quality/spec.md` | MARCH Proposer+Checker self-check pipeline: post-response factual consistency, information-asymmetry checker, `self-check` feature flag, per-turn `MarchVerdict`, Prometheus metrics (#3226) | `zeph-core` |
| `047-cli-modes/spec.md` | CLI execution modes: `--bare` (skip scheduler/indexer/eviction), `--json` (JSONL event stream), `-y` (auto-approve), `/loop` command (supervised loop with inline errors), `/recap` command (#3170, #3218) | `zeph-channels`, binary |
| `008-mcp/008-4-elicitation.md` | MCP server-driven elicitation (protocol 2025-06-18): `elicitation/create` routing to active channel, sensitive field warnings, Sandboxed trust hard-reject, Telegram timeout, URL field decline (#3218) | `zeph-mcp`, `zeph-channels` |
