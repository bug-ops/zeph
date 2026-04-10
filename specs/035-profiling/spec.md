---
aliases:
  - Profiling and Tracing Instrumentation
  - Telemetry Backend
  - Profiling System
tags:
  - sdd
  - spec
  - infra
  - observability
  - cross-cutting
created: 2026-04-10
status: draft
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[001-system-invariants/spec]]"
  - "[[011-tui/spec]]"
  - "[[027-runtime-layer/spec]]"
  - "[[029-feature-flags/spec]]"
---

# Spec: Profiling and Tracing Instrumentation

> [!info]
> Two-tier telemetry backend for performance analysis and distributed tracing. Tier 1 (local development): zero-infrastructure chrome traces via tracing-chrome. Tier 2 (production): OTLP + Pyroscope for distributed traces and continuous profiling. Feature-gated with minimal overhead when disabled.

## Sources

### External
- [tracing-chrome 0.7.2](https://docs.rs/tracing-chrome/latest/tracing_chrome/) — Chrome Trace Format writer
- [sysinfo 0.38.4](https://docs.rs/sysinfo/latest/sysinfo/) — system metrics
- [tracking-allocator 0.4.0](https://docs.rs/tracking-allocator/latest/tracking_allocator/) — per-span allocation tracking
- [pyroscope 2.0.0](https://docs.rs/pyroscope/latest/pyroscope/) — continuous profiling agent
- [OpenTelemetry Specification](https://opentelemetry.io/docs/specs/otel/) — OTLP semantics

### Internal
| File | Contents |
|---|---|
| `src/tracing_init.rs` | Existing tracing subscriber initialization |
| `src/config.rs` | Configuration loading and TelemetryConfig struct |
| `src/agent/agent_loop.rs` | Agent turn lifecycle and instrumentation sites |
| `crates/zeph-llm/src/providers/` | LLM provider instrumentation |
| `crates/zeph-memory/src/` | Memory subsystem instrumentation |
| `crates/zeph-tools/src/` | Tool execution instrumentation |
| `crates/zeph-skills/src/` | Skills system instrumentation |
| `crates/zeph-mcp/src/` | MCP client instrumentation |
| `crates/zeph-channels/src/` | Channel I/O instrumentation |

---

## 1. Overview

### Problem Statement

Performance analysis and bottleneck identification in Zeph require visibility into execution flow across agent turn phases, LLM calls, memory operations, tool execution, and inter-crate boundaries. Current logging provides event-level information but lacks:

- Timing attribution for individual functions and subsystems
- Multi-threaded task correlation in the tokio runtime
- Distributed trace propagation (for A2A and multi-agent scenarios)
- Per-span resource allocation tracking
- Visualizable timeline representation (critical for identifying cascading delays)

Development iterations require either external profilers (flamegraph, perf) or manual `Instant::now()` bookkeeping. Production deployments need continuous profiling for cost optimization and latency SLO tracking without persistent infrastructure.

### Goal

Enable structured tracing of function execution with fine-grained timing, resource allocation, and optional OTLP export to Grafana Tempo. Provide:

1. Zero-infrastructure local profiling (chrome JSON export, Perfetto UI view)
2. Optional production-tier OTLP + Pyroscope integration
3. Per-function instrumentation without manual latency tracking code
4. Per-span allocation tracking (feature-gated)
5. System metrics (RSS, CPU%, thread count, fd count) correlated with trace timeline

### Out of Scope

- In-process flame graph rendering (use exported traces with Perfetto or Grafana)
- Continuous memory leak detection (out-of-process tool territory)
- Cost attribution per LLM call (handled by existing MetricsSnapshot)
- Replacement of structured logging (tracing events remain for text-based analysis)
- Real-time TUI metrics from profiling spans (TUI remains integrated with existing MetricsSnapshot)

---

## 2. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN `[telemetry] enabled=true` and `backend="local"` THE SYSTEM SHALL write chrome trace JSON to `.local/traces/{session_id}_{timestamp}.json` | must |
| FR-002 | WHEN profiling feature flag is disabled THE SYSTEM SHALL compile #[instrument] macros to no-ops with zero overhead | must |
| FR-003 | WHEN `[telemetry] backend="otlp"` and otlp_endpoint is set THE SYSTEM SHALL export trace spans and events to the OTLP endpoint | must |
| FR-004 | WHEN profiling-alloc feature flag is enabled THE SYSTEM SHALL track per-span allocation counts and bytes as span attributes | should |
| FR-005 | WHEN profiling-pyroscope feature flag is enabled THE SYSTEM SHALL push CPU and heap profiles to Pyroscope endpoint with trace correlation | should |
| FR-006 | WHEN profiling feature flag is enabled THE SYSTEM SHALL emit sysinfo metrics (RSS, CPU%, thread count, fd count) every 5 seconds | should |
| FR-007 | WHEN telemetry.include_args=true THE SYSTEM SHALL include function arguments in span attributes (for debugging) | should |
| FR-008 | WHEN a span is entered THE SYSTEM SHALL NOT block the executor (allocation tracking must use thread-local storage) | must |
| FR-009 | WHEN ZEPH_OTEL_HEADERS vault key is present AND backend="otlp" THE SYSTEM SHALL use vault-provided auth headers for OTLP requests | must |

---

## 3. Architecture

### 3.1 Two-Tier Backend Design

**Tier 1 — Local Development**

- No infrastructure required
- Uses `tracing-chrome` 0.7.2 to write W3C Chrome Trace Format JSON
- Output: `.local/traces/{session_id}_{timestamp}.json` (gitignored)
- Viewable in Perfetto UI (`ui.perfetto.dev`)
- Overhead: 3–8%
- Config: `[telemetry] enabled=true, backend="local", trace_dir=".local/traces", include_args=true`

**Tier 2 — Production/Staging**

- OTLP (OpenTelemetry Protocol) export to Grafana Tempo or compatible receiver
- Optional Pyroscope 2.0.0 integration for continuous CPU/heap profiling with trace correlation
- Overhead: 5–15% (depends on sampling rate and profiler backpressure)
- Config: `[telemetry] enabled=true, backend="otlp", otlp_endpoint="...", service_name="zeph-agent", sample_rate=1.0, pyroscope_endpoint="..."`
- Auth headers from vault: `ZEPH_OTEL_HEADERS` (never in config files)

### 3.2 Subscriber Layer Stack

Existing `init_tracing()` in `src/tracing_init.rs` builds a registry with:

1. **stderr fmt layer** (existing) — human-readable log output to terminal
2. **file fmt layer** (existing) — structured JSON to `.local/zeph.log`
3. **profiling layer** (NEW, feature-gated) — chrome or OTLP export
4. **alloc layer** (NEW, feature-gated profiling-alloc) — per-span allocation tracking
5. **system metrics task** (NEW, feature-gated profiling) — periodic sysinfo emission

All new layers are **additive** to existing behavior. When `telemetry.enabled=false` or profiling feature is disabled, no layers activate.

### 3.3 Feature Flag Hierarchy

```
profiling                     (base: tracing-chrome, #[instrument], system_metrics)
  +-- profiling-alloc        (allocation tracking via tracking-allocator global allocator)
  +-- profiling-pyroscope    (continuous CPU + heap profiling via Pyroscope SDK)
```

Existing `otel` flag remains independent (controls OTLP export infrastructure).

**Flag combinations and overhead:**

- **(none)**: Zero instrumentation overhead, #[instrument] macros are no-ops
- **profiling**: Chrome traces in `.local/traces/`, #[instrument] active, local file output (3–8% overhead)
- **profiling + otel**: OTLP export instead of chrome traces (5–12% overhead)
- **profiling + profiling-alloc**: + per-span allocation counters as attributes
- **profiling + profiling-pyroscope + otel**: + continuous CPU/heap profiling with trace correlation (10–15% overhead)

**Conditional instrument macro pattern** (applied per crate):

```rust
#[cfg(feature = "profiling")]
macro_rules! instrument_fn {
    ($($tt:tt)*) => { #[tracing::instrument($($tt)*)] };
}
#[cfg(not(feature = "profiling"))]
macro_rules! instrument_fn {
    ($($tt:tt)*) => {};
}
```

When `profiling` feature is disabled, all instrumentation compiles away. Zero code paths added to the binary.

### 3.4 Instrumented Channel Wrapper (`InstrumentedChannel<T>`)

Transparent wrapper around tokio channels with optional metric collection.

**Metrics tracked:**

- `channel.sent` — counter of messages sent
- `channel.received` — counter of messages received
- `channel.queue_depth` — gauge of pending messages (sampled every 16th send)
- `channel.backpressure_latency_us` — histogram of send wait times
- `channel.dropped` — counter of messages dropped (if bounded and full)

**Applied to 6 primary channel sites:**

1. `status_tx` — agent status updates
2. `skill_reload_rx` — skill hot-reload signals
3. `config_reload_rx` — config change notifications
4. `elicitation_tx` — MCP elicitation messages
5. `metrics_rx` — metrics collector input
6. `cancel_signal` — graceful shutdown signaling

**Zero-cost when profiling disabled**: InstrumentedChannel delegates to the underlying channel type with no overhead.

### 3.5 Allocation Tracking Layer

Feature: `profiling-alloc`

Uses `tracking-allocator` 0.4.0 as a global allocator wrapper.

**How it works:**

1. On span enter (via custom Layer's `on_enter`): push a thread-local allocation counter frame
2. During span lifetime: intercept all `alloc()` and `dealloc()` calls in that thread, counting bytes and ops
3. On span exit: pop counter frame, write allocations as span attributes:
   - `alloc.count` — number of allocations
   - `alloc.bytes` — total bytes allocated
   - `dealloc.count` — number of deallocations
   - `dealloc.bytes` — total bytes deallocated
   - `alloc.net_bytes` — net bytes (alloc.bytes - dealloc.bytes)

**Caveats:**

- Multi-threaded tokio runtime may miss cross-thread allocations. Allocation count reflects only the thread that entered the span.
- For accurate per-span allocation attribution, run with `--current-thread` executor.
- Thread-local frame stack prevents race conditions.

**Safety:** `unsafe_code` is permitted only in a single `#[allow(unsafe_code)]` block in `main.rs` for the global allocator initialization.

### 3.6 System Metrics Emission

Feature: `profiling` (enabled whenever profiling is active)

Uses `sysinfo` 0.38.4. Periodic task spawned at startup:

- Samples every 5 seconds (configurable via `[telemetry] system_metrics_interval_secs`)
- Emits tracing event at INFO level with fields: `rss_bytes`, `cpu_percent`, `thread_count`, `fd_count`
- Appears as instant markers in chrome trace timeline
- Negligible overhead (< 1% on most systems)

---

## 4. Key Invariants

### Always

- Feature-gated instrumentation MUST compile to zero overhead when disabled. No `if` branches left in the binary.
- `#[instrument]` macros MUST use the conditional `instrument_fn!` pattern per crate, not bare `#[instrument]`.
- Vault secrets (ZEPH_OTEL_HEADERS) MUST NOT appear in config files. Auth headers resolved at runtime from vault only.
- When telemetry.enabled=false, no subscriber layers activate and no profiling overhead is incurred.
- Profiling span attributes MUST NOT include sensitive data (PII, keys, credentials). Use `skip` parameter on fields as needed.

### Ask First

- Adding instrumentation to hot path functions (multiple calls per turn) — verify overhead is acceptable
- Changing OTLP endpoint or sampling rate — coordinate with observability team / infrastructure
- Adding new feature flags beyond `profiling-alloc` and `profiling-pyroscope` — must justify against feature flag decision rule (sect 029)

### Never

- Use manual `Instant::now()` timing instead of span duration (when profiling active, MetricsBridge replaces manual timings)
- Hard-code OTLP endpoints in code; always source from config or vault
- Link tracing layers in the unconditional build path when profiling feature is disabled
- Store raw vault secrets in span attributes or logs

---

## 5. TelemetryConfig Schema

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TelemetryConfig {
    /// Enable profiling and tracing instrumentation
    pub enabled: bool,

    /// Backend: "local" (chrome JSON) or "otlp" (OpenTelemetry Protocol)
    pub backend: TelemetryBackend,

    /// Directory for local chrome trace files (used when backend="local")
    pub trace_dir: PathBuf,

    /// Include function arguments in span attributes (for debugging)
    pub include_args: bool,

    /// OTLP receiver endpoint URL (used when backend="otlp")
    pub otlp_endpoint: Option<String>,

    /// Vault key containing OTLP authentication headers (JSON object)
    pub otlp_headers_vault_key: Option<String>,

    /// Service name for trace spans (e.g., "zeph-agent")
    pub service_name: String,

    /// Trace sampling rate (0.0–1.0)
    pub sample_rate: f64,

    /// Pyroscope profiler endpoint URL (optional, requires profiling-pyroscope feature)
    pub pyroscope_endpoint: Option<String>,

    /// Interval (seconds) for system metrics emission
    pub system_metrics_interval_secs: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TelemetryBackend {
    Local,
    Otlp,
}
```

**Config migration:**

Old `[observability]` and `[debug.traces]` sections are migrated to `[telemetry]` with deprecation warnings. Backward compatibility provided for one release cycle.

---

## 6. Instrumentation Map

### 6.1 Agent Loop (zeph-core/src/agent/)

**Turn phase spans:**

- `agent.turn` — outer envelope for entire turn
  - `agent.prepare_context` — context assembly, truncation, cost estimation
  - `agent.process_response` — response parsing, tool extraction
  - `agent.security_prescreen` — input validation, injection checks
  - `agent.tool_loop` — tool planning and execution loop
    - `agent.tool_exec` — single tool invocation
    - `agent.tool_batch` — batch execution of multiple tools
  - `agent.learning` — skill ranking and self-learning feedback
  - `agent.code_index` — code repository indexing
  - `agent.mcp_dispatch` — MCP tool dispatching
  - `agent.plan_execute` — orchestration plan execution

**Context assembly spans:**

- `context.assembly` — full context building
  - `context.fetch_recall` — vector/BFS memory recall
  - `context.fetch_summaries` — compressed memory retrieval
  - `context.fetch_code` — code index retrieval
  - `context.fetch_cross_session` — cross-session context
  - `context.fetch_graph_facts` — entity graph facts
  - `context.compaction` — context compression
    - `context.check_summarization` — summarization eligibility
    - `context.microcompact` — in-turn compression

**Persistence spans:**

- `agent.persist_message` — message storage
- `agent.restore_history` — history retrieval

### 6.2 LLM Providers (zeph-llm/src/)

**Per-provider spans:**

- `llm.chat` (claude, openai, ollama, gemini, compatible) — single-turn completion
- `llm.chat_stream` — streaming completion
- `llm.embed` — embedding generation
- `llm.extract` — structured extraction
- `llm.http_request` — HTTP request (underlying call)
- `llm.retry` — retry logic with backoff
- `llm.router` — provider selection and routing

### 6.3 Memory (zeph-memory/src/)

- `memory.remember` — store operation
- `memory.recall` — vector/BFS retrieval
- `memory.summarize` — summarization
- `memory.graph_extract` — entity/relation extraction
- `memory.persona_extract` — persona fact extraction
- `memory.cross_session` — cross-session consolidation
- `memory.vector_store` — Qdrant operations
- `memory.sqlite` — SQLite operations
- `memory.admission` — admission control check
- `memory.compaction_probe` — compaction eligibility probe
- `memory.forgetting` — temporal decay and eviction
- `memory.consolidation` — multi-turn consolidation
- `memory.consolidation_loop` — consolidation loop iteration

### 6.4 Tools (zeph-tools/src/)

- `tool.execute` — outer tool execution wrapper
  - `tool.execute_call` — single tool call
  - `tool.shell` — shell command execution
  - `tool.web_scrape` — web scraping
  - `tool.file` — file system operations
  - `tool.search_code` — code search
- `tool.audit_log` — audit logging
- `tool.cache` — result caching

### 6.5 Skills (zeph-skills/src/)

- `skill.registry_load` — load skill registry
- `skill.match` — skill matching (hybrid or pure embedding)
- `skill.matcher_sync` — matcher synchronization
- `skill.hot_reload` — hot-reload cycle
- `skill.generate` — skill generation
- `skill.evolution` — skill evolution via feedback
- `skill.bm25_search` — BM25 matching
- `skill.qdrant_match` — embedding-based matching

### 6.6 MCP Client (zeph-mcp/src/)

- `mcp.connect` — server connection
- `mcp.connect_url` — connect to specific URL
- `mcp.list_tools` — tool discovery
- `mcp.call_tool` — tool invocation
- `mcp.shutdown` — graceful shutdown
- `mcp.tool_refresh` — periodic tool refresh

### 6.7 Channels (zeph-channels/src/)

- `channel.cli` — CLI channel I/O
- `channel.telegram` — Telegram adapter
- `channel.discord` — Discord adapter (feature-gated)
- `channel.slack` — Slack adapter (feature-gated)

### 6.8 InstrumentedChannel Usage

Applied to:

1. `status_tx` — spans: `channel.status_send`, `channel.status_recv`
2. `skill_reload_rx` — spans: `channel.skill_reload_send`, `channel.skill_reload_recv`
3. `config_reload_rx` — spans: `channel.config_reload_send`, `channel.config_reload_recv`
4. `elicitation_tx` — spans: `channel.elicitation_send`, `channel.elicitation_recv`
5. `metrics_rx` — spans: `channel.metrics_send`, `channel.metrics_recv`
6. `cancel_signal` — spans: `channel.cancel_send`, `channel.cancel_recv`

---

## 7. MetricsBridge Integration (Phase 2)

*Described here for context; detailed in Phase 2 spec.*

When profiling is active, a `MetricsBridge` layer derives timing from span durations, replacing manual `Instant::now()` bookkeeping.

- **When profiling disabled**: existing `Instant::now()` / `elapsed()` timing remains
- **When profiling enabled**: span duration (timestamp exit - timestamp enter) updates MetricsSnapshot fields
- **Validation phase** (Phase 2): verify span-derived timings match manual measurements within 5% before full rollout

---

## 8. Phase 1: Foundation (1–2 PRs)

**Deliverables:**

1. TelemetryConfig struct and config loading
2. `profiling` feature flag + conditional `instrument_fn!` macro
3. tracing-chrome initialization in init_tracing()
4. 4 agent turn phase instruments: `agent.turn`, `agent.prepare_context`, `agent.process_response`, `agent.security_prescreen`
5. Top-level LLM provider instruments: `llm.chat` (per provider)
6. Config migration from old sections with deprecation warnings
7. `.local/traces/` directory initialization (gitignore already in place)

**Test coverage:**

- Unit test: tracing layer initialization succeeds with profiling flag enabled/disabled
- Integration test: write chrome JSON and verify W3C format validity
- Config test: old config sections migrate cleanly to new [telemetry]

---

## 9. Phase 2: Deep Instrumentation (2–3 PRs)

**Deliverables:**

1. All subsystem spans from instrumentation map (sections 6.1–6.7)
2. `InstrumentedChannel<T>` wrappers for 6 primary channels
3. MetricsBridge layer (validation: span durations vs manual timing)

**Test coverage:**

- Subsystem integration tests verify all spans emit in expected order
- Stress test: verify InstrumentedChannel overhead  < 1% on high-message-rate scenarios

---

## 10. Phase 3: Allocation Tracking + System Metrics (1–2 PRs)

**Deliverables:**

1. `profiling-alloc` feature flag
2. AllocLayer custom Layer + tracking-allocator global allocator
3. sysinfo periodic task (5s interval) emitting RSS, CPU%, thread count, fd count
4. Span attributes: `alloc.count`, `alloc.bytes`, `alloc.net_bytes`, `dealloc.count`, `dealloc.bytes`

**Test coverage:**

- Unit test: AllocLayer on_enter/on_exit correctly push/pop counter frames
- Integration test: allocation attributes appear in exported spans
- Benchmark: verify allocation tracking overhead < 2% on single-threaded executor

---

## 11. Phase 4: Production Tier + Pyroscope (1 PR)

**Deliverables:**

1. OTLP export path using opentelemetry-otlp 0.31
2. `profiling-pyroscope` feature flag + Pyroscope 2.0.0 + pyroscope_pprofrs 0.2.10 integration
3. Trace correlation via `pyroscope.pprof.profile_id` span attribute
4. Vault auth header resolution (ZEPH_OTEL_HEADERS)
5. docker-compose.profiling.yml for local Grafana Tempo + Pyroscope stack
6. Grafana dashboard template

**Test coverage:**

- Integration test: export to mock OTLP receiver
- Live test: export to real Tempo instance (optional, documented)
- Profiler correlation: verify profile_id appears in traces

---

## 12. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| `telemetry.enabled=false` | No profiling overhead, all layers skipped |
| Chrome trace file write fails (disk full) | Error logged, tracing continues (non-fatal) |
| OTLP endpoint unreachable | Batched export retries with exponential backoff; after N retries, drops batch silently |
| Vault key ZEPH_OTEL_HEADERS missing | OTLP export fails with clear error; fallback to no-auth if headers optional |
| Span duration is negative (clock skew) | Log warning, emit duration as 0 |
| Thread local allocation frame stack underflow | Panic with clear error (indicates layer misuse) |
| Pyroscope endpoint down | Profiler logs error, continues (non-fatal) |
| High-frequency system_metrics task (interval < 1s) | Capped at 1s minimum to prevent overhead |

---

## 13. Success Criteria

Profiling and tracing system is complete when:

- [ ] Phase 1 foundation merged: TelemetryConfig, profiling feature, chrome layer, 4 agent span instruments, LLM instruments
- [ ] Phase 2 deep instrumentation merged: all subsystem spans, InstrumentedChannel wrappers, MetricsBridge validation
- [ ] Phase 3 allocation + metrics merged: profiling-alloc feature, AllocLayer, sysinfo task
- [ ] Phase 4 production tier merged: OTLP export, profiling-pyroscope, Grafana stack
- [ ] Chrome traces export valid W3C format (verified with Perfetto UI)
- [ ] Span durations in exported traces match manual timing within ±5%
- [ ] Overhead is within spec: local 3–8%, OTLP 5–12%, OTLP+profiler 10–15%
- [ ] Feature flag compile-time elimination verified: `profiling` disabled = zero instrumentation in binary
- [ ] Live testing playbook created and tested: `.local/testing/playbooks/profiling.md`
- [ ] Coverage status updated: `.local/testing/coverage-status.md`

---

## 14. See Also

- [[MOC-specs]] — Map of all specifications
- [[constitution]] — Project-wide principles
- [[001-system-invariants/spec]] — Cross-cutting architectural contracts
- [[011-tui/spec]] — TUI dashboard (existing metrics display)
- [[027-runtime-layer/spec]] — Runtime layer hooks
- [[029-feature-flags/spec]] — Feature flag decision rules
