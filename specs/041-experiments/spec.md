---
aliases:
  - Experiments & Feature Gating
  - Runtime Experiments
  - A/B Testing Framework
tags:
  - sdd
  - spec
  - runtime
  - experiments
  - feature-gating
created: 2026-04-11
status: approved
related:
  - "[[MOC-specs]]"
  - "[[029-feature-flags/spec]]"
  - "[[020-config-loading/spec]]"
---

# Spec: Experiments & Runtime Feature Gating

> [!info]
> Specification for the experiments subsystem. Defines how runtime experiments
> are configured, enabled/disabled, and reported on.

**Crate**: `zeph-experiments` (Layer 2)  
**Status**: Approved (shipped v0.13.0+)

---

## 1. Overview

The experiments system enables **controlled rollout and A/B testing** of new features and hyperparameters
without recompiling the binary. This is distinct from compile-time feature flags ([[029-feature-flags/spec]])
which make trade-offs between binary size and capability.

Runtime experiments allow:
- Enabling/disabling features via config without recompile
- A/B testing parameter values (temperature, top-p, retrieval depth)
- Gradual rollout of new behavior to a percentage of users
- Collecting metrics and feedback before full deployment

---

## 2. ExperimentConfig TOML Section

Experiments are configured in the `[experiments]` section:

```toml
[experiments]
enabled = true

# List of active experiments
[[experiments.active]]
name = "higher_temperature"
description = "Test higher temperature (0.8) for more creative responses"
enabled = true
rollout_percentage = 50  # Apply to 50% of sessions

[[experiments.active]]
name = "deep_retrieval"
description = "Retrieve 10 instead of 5 memory items"
enabled = true
rollout_percentage = 100  # Apply to all sessions

[[experiments.active]]
name = "new_orchestrator"
description = "Test new orchestration strategy"
enabled = false  # Disabled, not active
rollout_percentage = 0
```

### 2.1 ExperimentConfig Fields

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `enabled` | bool | true | Master switch for experiments subsystem |
| `active` | [ExperimentDef] | [] | List of experiment definitions |

### 2.2 ExperimentDef Fields

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `name` | string | ✓ | Unique identifier (kebab-case) |
| `description` | string | ✗ | Human-readable description |
| `enabled` | bool | ✓ | Is this experiment active? |
| `rollout_percentage` | u32 | ✓ | 0–100: % of sessions affected (0 = disabled) |

---

## 3. Accessing Experiments at Runtime

### 3.1 Querying Active Experiments

In agent code:

```rust
use zeph_experiments::ExperimentEngine;

// Check if an experiment is active for this session
if engine.is_active("higher_temperature") {
    config.llm.temperature = 0.8;
} else {
    config.llm.temperature = 0.7;
}

// Get all active experiments
let active = engine.active_experiments();
for exp in active {
    tracing::info!("active experiment: {}", exp.name);
}
```

### 3.2 Rollout Percentage

Rollout is determined by session hash:

```rust
fn should_run_experiment(experiment_name: &str, rollout_pct: u32, session_id: &str) -> bool {
    let hash = blake3::hash(format!("{}:{}", experiment_name, session_id).as_bytes());
    let value = hash.as_bytes()[0] as u32;  // 0–255
    (value * 100) / 256 < rollout_pct
}

// Example: session "abc123", rollout 50%
// Hash → byte 128 (out of 255)
// (128 * 100) / 256 = 50
// 50 < 50? false → not active
//
// Hash → byte 64 (out of 255)
// (64 * 100) / 256 = 25
// 25 < 50? true → active
```

This ensures:
- **Deterministic**: Same session always gets same result
- **Stable**: Moving from 40% → 50% rollout includes all previous sessions
- **Uniform**: Each percentage point equally distributed across sessions

---

## 4. Experiment Results & Reporting

When experiments are enabled, metrics are collected:

```
[experiments]
enabled = true
results_dir = ".zeph/experiment-results"
persist_metrics = true
```

### 4.1 Result Schema

Each experiment produces a `ExperimentResult`:

```rust
pub struct ExperimentResult {
    pub name: String,
    pub session_id: String,
    pub started_at: Instant,
    pub completed_at: Instant,
    pub status: ExperimentStatus,
    pub metrics: ExperimentMetrics,
}

pub enum ExperimentStatus {
    Active,
    Completed,
    Failed,
}

pub struct ExperimentMetrics {
    pub turns_used: u32,
    pub tools_called: u32,
    pub errors: u32,
    pub api_cost_estimate: f64,
    pub duration_secs: f64,
}
```

### 4.2 Experiment Report

Generate reports via CLI:

```bash
# List all experiments and their status
cargo run --features full -- experiment list

# Get metrics for a specific experiment
cargo run --features full -- experiment report higher_temperature

# Compare control vs experiment group
cargo run --features full -- experiment compare higher_temperature
```

---

## 5. Built-in Experiments (Examples)

The crate ships with several predefined experiment templates:

