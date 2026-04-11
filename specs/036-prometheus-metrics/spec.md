---
aliases:
  - Prometheus Metrics Export
  - Observability Metrics
tags:
  - sdd
  - spec
  - infra
  - observability
  - cross-cutting
created: 2026-04-10
status: approved
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[001-system-invariants/spec]]"
  - "[[035-profiling/spec]]"
---

# Spec: Prometheus Metrics Export

> [!info]
> Aggregated time-series metrics export via Prometheus-compatible `/metrics` endpoint, complementary to both the in-process TUI metrics snapshot and distributed tracing (profiling spec 035). Enables external alerting, Grafana dashboards, and SLO tracking in server deployments. Feature-gated, MVP scope: ~25 gauge/counter metrics derived from MetricsSnapshot. Phase 1 foundation for Phase 2 Grafana stack and Phase 3 histograms.

## Sources

### External
- [prometheus-client 0.23](https://docs.rs/prometheus-client/latest/prometheus_client/) — OpenMetrics-native, no global state, official Prometheus project
- [Prometheus Exposition Format](https://github.com/prometheus/docs/blob/main/content/docs/instrumenting/exposition_formats.md) and [OpenMetrics Format](https://openmetrics.io/)
- [axum 0.8](https://docs.rs/axum/latest/axum/) — HTTP routing (already used by `zeph-gateway`)

### Internal
| File | Contents |
|---|---|
| `crates/zeph-config/src/root.rs` | Configuration loading for feature-gated sections |
| `crates/zeph-core/src/metrics.rs` | Existing `MetricsSnapshot` watch channel |
| `crates/zeph-gateway/src/router.rs` | HTTP route handling |
| `crates/zeph-gateway/src/server.rs` | GatewayServer builder interface |
| `src/main.rs` | Binary crate bootstrap and wiring |
| `src/config.rs` | Config initialization |
| `.local/testing/coverage-status.md` | Feature tracking |

---

## 1. Overview

### Problem Statement

Zeph currently provides observability via two complementary but independent systems:

1. **TUI metrics snapshot** (`MetricsSnapshot` in zeph-core/src/metrics.rs) — in-process, updated every agent turn, visible only in TUI dashboard, no persistence
2. **Distributed tracing** (spec 035 profiling) — per-request spans exported to Tempo, detailed but sampled

However, external monitoring (Prometheus scraping, alerting, Grafana dashboards) requires **always-on aggregated metrics** (counters, gauges) that:
- Survive process restarts (via remote scraping, not persistence)
- Enable time-series alerting (e.g., "alert if error rate > 5%")
- Correlate with Grafana dashboards (Grafana consumes both Prometheus metrics AND Tempo traces)
- Follow Prometheus conventions (standard scrape protocol, `/metrics` endpoint)

The gap: Zeph has metrics data (in `MetricsSnapshot`) but no external export mechanism.

### Goal

Expose a Prometheus-compatible `/metrics` endpoint on the gateway HTTP server that:

1. Exports ~25 metric families (gauges + counters) derived from `MetricsSnapshot`
2. Uses `prometheus-client` 0.23 (OpenMetrics-native, no global state)
3. Syncs `MetricsSnapshot` values to Prometheus metrics on a configurable interval
4. Requires zero infrastructure for MVP (local metrics scraping)
5. Enables Phase 2 (Grafana dashboards) and Phase 3 (histogram recording)

### Out of Scope

- Real-time TUI metric updates from Prometheus (TUI remains fed by `MetricsSnapshot` watch channel; Prometheus reads FROM `MetricsSnapshot`, not the other way)
- Histogram metrics (Phase 3 — requires per-event recording points)
- Grafana dashboard provisioning (Phase 2)
- Custom metric facades or indirection layers (use `prometheus-client` directly)
- Metrics authentication (scraping happens behind firewall; no per-endpoint auth)

---

## 2. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN `[metrics] enabled=true` and gateway is enabled THE SYSTEM SHALL expose a `/metrics` endpoint returning OpenMetrics 1.0.0 text format | must |
| FR-002 | WHEN prometheus feature flag is disabled THE SYSTEM SHALL not compile the `/metrics` handler or prometheus-client dependency | must |
| FR-003 | WHEN `[metrics] enabled=true` THE SYSTEM SHALL synchronize `MetricsSnapshot` values to Prometheus gauges/counters every `sync_interval_secs` seconds | must |
| FR-004 | WHEN a counter metric value decreases (session restart detected) THE SYSTEM SHALL treat the new value as absolute (reset detection) | must |
| FR-005 | WHEN `[metrics]` section is absent from config THE SYSTEM SHALL use documented defaults: `enabled=false`, `path="/metrics"`, `sync_interval_secs=5` | must |
| FR-006 | WHEN `GET /metrics` is requested THE SYSTEM SHALL return HTTP 200 with Content-Type `application/openmetrics-text; version=1.0.0; charset=utf-8` | must |
| FR-007 | WHEN `[metrics] enabled=true` but `[gateway] enabled=false` THE SYSTEM SHALL warn and skip metrics export (graceful degradation) | should |
| FR-008 | WHEN config is migrated via `--migrate-config` THE SYSTEM SHALL insert a `[metrics]` section with defaults as comments | should |
| FR-009 | WHEN `--init` wizard runs and gateway is enabled THE SYSTEM SHALL prompt user to enable Prometheus metrics | should |
| FR-010 | WHEN metrics export encounters an error (registry encode fail, watch channel close) THE SYSTEM SHALL log error and continue (non-fatal) | must |

---

## 3. Architecture

### 3.1 Data Flow

```
zeph-core:
  MetricsSnapshot (updated every turn)
      |
      v
  tokio::sync::watch channel
      |
      +-- TUI (existing) — MetricsSnapshot watch receiver → TUI refresh
      |
      +-- Prometheus sync task (NEW) — watch receiver → period read → PrometheusMetrics::sync()
          |
          v
      PrometheusMetrics registry (prometheus_client::registry::Registry)
          |
          v
      zeph-gateway
          |
          v
      GET /metrics endpoint
          |
          v
      Prometheus scraper (or human)
```

### 3.2 Component Roles

**`PrometheusMetrics` struct (binary crate, new file `src/metrics_export.rs`)**

- Owns an `Arc<prometheus_client::registry::Registry>`
- Creates ~25 concrete metric instances (counter, gauge objects) at startup
- Provides `sync(&snapshot, &prev_snapshot)` method to update gauges and compute counter deltas

**`spawn_metrics_sync` task (binary crate)**

- Spawned in main loop after gateway initialization
- Reads from `MetricsSnapshot` watch channel every `sync_interval_secs` seconds
- Calls `PrometheusMetrics::sync()` to update all metrics
- Tracked in background task supervisor

**Gateway integration**

- `GatewayServer::with_metrics_registry()` builder method (feature-gated `prometheus`)
- Adds router to `/metrics` path (configurable via `[metrics] path`)
- Handler encodes registry to OpenMetrics text and returns

**Config struct (`crates/zeph-config/src/metrics.rs`)**

- `MetricsConfig` with fields: `enabled`, `path`, `sync_interval_secs`
- Integrated into `Config` root struct via `pub metrics: MetricsConfig`

### 3.3 Metric Families (~25 total, all with `zeph_` prefix)

**LLM Provider Metrics (highest value)**

| Metric | Type | Labels | Source |
|--------|------|--------|--------|
| `zeph_llm_tokens_total` | Counter | `direction={prompt,completion,cache_read,cache_create}` | `prompt_tokens`, `completion_tokens`, `cache_read_tokens`, `cache_creation_tokens` |
| `zeph_llm_api_calls_total` | Counter | `provider`, `model` | `api_calls` |
| `zeph_llm_cost_cents_total` | Counter | `provider` | `cost_spent_cents`, `provider_cost_breakdown` |
| `zeph_llm_latency_ms` | Gauge | — | `last_llm_latency_ms` |
| `zeph_llm_context_tokens` | Gauge | — | `context_tokens` |

**Agent Turn Metrics**

| Metric | Type | Labels | Source |
|--------|------|--------|--------|
| `zeph_turn_phase_duration_ms` | Gauge | `phase={prepare_context,llm_chat,tool_exec,persist}` | `last_turn_timings.*` |
| `zeph_turn_phase_avg_ms` | Gauge | `phase` | `avg_turn_timings.*` |
| `zeph_turn_phase_max_ms` | Gauge | `phase` | `max_turn_timings.*` |

**Memory Metrics**

| Metric | Type | Labels | Source |
|--------|------|--------|--------|
| `zeph_memory_messages_total` | Gauge | — | `sqlite_message_count` |
| `zeph_memory_embeddings_total` | Counter | — | `embeddings_generated` |
| `zeph_memory_summaries_total` | Counter | — | `summaries_count` |
| `zeph_memory_compactions_total` | Counter | `tier={soft,hard}` | `context_compactions`, `compaction_hard_count` |
| `zeph_memory_qdrant_available` | Gauge | — | `qdrant_available` (1/0 as f64) |

**Tool Metrics**

| Metric | Type | Labels | Source |
|--------|------|--------|--------|
| `zeph_tool_cache_total` | Counter | `result={hit,miss}` | `tool_cache_hits`, `tool_cache_misses` |
| `zeph_tool_output_prunes_total` | Counter | — | `tool_output_prunes` |

**Security Metrics**

| Metric | Type | Labels | Source |
|--------|------|--------|--------|
| `zeph_security_injection_flags_total` | Counter | — | `sanitizer_injection_flags` |
| `zeph_security_exfiltration_blocks_total` | Counter | — | `exfiltration_images_blocked` |
| `zeph_security_quarantine_total` | Counter | `result={invoked,failed}` | `quarantine_invocations`, `quarantine_failures` |
| `zeph_security_rate_limit_trips_total` | Counter | — | `rate_limit_trips` |

**Orchestration Metrics**

| Metric | Type | Labels | Source |
|--------|------|--------|--------|
| `zeph_orchestration_plans_total` | Counter | — | `orchestration.plans_total` |
| `zeph_orchestration_tasks_total` | Counter | `status={completed,failed,skipped}` | `orchestration.tasks_*` |

**System Metrics**

| Metric | Type | Labels | Source |
|--------|------|--------|--------|
| `zeph_uptime_seconds` | Gauge | — | `uptime_seconds` |
| `zeph_skills_total` | Gauge | — | `total_skills` |
| `zeph_mcp_servers` | Gauge | `status={connected,failed}` | `mcp_connected_count`, `mcp_server_count` |
| `zeph_background_tasks` | Gauge | `state={inflight,dropped,completed}` | `bg_inflight`, `bg_dropped`, `bg_completed` |

**Total: 25 metrics.** Conservative MVP scope per pre-v1.0.0 principle. More can be added post-v1.0.0.

### 3.4 Counter Delta Computation

Counters must compute deltas from snapshot to snapshot to avoid double-counting:

```
previous_snapshot.field = 100
current_snapshot.field = 105
delta = 105 - 100 = 5
prometheus_counter.inc_by(5)
```

Reset detection: if `current < previous`, treat `current` as absolute (session restart):

```
previous_snapshot.field = 100
current_snapshot.field = 20  (< 100 → session restart)
delta = 20 (absolute)
prometheus_counter.inc_by(20)
```

### 3.5 Feature Flag Design

```toml
# In root Cargo.toml [features]
prometheus = ["gateway", "dep:prometheus-client", "zeph-gateway/prometheus"]

# In zeph-gateway Cargo.toml [features]
prometheus = ["dep:prometheus-client"]

# Add prometheus to server bundle (alongside gateway, a2a, otel)
server = ["gateway", "a2a", "otel", "prometheus"]
```

When `prometheus` feature is disabled:
- `prometheus-client` is not linked
- `/metrics` route not compiled
- Zero runtime overhead

---

## 4. Key Invariants

### Always

- All Prometheus metrics MUST use the `zeph_` namespace prefix
- Metric names MUST follow Prometheus naming conventions: lowercase, underscores, no dashes
- Counter deltas MUST be computed from previous snapshot to current (no double-counting)
- Session restart detection: if counter value decreases, treat new value as absolute
- Sync task MUST be non-blocking; errors logged but not fatal
- Registry MUST be `Arc<prometheus_client::registry::Registry>` passed explicitly, never global state
- `/metrics` endpoint is UNAUTHENTICATED (standard Prometheus scraping pattern, operates behind firewall)
- When `metrics.enabled=true` but `gateway.enabled=false`, a warning MUST be logged and metrics export skipped gracefully

### Ask First

- Adding new metric families beyond the initial ~25 (justifies MVP scope and naming)
- Changing metric names or label names (impacts external dashboards and alerting rules)
- Integrating metrics with new subsystems (may need new fields in `MetricsSnapshot`)

### Never

- Use hardcoded metric names or values (all metric definitions in `PrometheusMetrics` struct)
- Export PII or secrets in metric labels or values (metrics MUST be redactable)
- Implement global static registry or thread-local state for metrics (use `Arc<Registry>` pattern)
- Add authentication to `/metrics` endpoint (follows Prometheus conventions)

---

## 5. Configuration Schema

### `MetricsConfig` Struct

```rust
// crates/zeph-config/src/metrics.rs (new file)

/// Prometheus metrics export configuration.
///
/// When `enabled = true`, the gateway HTTP server exposes a `/metrics` endpoint
/// in OpenMetrics 1.0.0 text format. Metric values are synchronized from the internal
/// `MetricsSnapshot` every `sync_interval_secs` seconds.
///
/// # Example (TOML)
///
/// ```toml
/// [metrics]
/// enabled = true
/// path = "/metrics"
/// sync_interval_secs = 5
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct MetricsConfig {
    /// Enable Prometheus metrics endpoint. Requires `gateway` to be enabled.
    /// Default: `false`.
    #[serde(default)]
    pub enabled: bool,

    /// HTTP path for the metrics scrape endpoint.
    /// Default: `"/metrics"`.
    #[serde(default = "default_metrics_path")]
    pub path: String,

    /// Interval in seconds between MetricsSnapshot-to-Prometheus synchronization passes.
    /// Lower values (< 5) increase CPU slightly but reduce gauge staleness.
    /// Higher values (> 10) reduce CPU but gauges become stale faster.
    /// Default: `5`.
    #[serde(default = "default_sync_interval")]
    pub sync_interval_secs: u64,
}

const fn default_metrics_path() -> String {
    String::from("/metrics")
}

const fn default_sync_interval() -> u64 {
    5
}
```

### Integration into `Config` root struct

Add to `crates/zeph-config/src/root.rs`:

```rust
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct Config {
    // ... existing fields ...
    #[serde(default)]
    pub metrics: MetricsConfig,
    // ...
}
```

### TOML Example

```toml
# Enable Prometheus metrics export on the gateway HTTP server.
[metrics]
enabled = true
path = "/metrics"
sync_interval_secs = 5
```

---

## 6. Implementation Components

### 6.1 `PrometheusMetrics` struct (new file: `src/metrics_export.rs`)

```rust
use prometheus_client::registry::Registry;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::gauge::Gauge;
use std::sync::Arc;
use zeph_core::metrics::MetricsSnapshot;

/// Prometheus metric descriptors and registry.
///
/// All metric families are registered at startup.
/// Sync updates gauges and counters from the latest MetricsSnapshot.
pub struct PrometheusMetrics {
    pub registry: Arc<Registry>,
    
    // LLM metrics
    llm_tokens_prompt: Counter,
    llm_tokens_completion: Counter,
    llm_tokens_cache_read: Counter,
    llm_tokens_cache_create: Counter,
    llm_api_calls_total: Counter,
    llm_cost_cents_total: Counter,
    llm_latency_ms: Gauge,
    llm_context_tokens: Gauge,
    
    // Agent turn metrics
    turn_phase_duration_ms: Gauge,  // labeled by phase
    turn_phase_avg_ms: Gauge,       // labeled by phase
    turn_phase_max_ms: Gauge,       // labeled by phase
    
    // Memory metrics
    memory_messages_total: Gauge,
    memory_embeddings_total: Counter,
    memory_summaries_total: Counter,
    memory_compactions_total: Counter,
    memory_qdrant_available: Gauge,
    
    // Tool metrics
    tool_cache_hits: Counter,
    tool_cache_misses: Counter,
    tool_output_prunes_total: Counter,
    
    // Security metrics
    security_injection_flags_total: Counter,
    security_exfiltration_blocks_total: Counter,
    security_quarantine_invocations: Counter,
    security_quarantine_failures: Counter,
    security_rate_limit_trips_total: Counter,
    
    // Orchestration metrics
    orchestration_plans_total: Counter,
    orchestration_tasks_completed: Counter,
    orchestration_tasks_failed: Counter,
    orchestration_tasks_skipped: Counter,
    
    // System metrics
    uptime_seconds: Gauge,
    skills_total: Gauge,
    mcp_servers_connected: Gauge,
    mcp_servers_failed: Gauge,
    background_tasks_inflight: Gauge,
    background_tasks_dropped: Gauge,
    background_tasks_completed: Gauge,
}

impl PrometheusMetrics {
    /// Create a new registry with all Zeph metric families registered.
    pub fn new() -> Self {
        let registry = Arc::new(Registry::default());
        // Register all counter and gauge families...
        Self { registry, /* ... */ }
    }

    /// Update all gauges and counters from the latest MetricsSnapshot.
    ///
    /// Counter updates compute deltas from the previous snapshot to avoid double-counting.
    /// Session restart detection: if a counter value decreases, treat it as absolute.
    pub fn sync(&self, snapshot: &MetricsSnapshot, prev: &MetricsSnapshot) {
        // Update gauges directly (e.g., llm_latency_ms.set(snapshot.last_llm_latency_ms as f64))
        // Update counters via delta:
        //   let delta = match snapshot.prompt_tokens < prev.prompt_tokens {
        //     true => snapshot.prompt_tokens,  // reset detected
        //     false => snapshot.prompt_tokens - prev.prompt_tokens,
        //   };
        //   self.llm_tokens_prompt.inc_by(delta);
    }
}
```

### 6.2 Metrics sync task (`spawn_metrics_sync`)

```rust
/// Spawn a background task that syncs MetricsSnapshot into Prometheus gauges.
///
/// Runs every `interval_secs` seconds. Non-blocking; errors are logged.
pub fn spawn_metrics_sync(
    metrics: Arc<PrometheusMetrics>,
    mut snapshot_rx: tokio::sync::watch::Receiver<MetricsSnapshot>,
    interval_secs: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        let mut prev_snapshot = *snapshot_rx.borrow();

        loop {
            interval.tick().await;

            if let Ok(snapshot) = snapshot_rx.try_borrow() {
                metrics.sync(&snapshot, &prev_snapshot);
                prev_snapshot = *snapshot;
            }
            // On channel close, exit gracefully
        }
    })
}
```

### 6.3 Gateway integration (`crates/zeph-gateway/src/router.rs` and `server.rs`)

```rust
// In router.rs (feature-gated)
#[cfg(feature = "prometheus")]
async fn metrics_handler(
    State(registry): State<Arc<prometheus_client::registry::Registry>>,
) -> impl IntoResponse {
    use prometheus_client::encoding::text::encode;
    
    let mut buf = String::new();
    match encode(&mut buf, &registry) {
        Ok(_) => (
            [
                (
                    axum::http::header::CONTENT_TYPE,
                    "application/openmetrics-text; version=1.0.0; charset=utf-8",
                )
            ],
            buf,
        )
            .into_response(),
        Err(e) => {
            tracing::error!("failed to encode prometheus metrics: {}", e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "metrics encoding failed",
            )
                .into_response()
        }
    }
}

// In server.rs
#[cfg(feature = "prometheus")]
impl GatewayServer {
    /// Attach a Prometheus metrics registry.
    /// When set, `GET /metrics` returns OpenMetrics 1.0.0 text format.
    pub fn with_metrics_registry(
        mut self,
        registry: Arc<prometheus_client::registry::Registry>,
        path: String,
    ) -> Self {
        self.router = self.router.route(
            &path,
            axum::routing::get(metrics_handler).with_state(registry),
        );
        self
    }
}
```

### 6.4 Binary crate wiring (in `src/main.rs` or bootstrap)

```rust
#[cfg(feature = "prometheus")]
if config.metrics.enabled && config.gateway.enabled {
    let prom = Arc::new(crate::metrics_export::PrometheusMetrics::new());
    let sync_handle = crate::metrics_export::spawn_metrics_sync(
        Arc::clone(&prom),
        metrics_snapshot_rx.clone(),
        config.metrics.sync_interval_secs,
    );
    gateway_server = gateway_server.with_metrics_registry(
        Arc::clone(&prom.registry),
        config.metrics.path.clone(),
    );
    // Track sync_handle in background_supervisor
} else if config.metrics.enabled && !config.gateway.enabled {
    tracing::warn!(
        "[metrics] enabled=true but [gateway] enabled=false; skipping metrics export"
    );
}
```

---

## 7. Phase Breakdown

### Phase 1: Foundation (1 PR)

**Deliverables:**

1. `MetricsConfig` struct in `crates/zeph-config/src/metrics.rs`
2. Add `pub metrics: MetricsConfig` to `Config` root struct
3. `prometheus-client` workspace dependency
4. `prometheus` feature flag (root + zeph-gateway)
5. `PrometheusMetrics` struct with ~25 metric families in `src/metrics_export.rs`
6. `spawn_metrics_sync()` background task
7. `GatewayServer::with_metrics_registry()` builder method
8. `GET /metrics` handler in gateway router (feature-gated)
9. Wiring in binary crate bootstrap
10. Config migration for `[metrics]` section (in ConfigMigrator)
11. Init wizard update to prompt for Prometheus metrics
12. Testing playbook at `.local/testing/playbooks/prometheus.md`
13. Coverage status row in `.local/testing/coverage-status.md` (status: Untested)
14. CHANGELOG.md entry

**Acceptance Criteria:**

- [ ] Spec approved
- [ ] `prometheus` feature can be compiled (`cargo build --features prometheus`)
- [ ] `/metrics` endpoint returns valid OpenMetrics text when enabled
- [ ] Metric values reflect current MetricsSnapshot state
- [ ] Counter deltas computed correctly (no double-counting on resync)
- [ ] Session restart detection works (counter reset on value decrease)
- [ ] Config defaults applied when section absent
- [ ] Config migration inserts `[metrics]` section with comments
- [ ] Init wizard prompts for metrics when gateway enabled
- [ ] All tests pass: `cargo nextest run --features prometheus`
- [ ] Feature-gated compilation verified: `prometheus` off = no endpoint
- [ ] Playbook created and documented in `.local/testing/`
- [ ] Coverage status row created (Untested)

### Phase 2: Grafana Stack (1 PR, post-v1.0.0)

**Deliverables:**

1. `docker-compose.metrics.yml` (Prometheus + Grafana services)
2. Pre-built Grafana dashboard JSON (provisioned via volume mount)
3. Prometheus scrape config targeting `localhost:8090/metrics`
4. mdbook documentation in `docs/src/` for setup and dashboard usage

### Phase 3: Histogram Metrics (1 PR, post-v1.0.0)

**Deliverables:**

1. `zeph_llm_latency_seconds` histogram (replaces latency gauge)
2. `zeph_turn_duration_seconds` histogram
3. `zeph_tool_execution_seconds` histogram
4. Histogram bucket configuration: 100ms, 500ms, 1s, 5s, 10s, 30s, 60s
5. Per-event recording points in agent loop, LLM providers, tool executor
6. Updated tests

---

## 8. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| `metrics.enabled=true` but `gateway.enabled=false` | Log warning, skip metrics export gracefully |
| Watch channel closes (MetricsSnapshot dropped) | Sync task exits cleanly, logs info |
| Metrics registry encode fails | Log error, return HTTP 500 from `/metrics` handler |
| Counter value decreases (session restart) | Treat new value as absolute, no negative delta |
| Gauge is NaN or infinity | Prometheus-client handles via OpenMetrics spec (special encoding) |
| `/metrics` endpoint called during sync | No race condition (Arc<Registry> is thread-safe) |
| Config section `[metrics]` missing | Use hardcoded defaults (enabled=false, path="/metrics", sync_interval_secs=5) |
| `sync_interval_secs` is 0 | Log warning, clamp to 1s minimum |
| Disk full when writing metric data | N/A (metrics are in-memory, no disk I/O) |

---

## 9. Success Criteria

Prometheus metrics feature is complete when:

- [ ] Phase 1 merged: `MetricsConfig`, `prometheus` feature, `PrometheusMetrics` struct, sync task, `/metrics` endpoint
- [ ] `/metrics` endpoint returns valid OpenMetrics 1.0.0 text (verified with `curl http://localhost:8090/metrics`)
- [ ] Metric values match `MetricsSnapshot` fields within the same turn
- [ ] Counter deltas computed correctly (no double-counting after resync)
- [ ] Session restart detection prevents negative deltas
- [ ] Feature flag compile-time elimination verified: `prometheus` disabled = zero endpoint in binary
- [ ] Config migration inserts `[metrics]` section in existing configs
- [ ] Init wizard prompts for metrics when gateway enabled
- [ ] Testing playbook created: `.local/testing/playbooks/prometheus.md`
- [ ] Coverage status updated: `.local/testing/coverage-status.md` shows `Untested` → `Tested` after Phase 1 verification
- [ ] All tests pass: `cargo nextest run --workspace --features full`
- [ ] CHANGELOG.md updated with Phase 1 delivery

---

## 10. See Also

- [[MOC-specs]] — Map of all specifications
- [[constitution]] — Project-wide principles
- [[001-system-invariants/spec]] — Cross-cutting architectural contracts
- [[035-profiling/spec]] — Complementary distributed tracing system
- [[011-tui/spec]] — TUI metrics display (existing MetricsSnapshot consumer)
- [[019-gateway/spec]] — HTTP gateway that hosts `/metrics` endpoint
- [[029-feature-flags/spec]] — Feature flag decision rules
