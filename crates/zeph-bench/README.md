# zeph-bench

[![Crates.io](https://img.shields.io/crates/v/zeph-bench)](https://crates.io/crates/zeph-bench)
[![docs.rs](https://img.shields.io/docsrs/zeph-bench)](https://docs.rs/zeph-bench)
[![CI](https://img.shields.io/github/actions/workflow/status/bug-ops/zeph/ci.yml?branch=main)](https://github.com/bug-ops/zeph/actions)
[![MSRV](https://img.shields.io/badge/MSRV-1.95-blue)](https://www.rust-lang.org)
[![License](https://img.shields.io/crates/l/zeph-bench)](../../LICENSE)

Benchmark harness for evaluating Zeph agent performance on standardized datasets.

Feeds LOCOMO, GAIA, FRAMES, LongMemEval, and tau-bench tasks through the full Zeph agent
loop and records correctness, latency, and token usage. Designed for reproducible baseline
evaluation: no tools, no memory, no MCP — raw model capability only.

## Baseline Results

`gpt-5.4-mini`, baseline mode, 2026-04-25:

| Dataset | Scorer | Scenarios | Mean score | Exact match |
|---------|--------|-----------|------------|-------------|
| LOCOMO | Token F1 ≥ 0.5 | 11 | **1.0000** | 11/11 |
| GAIA | GAIA normalized exact | 8 | **1.0000** | 8/8 |
| LongMemEval | Exact match + Token F1 | 6 | **1.0000** | 6/6 |
| tau-bench | Task completion (exact) | 5 | **1.0000** | 5/5 |

> [!NOTE]
> Baseline mode injects a concise-answer system prompt and post-processes responses
> (first-line extraction, markdown strip) before scoring. This is the primary driver
> of score quality — without it, verbose answers fail both Token F1 and exact-match evaluators.

## CLI Usage

`zeph-bench` is invoked through the main `zeph` binary (requires the `bench` feature):

```bash
# List available datasets
zeph bench list

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
```

> [!TIP]
> `--provider` references a named entry from `[[llm.providers]]` in your config.
> If omitted, the default provider is used. Use a fast, cheap model for large evaluation runs.

Output directory receives two files: `results.json` (machine-readable) and `summary.md`
(human-readable markdown table).

## Library Usage

```rust
use std::path::Path;
use zeph_bench::runner::{BenchRunner, RunOptions};
use zeph_bench::loaders::{GaiaLoader, GaiaEvaluator};
use zeph_llm::{any::AnyProvider, mock::MockProvider};

# async fn example() -> Result<(), zeph_bench::BenchError> {
let provider = AnyProvider::Mock(MockProvider::with_responses(vec!["1945".into()]));
let runner = BenchRunner::new(provider, false);
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
    fn name(&self) -> &str { "my-dataset" }

    fn load(&self, path: &Path) -> Result<Vec<Scenario>, zeph_bench::BenchError> {
        // parse your file format here
        todo!()
    }
}

struct MyEvaluator;

impl Evaluator for MyEvaluator {
    fn evaluate(&self, scenario: &Scenario, response: &str) -> EvalResult {
        let score = if response.trim() == scenario.expected.trim() { 1.0 } else { 0.0 };
        EvalResult { score }
    }
}
```

## Supported Datasets

| Dataset | Format | Scorer | Status |
|---------|--------|--------|--------|
| [LOCOMO](https://github.com/snap-research/locomo) | JSON | Token F1 ≥ 0.5 | Ready |
| [GAIA](https://huggingface.co/datasets/gaia-benchmark/GAIA) | JSONL | Normalized exact match | Ready |
| [FRAMES](https://huggingface.co/datasets/google/frames-benchmark) | JSONL | Normalized exact match | Ready |
| LongMemEval | JSONL | Exact match + Token F1 | Ready |
| tau-bench | JSON | Task completion (exact) | Ready |

> [!IMPORTANT]
> Requires Rust 1.95 or later.

## Architecture

The harness is built on three composable traits:

- **`DatasetLoader`** — reads a dataset file, returns `Vec<Scenario>`
- **`Evaluator`** — scores one agent response against a `Scenario`
- **`BenchmarkChannel`** — headless `Channel` impl that drives the agent loop without a terminal

`BenchRunner` wires them together: one fresh `Agent<BenchmarkChannel>` per scenario, no shared
state between runs. Results accumulate into a `BenchRun` and are persisted by `ResultWriter`.

## Installation

```toml
[dependencies]
zeph-bench = "0.20"
```

This crate is part of the [Zeph](https://github.com/bug-ops/zeph) workspace. See the
[API documentation](https://docs.rs/zeph-bench) for the complete reference.

## License

Licensed under MIT OR Apache-2.0 — see [LICENSE](../../LICENSE) for details.
