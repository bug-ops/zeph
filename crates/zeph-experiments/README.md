# zeph-experiments

[![Crates.io](https://img.shields.io/crates/v/zeph-experiments)](https://crates.io/crates/zeph-experiments)
[![docs.rs](https://img.shields.io/docsrs/zeph-experiments)](https://docs.rs/zeph-experiments)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.98-blue)](https://www.rust-lang.org)

Experiment engine for adaptive agent behavior — autonomous hyperparameter search and A/B testing for Zeph.

## Overview

Provides a self-experimentation loop that mutates agent configuration parameters (temperature, top-p, system prompt), runs benchmark evaluations using LLM-as-judge scoring, and tracks results in SQLite. Three search strategies — exhaustive grid sweep, random sampling, and local neighborhood search — are available. Experiments can be triggered on-demand via slash commands or scheduled via cron.

## Key types

| Type | Description |
|------|-------------|
| `Variation` | A config mutation (temperature, top-p, top-k, frequency/presence penalty, system prompt) |
| `ExperimentResult` | Single experiment outcome with LLM-as-judge score and latency |
| `ExperimentEngine` | Orchestrator: evaluates a baseline, iterates variations (greedy hill-climbing), and persists results; yields an `ExperimentSessionReport` |
| `BenchmarkSet` / `BenchmarkCase` | Evaluation dataset loaded from a TOML file |
| `Evaluator` | Runs benchmark cases with parallel judge scoring and token budget enforcement |
| `EvalReport` | Summary with mean score, p50/p95 latency, error count |
| `SearchSpace` | Parameter ranges and bounds for variation generation |
| `VariationGenerator` | Strategy trait — `GridStep`, `Random`, `Neighborhood` implementations |
| `ConfigSnapshot` | Captures the current baseline config for rollback |

## Usage

Experiments are launched from the agent chat via slash commands:

```text
/experiment start         # run up to max_experiments from config
/experiment start 5       # run at most 5 experiments
/experiment stop          # cancel the running session
/experiment status        # show current session progress
/experiment report        # print results table
/experiment best          # show the top-scoring variation
```

> [!NOTE]
> Only one experiment session can be active at a time. Use `/experiment stop` to cancel before starting a new one.

## Configuration

```toml
[experiments]
enabled = true
max_experiments = 10
max_wall_time_secs = 300
eval_budget_tokens = 4096
min_improvement = 0.05       # minimum score gain to accept a variation
eval_provider = "fast"       # references a [[llm.providers]] name; empty = primary provider

# Scheduled experiments via cron
[experiments.schedule]
cron = "0 0 2 * * *"         # daily at 02:00
max_experiments_per_run = 3
max_wall_time_secs = 600
```

Benchmark datasets are loaded from TOML files:

```toml
# benchmark.toml
[[cases]]
prompt = "Explain Rust ownership in one sentence."
reference = "Each value has a single owner; the value is dropped when the owner goes out of scope."
tags = ["rust", "concepts"]

[[cases]]
prompt = "Write a hello world in Python."
reference = "print(\"hello world\")"
```

Pass the benchmark file via config:

```toml
[experiments]
benchmark_file = ".zeph/benchmarks/core.toml"
```

## Features

| Feature | Default | Description |
|---------|---------|-------------|
| `sqlite` | yes | SQLite backend forwarded to `zeph-memory` |
| `postgres` | no | PostgreSQL backend forwarded to `zeph-memory` |
| `mock` | no | Exposes mock types for downstream tests |

> [!NOTE]
> `zeph-memory` (and transitively `zeph-db`) requires a backend to compile; `sqlite` is the default so this crate builds in isolation.

## Installation

```bash
cargo add zeph-experiments
```

Or reference it from another workspace crate:

```toml
[dependencies]
zeph-experiments = { workspace = true }
```

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

Licensed under either of [MIT](../../LICENSE) or [Apache License, Version 2.0](../../LICENSE-APACHE) at your option.
