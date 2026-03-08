# Experiments

The experiments engine lets Zeph autonomously tune its own configuration by running controlled A/B trials against a benchmark. Inspired by [karpathy/autoresearch](https://github.com/karpathy/autoresearch), it varies a single parameter at a time, evaluates both baseline and candidate responses using an LLM-as-judge, and keeps the variation only if the candidate scores higher. This is an optional, feature-gated component (`--features experiments`) that persists results in SQLite.

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

## How It Works

Each experiment session follows a four-step loop:

1. **Select a parameter** — pick one tunable parameter (e.g., `temperature`, `top_p`, `retrieval_top_k`) and generate a candidate value.
2. **Run baseline** — send a benchmark prompt with the current configuration and record the response.
3. **Run candidate** — send the same prompt with the varied parameter and record the response.
4. **Judge** — an LLM evaluator scores both responses on a numeric scale. If the candidate exceeds the baseline by at least `min_improvement`, the variation is accepted; otherwise it is reverted.

The engine repeats this loop up to `max_experiments` times per session, staying within `max_wall_time_secs` and `eval_budget_tokens` limits.

## Tunable Parameters

The engine can vary the following parameters:

| Parameter | Type | Description |
|-----------|------|-------------|
| `temperature` | float | LLM sampling temperature |
| `top_p` | float | Nucleus sampling threshold |
| `top_k` | int | Top-K sampling limit |
| `frequency_penalty` | float | Penalize repeated tokens |
| `presence_penalty` | float | Penalize tokens already present |
| `retrieval_top_k` | int | Number of memory results to retrieve |
| `similarity_threshold` | float | Minimum similarity for memory recall |
| `temporal_decay` | float | Weight decay for older memories |

## Search Space

The search space defines the bounds and resolution for each tunable parameter. It is represented by a `SearchSpace` containing a list of `ParameterRange` entries.

Each `ParameterRange` specifies:

| Field | Type | Description |
|-------|------|-------------|
| `kind` | `ParameterKind` | Which parameter this range controls |
| `min` | `f64` | Lower bound of the range |
| `max` | `f64` | Upper bound of the range |
| `step` | `Option<f64>` | Discrete step size for grid and quantization. `None` means continuous |
| `default` | `f64` | Default value used as the baseline starting point |

The default search space covers five LLM generation parameters:

| Parameter | Min | Max | Step | Default |
|-----------|-----|-----|------|---------|
| `temperature` | 0.0 | 2.0 | 0.1 | 0.7 |
| `top_p` | 0.1 | 1.0 | 0.05 | 0.9 |
| `top_k` | 1 | 100 | 5 | 40 |
| `frequency_penalty` | -2.0 | 2.0 | 0.2 | 0.0 |
| `presence_penalty` | -2.0 | 2.0 | 0.2 | 0.0 |

You can customize the search space by adding or removing parameters. The remaining tunable parameters (`retrieval_top_k`, `similarity_threshold`, `temporal_decay`) are not included in the default space but can be added manually.

## Config Snapshot

A `ConfigSnapshot` captures the values of all tunable parameters for a single experiment arm. It serves as the bridge between the runtime configuration and the variation engine.

- The baseline snapshot is created from the current `Config` via `ConfigSnapshot::from_config`.
- Each variation produces a new snapshot with exactly one parameter changed (`snapshot.apply(&variation)`).
- The `diff` method compares two snapshots and returns the single `Variation` that differs, or `None` if zero or more than one parameter changed.

Snapshots also provide `to_generation_overrides()` to extract LLM-relevant parameters for use during evaluation.

## Variation Strategies

The variation engine uses a `VariationGenerator` trait to produce candidate parameter values. Each call to `next()` returns a `Variation` that changes exactly **one parameter** from the baseline. This one-at-a-time constraint isolates the effect of each change, making it possible to attribute score differences to a specific parameter.

All strategies track visited variations via a `HashSet<Variation>` to avoid re-testing the same configuration. Floating-point values use `OrderedFloat` for reliable hashing and equality.

### Grid

`GridStep` performs a systematic sweep of every parameter through its discrete steps from `min` to `max`. Parameters are swept one at a time: all grid points for the first parameter are enumerated before moving to the next. Already-visited variations are skipped. Returns `None` when the full grid has been covered.

Grid is the default starting strategy. It provides complete coverage of the discrete search space and is deterministic (no randomness involved). Values are quantized to the nearest step to avoid floating-point accumulation errors.

### Random

`Random` samples uniformly within each parameter's bounds. At each call, it picks a random parameter, samples a random value from its `[min, max]` range, and quantizes to the nearest step. The sample is rejected if already visited. After 1000 consecutive rejections, the space is considered exhausted.

Random sampling is seeded (`SmallRng::seed_from_u64`) for reproducibility. It is useful when the grid is too large to sweep exhaustively or when you want to explore the space without systematic bias.

### Neighborhood

`Neighborhood` perturbs the current best configuration by a small amount. At each call, it picks a random parameter and computes a new value as `baseline ± U(-radius, radius) * step`, then clamps and quantizes the result. This focuses exploration around a known-good region.

Neighborhood is most useful as a refinement step after a grid or random sweep has identified a promising baseline. The `radius` parameter (must be positive) controls the perturbation range in units of `step`. For example, `radius = 1.0` with `step = 0.1` means perturbations of at most ±0.1 from the baseline value.

### Strategy Selection

Choose a strategy based on your goals:

| Strategy | Best for | Deterministic | Coverage |
|----------|----------|---------------|----------|
| Grid | Small search spaces, complete coverage | Yes | Exhaustive |
| Random | Large spaces, quick exploration | Seeded | Stochastic |
| Neighborhood | Refinement around a known-good config | Seeded | Local |

A typical workflow combines strategies across sessions: start with Grid or Random to identify promising regions, then switch to Neighborhood for fine-tuning.

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

Subject responses are wrapped in `<subject_response>` XML boundary tags before being sent to the judge. XML metacharacters (`&`, `<`, `>`) in the response and reference fields are escaped to prevent prompt injection from the evaluated model.

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

## Safety Model

The experiments engine uses a conservative, double opt-in design:

1. **Feature gate** — the `experiments` feature must be compiled in. It is off by default.
2. **Config gate** — `enabled = true` must be set in `[experiments]`. Default is `false`.
3. **No auto-apply** — `auto_apply` defaults to `false`. When disabled, accepted variations are recorded but not written back to the live configuration. Set to `true` only when you want the agent to self-tune in production.
4. **Budget limits** — `max_experiments`, `max_wall_time_secs`, and `eval_budget_tokens` cap resource usage per session.
5. **Sandboxed scope** — experiments only vary inference and retrieval parameters. They cannot modify tool permissions, security settings, or system prompts.

## Configuration

Add an `[experiments]` section to `config.toml`:

```toml
[experiments]
enabled = true
# eval_model = "claude-sonnet-4-20250514"  # Model for LLM-as-judge evaluation (default: agent's model)
# benchmark_file = "benchmarks/eval.toml"  # Prompt set for A/B comparison
max_experiments = 20                       # Max variations per session (default: 20, range: 1-1000)
max_wall_time_secs = 3600                  # Wall-clock budget per session in seconds (default: 3600, range: 60-86400)
min_improvement = 0.5                      # Minimum score delta to accept a variation (default: 0.5, range: 0.0-100.0)
eval_budget_tokens = 100000                # Token budget for all judge calls in a session (default: 100000, range: 1000-10000000)
auto_apply = false                         # Write accepted variations to live config (default: false)

[experiments.schedule]
enabled = false                            # Enable cron-based automatic runs (default: false)
cron = "0 3 * * *"                         # Cron expression for scheduled runs (default: daily at 03:00)
max_experiments_per_run = 20               # Max variations per scheduled run (default: 20, range: 1-100)
max_wall_time_secs = 1800                  # Wall-time cap per scheduled run in seconds (default: 1800, range: 60-86400)
```

### Field Reference

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Master switch for the experiments engine |
| `eval_model` | string | agent's model | Model used for LLM-as-judge scoring |
| `benchmark_file` | path | none | Path to a TOML file with evaluation prompts |
| `max_experiments` | u32 | `20` | Maximum variations per session |
| `max_wall_time_secs` | u64 | `3600` | Wall-clock time limit per session |
| `min_improvement` | f64 | `0.5` | Minimum score delta to accept a variation |
| `eval_budget_tokens` | u64 | `100000` | Token budget across all judge calls |
| `auto_apply` | bool | `false` | Apply accepted variations to live config |
| `schedule.enabled` | bool | `false` | Enable automatic scheduled experiment runs |
| `schedule.cron` | string | `"0 3 * * *"` | Cron expression (5-field) for scheduled runs |
| `schedule.max_experiments_per_run` | u32 | `20` | Cap per scheduled run |
| `schedule.max_wall_time_secs` | u64 | `1800` | Wall-time cap per scheduled run (overrides `max_wall_time_secs`) |

## Persistence

Experiment results are stored in the `experiment_results` SQLite table (same database as memory). Each row tracks:

- `session_id` — groups results from a single experiment run
- `parameter` — which parameter was varied (e.g., `temperature`)
- `value_json` — the candidate value as JSON
- `baseline_score` / `candidate_score` — numeric scores from the judge
- `delta` — score difference (candidate minus baseline)
- `latency_ms` — wall-clock time for the trial
- `tokens_used` — tokens consumed by the judge call
- `accepted` — whether the variation met the `min_improvement` threshold
- `source` — `manual` or `scheduled`

## Error Handling

| Error | Cause | Effect |
|-------|-------|--------|
| `BenchmarkLoad` | File not found or unreadable | Evaluator construction fails |
| `BenchmarkParse` | Invalid TOML syntax | Evaluator construction fails |
| `EmptyBenchmarkSet` | No cases in the dataset | Evaluator construction fails |
| `PathTraversal` | Benchmark path escapes allowed directory | Evaluator construction fails |
| `BenchmarkTooLarge` | Benchmark file exceeds 10 MiB | Evaluator construction fails |
| `Llm` | Subject model call fails | Evaluation aborts (fatal) |
| `JudgeParse` | Judge returns invalid or non-finite score | Case excluded, logged as warning |
| `BudgetExceeded` | Token budget exhausted | Remaining cases skipped, partial report returned |

## Scheduler Integration

When both `experiments` and `scheduler` features are enabled, the experiment engine can run automatically on a cron schedule. This is configured via the `[experiments.schedule]` section.

### How It Works

1. At startup, if `experiments.enabled` and `experiments.schedule.enabled` are both `true`, the scheduler registers an `auto-experiment` periodic task with the configured cron expression.
2. When the cron fires, an `ExperimentTaskHandler` spawns a non-blocking `tokio::spawn` task that runs a full experiment session.
3. An `AtomicBool` running guard prevents overlapping sessions. If a previous session is still in progress when the next cron trigger fires, the new run is skipped with a warning log.
4. Scheduled runs use `ExperimentSource::Scheduled` tagging so results can be distinguished from manual runs in the persistence layer (the `source` column in `experiment_results`).
5. The `schedule.max_wall_time_secs` field (default: 1800s) overrides the top-level `max_wall_time_secs` for scheduled runs, ensuring background sessions finish before the next cron trigger on typical schedules.

### Requirements

- Both `experiments` and `scheduler` feature flags must be compiled in.
- A valid `benchmark_file` must be configured (the handler loads the benchmark set on each run).
- The agent's LLM provider must be available for both subject and judge calls.

### Task Kind

The scheduler uses a dedicated `TaskKind::Experiment` variant (kind string: `"experiment"`). This can also be used in `[[scheduler.tasks]]` config entries, though the `[experiments.schedule]` section is the recommended way to configure automatic runs.

## CLI Flags

> [!NOTE]
> CLI flags are planned for Phase 6 of the experiments epic and are not yet available.

| Flag | Description |
|------|-------------|
| `--experiment-run` | Start an experiment session from the command line |
| `--experiment-report` | Print a summary of past experiment results |

## TUI Commands

> [!NOTE]
> TUI commands are planned for Phase 6 of the experiments epic and are not yet available.

| Command | Description |
|---------|-------------|
| `/experiment start` | Start a new experiment session |
| `/experiment stop` | Stop the running session |
| `/experiment status` | Show progress of the current session |
| `/experiment report` | Display results from past sessions |
| `/experiment best` | Show the best accepted variation per parameter |

## Related

- [Scheduler](scheduler.md) — cron-based task scheduler that drives automatic experiment runs
- [Daemon & Scheduler](../advanced/daemon.md) — running the scheduler alongside the gateway and A2A server
- [Self-Learning Skills](../advanced/self-learning.md) — passive feedback detection and Wilson score ranking
- [Model Orchestrator](../advanced/orchestrator.md) — multi-model routing and fallback chains
- [Feature Flags](../reference/feature-flags.md) — enabling the `experiments` feature
- [Configuration](../reference/configuration.md) — full config reference
- [Adaptive Inference](../advanced/adaptive-inference.md) — runtime model routing that experiments can tune
