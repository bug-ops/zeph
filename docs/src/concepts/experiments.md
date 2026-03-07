# Experiments

The experiments module provides an autonomous self-experimentation engine that evaluates LLM responses against structured benchmark datasets using an LLM-as-judge pattern. This page covers the benchmark dataset format, the evaluator, scoring reports, budget enforcement, and parallel evaluation.

Experiments is an optional, feature-gated component (`--features experiments`). It is compiled into the `full` feature set but disabled at runtime by default (`enabled = false`).

## Prerequisites

Enable the `experiments` feature flag before building:

```bash
cargo build --release --features experiments
```

The `experiments` feature is also included in the `full` feature set:

```bash
cargo build --release --features full
```

See [Feature Flags](../reference/feature-flags.md) for the full flag list.

## Benchmark Dataset

A benchmark dataset is a TOML file containing a list of test cases. Each case defines a prompt to send to the subject model, with optional context, reference answer, and tags.

```toml
[[cases]]
prompt = "Explain the difference between TCP and UDP"
tags = ["knowledge", "networking"]

[[cases]]
prompt = "Write a Python function to find the longest palindromic substring"
reference = "Dynamic programming approach with O(n^2) time"
tags = ["coding", "algorithms"]

[[cases]]
prompt = "Summarize the key ideas of the transformer architecture"
context = "The transformer was introduced in 'Attention Is All You Need' (2017)..."
tags = ["knowledge", "ml"]
```

### Case Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `prompt` | string | yes | The prompt sent to the subject model |
| `context` | string | no | System context injected before the prompt |
| `reference` | string | no | Reference answer the judge uses to calibrate scoring |
| `tags` | string array | no | Labels for filtering or grouping in reports |

Load a dataset from disk with `BenchmarkSet::from_file`:

```rust
# use std::path::Path;
# use zeph_core::experiments::BenchmarkSet;
let dataset = BenchmarkSet::from_file(Path::new("benchmarks/default.toml"))?;
dataset.validate()?; // rejects empty case lists
```

## LLM-as-Judge Evaluator

The `Evaluator` scores a subject model's responses by sending each one to a separate judge model. The judge rates responses on a 1--10 scale across four weighted criteria:

| Criterion | Weight |
|-----------|--------|
| Accuracy | 30% |
| Completeness | 25% |
| Clarity | 25% |
| Relevance | 20% |

The judge returns structured JSON output (`JudgeOutput`) containing a numeric score and a one-sentence justification.

### Evaluation Flow

1. **Subject calls** -- the evaluator sends each benchmark case to the subject model sequentially, collecting responses.
2. **Judge calls** -- responses are scored in parallel (up to `parallel_evals` concurrent tasks, default 3) using a separate judge model.
3. **Budget check** -- before each judge call, the evaluator checks cumulative token usage against the configured budget. If the budget is exhausted, remaining cases are skipped.
4. **Report** -- per-case scores are aggregated into an `EvalReport`.

### Security

Subject responses are wrapped in `<subject_response>` XML boundary tags before being sent to the judge. XML metacharacters (`&`, `<`, `>`) in the response are escaped to prevent prompt injection from the evaluated model.

### Creating an Evaluator

```rust
# use std::sync::Arc;
# use zeph_core::experiments::{BenchmarkSet, Evaluator};
# use zeph_llm::any::AnyProvider;
# fn example(judge: Arc<AnyProvider>, subject: &AnyProvider, benchmark: BenchmarkSet) {
let evaluator = Evaluator::new(
    judge,              // judge model provider
    benchmark,          // loaded benchmark dataset
    100_000,            // token budget for all judge calls
)?
.with_parallel_evals(5); // override default concurrency (3)
# }
```

Run the evaluation:

```rust
# use zeph_core::experiments::Evaluator;
# use zeph_llm::any::AnyProvider;
# async fn example(evaluator: &Evaluator, subject: &AnyProvider) {
let report = evaluator.evaluate(subject).await?;
println!("Mean score: {:.1}/10 ({} of {} cases)",
    report.mean_score, report.cases_scored, report.cases_total);
# }
```

## Evaluation Report

`EvalReport` contains aggregate metrics and per-case detail:

| Field | Type | Description |
|-------|------|-------------|
| `mean_score` | `f64` | Mean score across scored cases (NaN if none succeeded) |
| `p50_latency_ms` | `u64` | Median latency of judge calls |
| `p95_latency_ms` | `u64` | 95th-percentile latency of judge calls |
| `total_tokens` | `u64` | Total tokens consumed by judge calls |
| `cases_scored` | `usize` | Number of successfully scored cases |
| `cases_total` | `usize` | Total cases in the benchmark set |
| `is_partial` | `bool` | True if budget was exceeded or errors occurred |
| `error_count` | `usize` | Number of failed cases (LLM error, parse error, or budget) |
| `per_case` | `Vec<CaseScore>` | Per-case scores ordered by case index |

Each `CaseScore` entry contains:

| Field | Type | Description |
|-------|------|-------------|
| `case_index` | `usize` | Zero-based index into the benchmark cases |
| `score` | `f64` | Clamped score in [1.0, 10.0] |
| `reason` | `String` | Judge's one-sentence justification |
| `latency_ms` | `u64` | Wall-clock time for the judge call |
| `tokens` | `u64` | Tokens consumed by this judge call |

## Budget Enforcement

The evaluator tracks cumulative token usage across all judge calls with an atomic counter. Before each judge call, the current total is checked against the configured `budget_tokens`. If the budget is exhausted:

- The current batch of in-flight judge calls is drained
- Remaining cases are excluded from scoring
- The report is marked as partial (`is_partial = true`)

Budget exhaustion is not a fatal error -- the evaluator returns a valid `EvalReport` with partial results.

## Parallel Evaluation

Judge calls run concurrently using `FuturesUnordered` with a `Semaphore` controlling the maximum number of in-flight requests. The default concurrency limit is 3 and can be overridden with `with_parallel_evals`. Subject calls remain sequential to avoid overwhelming the subject model.

Each parallel judge task receives a cloned provider instance so per-task token usage tracking is isolated. The shared atomic token counter aggregates usage across all tasks for budget enforcement.

## Error Handling

| Error | Cause | Effect |
|-------|-------|--------|
| `BenchmarkLoad` | File not found or unreadable | Evaluator construction fails |
| `BenchmarkParse` | Invalid TOML syntax | Evaluator construction fails |
| `EmptyBenchmarkSet` | No cases in the dataset | Evaluator construction fails |
| `Llm` | Subject model call fails | Evaluation aborts (fatal) |
| `JudgeParse` | Judge returns invalid or non-finite score | Case excluded, logged as warning |
| `BudgetExceeded` | Token budget exhausted | Remaining cases skipped, partial report returned |

## Related

- [Self-Learning Skills](../advanced/self-learning.md) -- passive feedback detection and Wilson score ranking
- [Model Orchestrator](../advanced/orchestrator.md) -- multi-model routing and fallback chains
- [Feature Flags](../reference/feature-flags.md) -- enabling the `experiments` feature
