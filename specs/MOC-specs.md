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

## Business and Requirements Documentation

- [[BRD]] — Business Requirements Document: executive summary, problem statement, target personas (CLI Developer, Power User/TUI, Remote User/Telegram, Team Operator), functional requirements, business constraints, success criteria, open questions
- [[SRS]] — Software Requirements Specification (ISO/IEC/IEEE 29148:2018): system context diagram, full functional requirements in EARS notation for all 17 subsystems, traceability matrix to BRD, verification matrix
- [[NFR]] — Non-Functional Requirements (ISO/IEC 25010:2011): measurable quality targets for all 8 ISO 25010 characteristics plus operational safety; verification matrix with methods and environments

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
  - [[004-memory/004-16-memory-type-aware-retrieval|MemGuard Type-Aware Retrieval (memory sub-spec)]] — opt-in fetch-time gate on `schedule_context_fetchers`, `FunctionalType` enum, intent-scoped widening via existing `HeuristicRouter` (no new LLM call), `BehavioralRule` always-composed safety invariant; retrieval-only, byte-for-byte no-op when disabled; GitHub #6086, #6226
  - [[004-memory/004-16-shadow-memory-safety|Shadow Memory Safety (memory sub-spec)]] — `TrajectoryRiskAccumulator` MAGE multi-turn goal-hijacking detection, `ShadowMemory`/`GoalDriftResult`; SafeHarbor guardrail tree aspirational; GitHub #3695
  - [[004-memory/004-17-implicit-conflict-detection|Implicit Conflict Detection (memory sub-spec)]] — write-time `ImplicitConflictDetector` (STALE/CUPMem fuzzy predicate matching), propagation-aware SYNAPSE recall; GitHub #3702
  - [[004-memory/004-18-five-signal-retrieval|Five-Signal Retrieval (memory sub-spec)]] — access frequency, causal distance, novelty, recency, goal-relevance signals + async consolidation daemon (MemTier); GitHub #3703
- [[067-knowledge-ingest/spec|Knowledge Ingest]] — `zeph knowledge ingest` operator command; static artifacts → semantic notes (existing `IngestionPipeline`, no graph), subagent transcripts → graph (gated by measurement spike); Phase 0 provenance (`origin`/`import_batch_id`/`source_uri`) + `rollback`; honors write-gate (004-9) + admission (004-3), bypasses only RPE; sanitizer on write path; external Claude/Codex import deferred; code stays in [[017-index/spec|zeph-index]]

