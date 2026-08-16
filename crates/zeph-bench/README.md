# zeph-bench

[![Crates.io](https://img.shields.io/crates/v/zeph-bench)](https://crates.io/crates/zeph-bench)
[![docs.rs](https://img.shields.io/docsrs/zeph-bench)](https://docs.rs/zeph-bench)
[![CI](https://img.shields.io/github/actions/workflow/status/bug-ops/zeph/ci.yml?branch=main)](https://github.com/bug-ops/zeph/actions)
[![MSRV](https://img.shields.io/badge/MSRV-1.97-blue)](https://www.rust-lang.org)
[![License](https://img.shields.io/crates/l/zeph-bench)](../../LICENSE)

Benchmark harness for evaluating Zeph agent performance on standardized datasets.

Feeds LOCOMO, GAIA, FRAMES, LongMemEval, and tau2-bench tasks through the full Zeph agent
loop and records correctness, latency, and token usage. The default run is a reproducible
baseline — no tools, no memory, no MCP, temperature pinned to `0.0` — measuring raw model
capability; `SemanticMemory` can be wired in per run to measure what memory adds
(see [Memory A/B mode](#memory-ab-mode---baseline)).

## Baseline Results

`gpt-5.4-mini`, baseline mode, 2026-04-25:

| Dataset | Scorer | Scenarios | Mean score | Exact match |
|---------|--------|-----------|------------|-------------|
| LOCOMO | Token F1 ≥ 0.5 | 11 | **1.0000** | 11/11 |
| GAIA | GAIA normalized exact | 8 | **1.0000** | 8/8 |
| FRAMES | Normalized exact match | 7 | **1.0000** | 7/7 |
| LongMemEval | Exact match + Token F1 | 6 | **1.0000** | 6/6 |
| tau2-bench | Task completion (exact) | 5 | **1.0000** | 5/5 |

> [!NOTE]
> Baseline mode injects a concise-answer system prompt and post-processes responses
> (first-line extraction, markdown strip) before scoring. This is the primary driver
> of score quality — without it, verbose answers fail both Token F1 and exact-match evaluators.

## CLI Usage

`zeph-bench` is invoked through the main `zeph` binary (requires the `bench` feature):

```bash
# List available datasets and their cache status
zeph bench list

# Download tau2-bench into the local cache (other datasets must be fetched manually —
# `zeph bench list` prints their source URLs)
zeph bench download --dataset tau2-bench

# Run GAIA sample
zeph bench run \
  --dataset gaia \
  --data-file path/to/gaia.jsonl \
  --provider my-provider \
  --output results/

# Run a single scenario for debugging
zeph bench run \
  --dataset locomo \
  --data-file path/to/locomo.json \
  --scenario s1_0 \
  --output results/

# Resume an interrupted run
zeph bench run \
  --dataset gaia \
  --data-file path/to/gaia.jsonl \
  --resume \
  --output results/

# Print a summary of a previous run
zeph bench show --results results/results.json
```

> [!TIP]
> `--provider` references a named entry from `[[llm.providers]]` in your config.
> If omitted, the default provider is used. Use a fast, cheap model for large evaluation runs.

Additional `run` flags:

| Flag | Effect |
|------|--------|
| `--scenario <id>` | Run a single scenario instead of the whole dataset |
| `--resume` | Skip scenarios already completed in a prior run in the same output directory |
| `--baseline` | Run the memory-off/memory-on A/B pair (see below) |
| `--no-deterministic` | Use the provider's configured temperature; by default temperature is forced to `0.0` for reproducibility |

Output directory receives two files: `results.json` (machine-readable) and `summary.md`
(human-readable markdown table).

### Memory A/B mode (`--baseline`)

`--baseline` runs the dataset twice — once with `MemoryMode::Off` and once with a per-scenario
SQLite-backed `SemanticMemory` (`MemoryMode::On`) — and writes each pass to its own subdirectory
plus a delta report:

```
<output>/baseline/memory-off/{results.json,summary.md}
<output>/baseline/memory-on/{results.json,summary.md}
<output>/baseline/comparison.json
```

`comparison.json` is a `BaselineComparison`: per-scenario `ScenarioDelta` records plus the
aggregate score difference between the two passes.

## Library Usage

```rust
use std::path::Path;
use zeph_bench::runner::{BenchRunner, RunOptions};
use zeph_bench::loaders::{GaiaLoader, GaiaEvaluator};
use zeph_llm::{any::AnyProvider, mock::MockProvider};

# async fn example() -> Result<(), zeph_bench::BenchError> {
let provider = AnyProvider::Mock(MockProvider::with_responses(vec!["1945".into()]));
let runner = BenchRunner::new(provider);
let opts = RunOptions::default();
let run = runner
    .run_dataset(&GaiaLoader::all_levels(), &GaiaEvaluator, Path::new("gaia.jsonl"), opts)
    .await?;
println!("mean score: {:.4}", run.aggregate.mean_score);
# Ok(())
# }
```

### Implementing a custom dataset

```rust
use zeph_bench::scenario::{DatasetLoader, Evaluator, EvalResult, Scenario};
use std::path::Path;

struct MyLoader;

impl DatasetLoader for MyLoader {
    fn name(&self) -> &'static str { "my-dataset" }

    fn load(&self, path: &Path) -> Result<Vec<Scenario>, zeph_bench::BenchError> {
        // parse your file format here
        todo!()
    }
}

struct MyEvaluator;

impl Evaluator for MyEvaluator {
    fn evaluate(&self, scenario: &Scenario, response: &str) -> EvalResult {
        let passed = response.trim() == scenario.expected.trim();
        EvalResult {
            scenario_id: scenario.id.clone(),
            score: if passed { 1.0 } else { 0.0 },
            passed,
            details: format!("exact_match={passed}"),
        }
    }
}
```

## Supported Datasets

| Dataset (`--dataset` name) | Format | Scorer | Loader / Evaluator |
|---------|--------|--------|--------|
| `locomo` — [LOCOMO](https://github.com/snap-research/locomo) | JSON | Token F1 ≥ 0.5 | `LocomoLoader` / `LocomoEvaluator` |
| `gaia` — [GAIA](https://huggingface.co/datasets/gaia-benchmark/GAIA) | JSONL | Normalized exact match | `GaiaLoader` / `GaiaEvaluator` |
| `frames` — [FRAMES](https://huggingface.co/datasets/google/frames-benchmark) | JSONL | Normalized exact match | `FramesLoader` / `FramesEvaluator` |
| `longmemeval` — LongMemEval | JSONL | Exact match + Token F1 | `LongMemEvalLoader` / `LongMemEvalEvaluator` |
| `tau2-bench-retail`, `tau2-bench-airline` | JSON | Task completion (exact) | `Tau2BenchLoader` / `TauBenchEvaluator` |

> [!NOTE]
> tau2-bench is tool-use, not knowledge retrieval: it runs under `ResponseMode::ToolUse` against a
> simulated environment (`RetailEnv` / `AirlineEnv`) and is scored on the action trace rather than
> the response text. Every other dataset runs under `ResponseMode::TerseAnswer`.

> [!IMPORTANT]
> Requires Rust 1.97 or later.

## Architecture

The harness is built on three composable traits:

- **`DatasetLoader`** — reads a dataset file, returns `Vec<Scenario>`
- **`Evaluator`** — scores one agent response against a `Scenario`
- **`BenchmarkChannel`** — headless `Channel` impl that drives the agent loop without a terminal

`BenchRunner` wires them together: one fresh `Agent<BenchmarkChannel>` per scenario, no shared
state between runs. Results accumulate into a `BenchRun` and are persisted by `ResultWriter`.

`RunOptions` controls each run: `scenario_filter` (single-scenario debugging), `completed_ids`
(resume), and `memory_mode`. With `MemoryMode::On`, `BenchRunner::with_memory_params` supplies the
`BenchMemoryParams` (data dir, embedding model, run ID, dataset) used to build a per-scenario
SQLite-backed `SemanticMemory`; `MemoryMode::Off` is the default.

## Features

| Feature | Default | Description |
|---------|---------|-------------|
| `sqlite` | yes | SQLite backend forwarded to `zeph-memory`/`zeph-core`/`zeph-skills`/`zeph-tools` |
| `postgres` | no | PostgreSQL backend forwarded to the same crates |

## Installation

```toml
[dependencies]
zeph-bench = "0.22"
```

This crate is part of the [Zeph](https://github.com/bug-ops/zeph) workspace. See the
[API documentation](https://docs.rs/zeph-bench) for the complete reference.

## License

Licensed under MIT OR Apache-2.0 — see [LICENSE](../../LICENSE) for details.
