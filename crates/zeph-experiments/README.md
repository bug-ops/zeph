# zeph-experiments

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.94-blue)](https://www.rust-lang.org)

Experiment engine for adaptive agent behavior — autonomous hyperparameter search and A/B testing for Zeph.

## Overview

Provides a self-experimentation loop that mutates agent configuration parameters (temperature, top-p, system prompt), runs benchmark evaluations using LLM-as-judge scoring, and tracks results in SQLite. Three search strategies — exhaustive grid sweep, random sampling, and local neighborhood search — are available. Experiments can be triggered on-demand via slash commands or scheduled via cron.

> [!NOTE]
> This crate is marked `publish = false`. It is an internal workspace crate not published to crates.io.

## Key types

| Type | Description |
|------|-------------|
| `Variation` | A config mutation (temperature, top-p, top-k, frequency/presence penalty, system prompt) |
| `ExperimentResult` | Single experiment outcome with LLM-as-judge score and latency |
| `ExperimentStatus` | Lifecycle enum (`Idle`, `Running`, `Completed`, `Failed`) |
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
eval_model = "claude-haiku-4-5-20251001"   # optional; defaults to primary provider

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
expected_keywords = ["ownership", "borrow", "move"]

[[cases]]
prompt = "Write a hello world in Python."
expected_keywords = ["print", "hello"]
```

Pass the benchmark file via config:

```toml
[experiments]
benchmark_file = ".zeph/benchmarks/core.toml"
```

## Features

| Feature | Description |
|---------|-------------|
| `experiments` | Activates the experiment engine (required) |
| `mock` | Enables `MockProvider` for offline testing |

## Installation

This crate is a workspace-internal dependency. Reference it from another workspace crate:

```toml
[dependencies]
zeph-experiments = { workspace = true }
```

Enable the feature flag:

```toml
[features]
experiments = ["zeph-experiments/experiments"]
```

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

MIT