### Configuration & Loading
- [[020-config-loading/spec|Config Loading]] — config resolution order, mode-agnostic defaults, environment overrides
- [[022-config-simplification/spec|Provider Registry]] — see LLM Providers section above
- [[037-config-schema/spec|Config Schema]] — canonical TOML section inventory, validation rules, env-var override table, migration mechanism for `zeph-config` crate
- [[076-cli-init-migrate-config-flag-mismatch/spec|CLI Init/Migrate-Config Flag Mismatch]] — bug spec: `init`/`migrate-config` exist only as clap subcommands, but every mandatory doc (both CLAUDE.md files, `.zeph/zeph.md`, `crates/zeph-config/AGENTS.md`, a live-testing playbook, and `src/cli.rs`'s own doc comments) documents them as `--init`/`--migrate-config` flags; two remediation paths (flag-alias restoration per #587 precedent, or doc correction) left open for a future planning session
- [[077-safe-mode-and-cd-command/spec|Safe Mode & /cd Command]] — backfilled spec: `--safe-mode`/`ZEPH_SAFE_MODE` disables project-context/plugin/skill/hook/MCP loading for one session (orthogonal to `--bare`, gated across all 6 session entry points); `/cd <path>` reuses the existing `set_working_directory`/`check_cwd_changed` pipeline, re-scopes the repo-map and `CLAUDE.md`/`AGENTS.md` discovery, and rebuilds only the volatile system-prompt block to preserve Claude prompt-cache breakpoints; GitHub #6031, #6032

### Background Task Management
- [[039-background-task-supervisor/spec|Supervised Background Task Manager]] — (proposed) AgentTaskSupervisor with JoinSet, task priority classes (Critical/Enrichment/Telemetry), queue depth limits, turn-boundary cleanup, metrics integration (`bg_inflight`, `bg_dropped`, `bg_completed`); addresses GitHub issue #2816

### Context Management
- [[021-zeph-context/spec|Context Crate]] — `zeph-context` crate: `ContextBudget` token arithmetic, `CompactionState` state machine (Ready → CompactedThisTurn → Cooling → Exhausted), `ContextAssembler` parallel fetch via `FuturesUnordered`, `PreparedContext` output; extracted from `zeph-core` with no reverse dependency

### Shared Primitives
- [[043-zeph-common/spec|Shared Primitives]] — `zeph-common` crate: `Secret` (zeroize-on-drop, redacted Debug), `ToolName` (Arc<str>, O(1) clone), `SessionId` (UUID v4), `ToolDefinition`, `SkillTrustLevel`, `PolicyLlmClient`, sanitization helpers; no `zeph-*` peer dependencies

### Slash Command Dispatch
- [[042-zeph-commands/spec|Slash Command Registry]] — `zeph-commands` crate: `CommandRegistry` with longest-word-boundary dispatch, object-safe `CommandHandler<Ctx>` trait, `CommandOutput` enum, `ChannelSink` abstraction, static `COMMANDS` list for `/help` and TUI autocomplete; no dependency on `zeph-core`

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
- [[009-orchestration/spec|Orchestration & Planning]] — DAG planner, DagScheduler, AgentRouter, /plan command, plan template cache, VMAO adaptive replanning, cascade-aware DAG routing with CascadeDetector, tree-optimized dispatch, verifier tool-call grounding (deterministic `ground()` cross-check of narrated completions against the real tool-execution trace, `VerifyResponse` DTO, bidirectional-containment matching, tri-state trace availability, ensemble union-post-merge; #6278); defines strategy for multi-step task execution
- [[074-orchestration-hitl-interrupt/spec|Declarative HITL Interrupt]] — LangGraph `interrupt()` parity: `TaskNode.interrupt_before`/`resolved_input` pre-dispatch gate, `TaskGraph.pause_reason` (blob-only, no `DurablePromise` in Phase 1), `/plan provide <value>` command, `GraphStatus::Paused` reuse; extends [[009-orchestration/spec|Orchestration & Planning]]; GitHub #5918
- [[075-orchestration-node-control-parity/spec|Node Timeout / Retry-Exhausted Recovery]] — LangGraph `TimeoutPolicy`/error-handler parity: per-task `TimeoutPolicy` (`run_timeout_secs` enforced on spawned + RunInline tasks, `idle_timeout_secs` defined but a documented no-op in v1), `RecoveryAction { state_injection }` Mode-1 substitute-and-continue recovery on terminal failure (cascade-abort takes precedence, no resume re-scan needed); `route_to` reroute-to-alternate (Mode 2) deferred — dependency-based dormancy proved inverted; extends [[009-orchestration/spec|Orchestration & Planning]]; GitHub #6021

---

## Security & Validation

### Security Framework
- [[010-security/spec|Security & Content Isolation]] — Vault secret management, shell sandbox, content isolation, SSRF protection, IPI defense (DeBERTa soft-signal, AlignSentinel 3-class, TurnCausalAnalyzer), PII NER circuit breaker + allowlist, cross-tool injection correlation, AgentRFC protocol audit, MCP→ACP confused-deputy boundary enforcement, SMCP lifecycle + IBCT tokens, credential env-var scrubbing, MCP tool input schema injection scan
- [[038-vault/spec|Vault & Secret Management]] — VaultProvider trait, age encryption backend, env backend (testing), zeroize-on-drop guarantee, vault config schema, key invariants, multi-recipient vaults; `zeph-vault` crate

### ML Classifiers & Content Sanitization
- [[025-classifiers/spec|Candle-backed ML Classifiers]] — injection detection (CandleClassifier), PII detection (CandlePiiClassifier), LlmClassifier for feedback, unified regex+NER sanitization pipeline; provides signals for [[010-security/spec|Security Framework]]
- [[040-sanitizer/spec|Content Sanitizer]] — spotlighting pipeline, regex injection detection, PII scrubber, guardrail filter, quarantined summarizer, response verification, exfiltration guards, memory validation, causal analysis; eight-layer defense-in-depth

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

### Durable Execution
- [[064-durable-execution/spec|Durable Execution]] — `zeph-durable` Layer-0 crate: append-only journal, `DurableStep`/`DurableContext` (`&self` + `AtomicU32`), `EffectClass`+`OnAmbiguous` (construction-time error for destructive-unspecified), `JournalWriter` actor (mpsc capacity=1024, group-commit, ACK, supervised restart), AEAD `PayloadCipher` (XChaCha20-Poly1305, vault-keyed `ZEPH_DURABLE_KEY`, AAD-bound to step identity), `DurablePromise`/resolver-token auth, `DurableTimer`, dedicated `durable.db` (own pool+migrations), `ReplayDivergence` fingerprint guard, `read_execution_range` cursor (O(segment)); P1 agent-loop (explicit tier_loop.rs rewrite, LLM gate), P2 orchestration `/plan resume` (replan-budget restore), P3 scheduler exactly-once, P4 subagent durable promise; `restate` optional feature in `server` bundle

### Deep Link Scheme
- [[066-deep-link-scheme/spec|Deep Link Scheme]] — `zeph://` URI scheme: `zeph url-open`, `zeph url-scheme {register,unregister,status}`, OS registration (Linux `.desktop`/Windows HKCU full, macOS dispatch-only stub), INV-CWD security validation order, `[deep_link]` config section, `deep-link` feature flag; ACP attach deferred to v2; GitHub #4687

### Session Persistence
- [[068-session-persistence/spec|Session Persistence]] — new `zeph-session` crate: append-only JSONL `SessionEventLog` as source of truth, deterministic `ReplayEngine` (no tool re-execution), eager-copy `ForkEngine`, `Condenser`/`LlmCondenser`, INV-SP-1..4 crash invariants, per-session `SessionActor` (mpsc-in/broadcast-out) + `LiveSessionRegistry` for `zeph serve`, HTTP/SSE API, `/conv` TUI commands, SQLite+PostgreSQL migration 105; GitHub #2807, #3102, #3074

### Threat Model
- [[069-threat-model/spec|MATRA Threat Model]] — asset-centric threat model (arXiv:2605.10763): asset inventory (vault, SQLite, Qdrant, ShellExecutor, WebScrapeExecutor, channel adapters, MCP client, subagent transcripts, orchestration planner), attack trees, control mapping, uncontrolled blast radius; `NetworkScope` + `AssetSensitivity` classification on orchestration `TaskNode` (advisory-only pending runner wiring); GitHub #3913, #3934

### Transcript Integrity
- [[081-transcript-integrity/spec|Transcript Integrity]] — tamper-evident persisted history (competitive-parity finding vs. Claude Code 2.1.205): keyed-BLAKE3 hash-chain over sub-agent transcript and session event log JSONL (new `ZEPH_HISTORY_KEY` vault root secret), detecting in-place edits, reordering, partial chain-strip, and key-epoch tampering, all fail-closed; durable journal uses a separate authenticated per-execution high-water-mark instead of a chain (positional chains are incompatible with `checkpoint_fold` compaction), default-on whenever `ZEPH_DURABLE_KEY` is provisioned, with its own key-rotation window riding the AEAD cipher's `key_id`/`previous_key_id` lifecycle (#6460); the whole-file/whole-execution downgrade-to-legacy strip gap is closed via a vault-anchor mechanism (GitHub #6449, #6461); GitHub #6360 [implemented]

### Per-Message Usage/Cost Tracking
- [[082-per-message-usage-cost-tracking/spec|Per-Message Usage/Cost Tracking]] — new `usage_records` table (migration 115, both dialects) additive to `CostTracker`'s daily aggregate; every `CostTracker`-feeding site (turn loop, planner + aggregator, ensemble members) writes a paired row via `CostTracker::price_of`, inline-awaited, no new `tokio::spawn` site; `ttft_ms` true time-to-first-token on the speculative-decoding streaming path or a TTFB proxy otherwise; Goose per-message usage-stats parity gap; GitHub #6549 [implemented]

### Memory Write Consent Gate
- [[083-memory-write-consent-gate/spec|Memory Write Consent Gate]] — write-time consent gate ("MemGhost") for untrusted memory writes: provenance tagging, confirm/disclose thresholds, and audit logging gating what untrusted-origin content may be committed to memory, distinct from `004-9`'s MemReader quality scorer; closes cross-turn/cross-tier/reload TOCTOU bypasses and write-time provenance/audit gating gaps; GitHub #6544

---

## System-Wide Features

### Feature Flags & Dependencies
- [[029-feature-flags/spec|Feature Flags]] — feature flag decision rules, surviving flag inventory, bundle definitions (desktop, ide, server, full), always-on capabilities (openai, compatible, orchestrator, router, self-learning, qdrant, vault-age, mcp); `default = ["scheduler", "sqlite"]` in Cargo.toml
- [[041-experiments/spec|Experiments & Runtime Feature Gating]] — runtime A/B testing via `[experiments]` config section, ExperimentConfig, rollout percentage, experiment results reporting, CLI subcommands; distinct from compile-time feature flags

### Database Abstraction
- [[031-database-abstraction/spec|PostgreSQL Backend & Database Abstraction]] — zeph-db crate, DatabaseDriver trait, Dialect trait, sql!() macro, PostgreSQL migrations, MemoryConfig::database_url, zeph db migrate CLI, --init backend selection; mutually exclusive sqlite/postgres features

### Profiling & Tracing
- [[035-profiling/spec|Profiling and Tracing Instrumentation]] — two-tier telemetry backend (Tier 1: local chrome traces, Tier 2: OTLP + Pyroscope), per-span instrumentation via #[instrument] macros, allocation tracking (profiling-alloc), system metrics (sysinfo), InstrumentedChannel wrappers; zero-overhead when disabled; `profiling`, `profiling-alloc`, `profiling-pyroscope` feature flags

### Metrics Export
- [[036-prometheus-metrics/spec|Prometheus Metrics Export]] — aggregated time-series `/metrics` endpoint (OpenMetrics 1.0.0 format), ~25 gauge/counter metrics from MetricsSnapshot, periodic sync task, feature-gated with gateway; complements TUI gauges and distributed tracing; `prometheus` feature flag

---

## Special Topics & Documentation

### Handoff Protocol
- [[032-handoff-skill-system/spec|Skill-Based Handoff Protocol]] — YAML handoff protocol for inter-agent communication, structured skill exchange format

### Subagent Context
- [[033-subagent-context-propagation/spec|Subagent Context Propagation]] — gap analysis of `/agent spawn` context vs Claude Code reference, 12 gaps (P1–P4), phase-based fix plan; documents GAP-07 (cwd) and GAP-08b (loop exits) resolution
- [[044-subagent-lifecycle/spec|Subagent Lifecycle]] — full `zeph-subagent` crate spec: `SubAgentDef` parsing, `SubAgentManager` spawning and concurrency cap, `PermissionGrants` TTL, `FilteredToolExecutor` policy gate, transcript persistence, lifecycle hooks, and memory injection

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
| 027 | [[027-runtime-layer/spec\|Runtime Layer]] | specify | approved |
| 028 | [[028-hooks/spec\|Hooks]] | specify | approved |
| 029 | [[029-feature-flags/spec\|Feature Flags]] | specify | approved |
| 030 | [[030-tui-slash-autocomplete/spec\|TUI Slash Autocomplete]] | specify | approved |
| 031 | [[031-database-abstraction/spec\|Database Abstraction]] | specify | approved |
| 032 | [[032-handoff-skill-system/spec\|Handoff Protocol]] | specify | approved |
| 033 | [[033-subagent-context-propagation/spec\|Subagent Context]] | specify | approved |
| 034 | [[034-zeph-bench/spec\|Benchmark Harness]] | specify | approved |
| 035 | [[035-profiling/spec\|Profiling & Tracing]] | specify | approved |
| 036 | [[036-prometheus-metrics/spec\|Prometheus Metrics]] | specify | approved |
| 037 | [[037-config-schema/spec\|Config Schema]] | specify | approved |
| 038 | [[038-vault/spec\|Vault & Secret Management]] | specify | approved |
| 039 | [[039-background-task-supervisor/spec\|Background Task Supervisor]] | specify | draft |
| 040 | [[040-sanitizer/spec\|Content Sanitizer]] | specify | approved |
| 041 | [[041-experiments/spec\|Experiments & Runtime Feature Gating]] | specify | approved |
| 021 | [[021-zeph-context/spec\|Context Crate]] | specify | approved |
| 042 | [[042-zeph-commands/spec\|Slash Command Registry]] | specify | approved |
| 043 | [[043-zeph-common/spec\|Shared Primitives]] | specify | approved |
| 044 | [[044-subagent-lifecycle/spec\|Subagent Lifecycle]] | specify | approved |
| 045 | [[045-interop-protocol-gaps/spec\|Interop Protocol Gaps]] | specify | approved |
| 046 | [[046-march-quality/spec\|MARCH Quality Pipeline]] | specify | approved |
| 047 | [[047-cli-modes/spec\|CLI Execution Modes]] | specify | approved |
| 048 | [[048-slm-cost-metrics/spec\|SLM Cost Metrics]] | specify | approved |
| 049 | [[049-agent-decomposition/spec\|Agent Decomposition]] | specify | draft |
| 050 | [[050-security-capability-governance/spec\|Security Capability Governance]] | specify | draft |
| 051 | [[051-gonka-gateway/spec\|Gonka Gateway]] | specify | implemented |
| 052 | [[052-gonka-native/spec\|Gonka Native]] | specify | implemented |
| 053 | [[053-speculation-engine/spec\|Speculation Engine]] | specify | implemented |
| 054 | [[054-agent-feedback/spec\|Agent Feedback Detection]] | specify | approved |
| 055 | [[055-cocoon/spec\|Cocoon Distributed Compute]] | specify | draft |
| 056 | [[056-autoskill-trace-extraction/spec\|AutoSkill A1: Trace Extraction]] | specify | implemented |
| 057 | [[057-autoskill-versioned-merging/spec\|AutoSkill A2: Versioned Merging]] | specify | implemented |
| 058 | [[058-autoskill-query-rewriting/spec\|AutoSkill A3: Query Rewriting]] | specify | implemented |
| 059 | [[059-autoskill-bm25-hybrid/spec\|AutoSkill A4: BM25 Hybrid]] | specify | implemented |
| 060 | [[060-autoskill-trigger-sets/spec\|AutoSkill A5: Trigger Sets]] | specify | implemented |
| 061 | [[061-autoskill-heuristic-promotion/spec\|AutoSkill A6: Heuristic Promotion]] | specify | implemented |
| 062 | [[062-context-adaptive-memory/spec\|Context-Adaptive Memory]] | tasks | approved |
| 063 | [[063-worktree-subsystem/spec\|Worktree Subsystem]] | specify | approved |
| 064 | [[064-durable-execution/spec\|Durable Execution]] | specify | approved |
| 065 | [[065-ephemeral-plugins-provider-overrides/spec\|Ephemeral Plugins & Provider Overrides]] | specify | implemented |
| 066 | [[066-deep-link-scheme/spec\|Deep Link Scheme]] | specify | approved |
| 067 | [[067-knowledge-ingest/spec\|Knowledge Ingest]] | specify | draft |
| 068 | [[068-session-persistence/spec\|Session Persistence]] | specify | draft |
| 069 | [[069-threat-model/spec\|MATRA Threat Model]] | specify | approved |
| 070 | [[070-runtime-thinking-controls/spec\|Runtime Thinking Controls]] | specify | approved |
| 071 | [[071-router-thinking-budget-delegation/spec\|Router Thinking Budget Delegation]] | specify | approved |
| 072 | [[072-multimodal-mcp-passthrough/spec\|Multimodal MCP Passthrough]] | specify | draft |
| 073 | [[073-orch-ensemble-merge/spec\|ORCH Ensemble-Merge]] | specify | approved |
| 074 | [[074-orchestration-hitl-interrupt/spec\|Declarative HITL Interrupt]] | tasks | draft |
| 075 | [[075-orchestration-node-control-parity/spec\|Node Timeout / Retry-Exhausted Recovery]] | tasks | approved |
| 076 | [[076-cli-init-migrate-config-flag-mismatch/spec\|CLI Init/Migrate-Config Flag Mismatch]] | specify | draft |
| 077 | [[077-safe-mode-and-cd-command/spec\|Safe Mode & /cd Command]] | specify | implemented |
| 078 | [[078-agent-persistence/spec\|Agent Persistence]] | specify | approved |
| 079 | [[079-plugins/spec\|Plugin Management]] | specify | approved |
| 080 | [[080-cross-thread-store-dynamic-handoff/spec\|Cross-Thread Store & Dynamic Handoff]] | specify | approved |
| 081 | [[081-transcript-integrity/spec\|Transcript Integrity]] | specify | implemented |
| 082 | [[082-per-message-usage-cost-tracking/spec\|Per-Message Usage/Cost Tracking]] | specify | implemented |
| 083 | [[083-memory-write-consent-gate/spec\|Memory Write Consent Gate]] | specify | implemented |

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
| [[010-5-egress-logging]] | `EgressEvent` per outbound HTTP call, correlation_id, bounded telemetry, TUI surface |
| [[010-6-vigil-intent-anchoring]] | Verify-before-commit regex tripwire with Block/Sanitize + per-turn intent, subagent exemption |

---

## Navigation

- **By Layer**: [[#Foundation & Architecture]] → [[#Core Agent Systems]] → [[#Execution & Tools]] → [[#User Interface & Channels]]
- **By Phase**: Specs 001–061 are Phase 1 (specification) only; several later specs (062, 063, 065, 066, 067, 068, 070, 072, 073, 074, 075) additionally have `plan.md`/`tasks.md` (and some `brd.md`/`srs.md`/`nfr.md`) companion documents — see each directory for its actual file set
- **By Crate**: See crate field in README.md for crate mapping
- **Search**: Use Obsidian search by tag (e.g., `tag:sdd`) or filter by status

---

## Legend

- **Phase**: specify (requirements) | plan (technical design) | tasks (implementation) | research (investigation)
- **Status**: draft | approved | deprecated | research
- **Related**: See `related` field in each spec's frontmatter for explicit cross-references