### 5.1 Temperature Sweep

```toml
[[experiments.active]]
name = "temperature_sweep"
description = "Test different temperature values for creativity"
enabled = true
rollout_percentage = 50

[experiments.active.parameters]
temperature = 0.8  # Default is 0.7
```

### 5.2 Retrieval Depth

```toml
[[experiments.active]]
name = "deep_memory_retrieval"
description = "Retrieve 10 memory items instead of 5"
enabled = true
rollout_percentage = 25

[experiments.active.parameters]
memory_retrieval_depth = 10
```

### 5.3 New Orchestrator

```toml
[[experiments.active]]
name = "cascade_routing_v2"
description = "Test new cascade routing strategy (Phase 2)"
enabled = false
rollout_percentage = 0

[experiments.active.parameters]
orchestration_strategy = "cascade_v2"
```

---

## 6. Integration with Agent Loop

### 6.1 Startup Initialization

```rust
// In zeph-core main initialization
let experiments = ExperimentEngine::load_config(
    &config.experiments,
    &session_id,
)?;

agent_context.experiments = experiments;
```

### 6.2 During Turns

```rust
// In agent loop, before LLM call
let temperature = if agent_context.experiments.is_active("higher_temperature") {
    0.8
} else {
    config.llm.temperature
};

let response = provider.chat_with_config(messages, ChatConfig {
    temperature: Some(temperature),
    ..Default::default()
}).await?;
```

### 6.3 Metrics Collection

```rust
// After turn completes
if let Some(exp_result) = agent_context.experiments.finish_turn(
    turns_used,
    tools_called,
    errors_count,
    api_cost,
) {
    tracing::info!("experiment {} completed: {:?}", exp_result.name, exp_result.metrics);
    
    // Optionally persist to SQLite
    if config.experiments.persist_metrics {
        db.insert_experiment_result(&exp_result).await?;
    }
}
```

---

## 7. Relation to Compile-Time Features

| Dimension | Feature Flags (spec #029) | Experiments |
|-----------|---------------------------|-------------|
| **When decided** | Build time | Runtime |
| **Recompile needed?** | Yes | No |
| **Binary size impact** | Yes (features are baked in) | No (always present code) |
| **Scope** | Whole binary (crate-level) | Individual sessions |
| **Best for** | Platform-specific, optional crates | A/B testing, tuning |
| **Example** | `--features tui` | `temperature = 0.8` |

**Corollary**: Experiments are used for **tuning and rollout**; feature flags are for **architectural choices**.

---

## 8. CLI Subcommands

### 8.1 List Active Experiments

```bash
cargo run --features full -- experiment list
```

Output:

```
Active experiments:
  ✓ higher_temperature     (50% rollout) — Test higher temperature
  ✓ deep_memory_retrieval  (25% rollout) — Retrieve 10 items instead of 5
  ✗ cascade_routing_v2     (0% rollout)  — Test new cascade routing
```

### 8.2 Show Experiment Details

```bash
cargo run --features full -- experiment show higher_temperature
```

Output:

```
Name: higher_temperature
Description: Test higher temperature (0.8) for more creative responses
Status: active (50% rollout)
Sessions affected: 1250 / 2500
Average turns: 8.2
Average cost: $0.12
```

### 8.3 Run Full Experiment

```bash
cargo run --features full -- experiment run <name> --samples 100
```

Runs the experiment across N sample sessions and reports results.

---

## 9. Key Invariants

### Always
- Experiments are disabled by default (`[experiments] enabled = false`)
- Rollout percentage is always deterministic (same session gets same decision every time)
- Experiment names are unique and stable (rename = breaking change)
- Metrics are collected without blocking the agent loop
- Results are immutable once written to disk

### Ask First
- Enabling experiments on production deployments (ensure metrics collection is working)
- Running conflicting experiments simultaneously (e.g., two temperature experiments)
- Increasing rollout above 50% before verifying results on the lower cohort

### Never
- Use experiments for security-critical feature gating (use compile-time flags instead)
- Persist sensitive user data in experiment results
- Block agent turns while writing experiment metrics
- Share experiment results without redacting user-identifying info

---

## 10. Success Criteria

An experiment is considered successful when:

1. **Active**: Rollout > 0% for at least N sessions
2. **Completing**: >90% of sessions complete the experiment
3. **Stable**: Metrics are within expected bounds (no crashes, no OOM, no hangs)
4. **Improving**: Target metric (e.g., accuracy, cost) is better than baseline

Example decision tree:

```
Is temperature=0.8 better than control?
├─ Higher accuracy? ✓ → Safe to increase rollout to 75%
├─ No change in accuracy? → Keep at current 50%, continue monitoring
└─ Lower accuracy? → Disable (revert to control)
```

---

## 11. See Also

- [[MOC-specs]] — all specifications
- [[029-feature-flags/spec]] — compile-time feature flags
- [[020-config-loading/spec]] — config loading and defaults
- `crates/zeph-experiments/src/lib.rs` — implementation
