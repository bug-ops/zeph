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
# benchmark_file = "benchmarks/eval.jsonl" # Prompt set for A/B comparison
max_experiments = 20                       # Max variations per session (default: 20, range: 1-1000)
max_wall_time_secs = 3600                  # Wall-clock budget per session in seconds (default: 3600, range: 60-86400)
min_improvement = 0.5                      # Minimum score delta to accept a variation (default: 0.5, range: 0.0-100.0)
eval_budget_tokens = 100000                # Token budget for all judge calls in a session (default: 100000, range: 1000-10000000)
auto_apply = false                         # Write accepted variations to live config (default: false)

[experiments.schedule]
enabled = false                            # Enable cron-based automatic runs (default: false)
cron = "0 3 * * *"                         # Cron expression for scheduled runs (default: daily at 03:00)
max_experiments_per_run = 20               # Max variations per scheduled run (default: 20, range: 1-100)
```

### Field Reference

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Master switch for the experiments engine |
| `eval_model` | string | agent's model | Model used for LLM-as-judge scoring |
| `benchmark_file` | path | none | Path to a JSONL file with evaluation prompts |
| `max_experiments` | u32 | `20` | Maximum variations per session |
| `max_wall_time_secs` | u64 | `3600` | Wall-clock time limit per session |
| `min_improvement` | f64 | `0.5` | Minimum score delta to accept a variation |
| `eval_budget_tokens` | u64 | `100000` | Token budget across all judge calls |
| `auto_apply` | bool | `false` | Apply accepted variations to live config |
| `schedule.enabled` | bool | `false` | Enable automatic scheduled experiment runs |
| `schedule.cron` | string | `"0 3 * * *"` | Cron expression (5-field) for scheduled runs |
| `schedule.max_experiments_per_run` | u32 | `20` | Cap per scheduled run |

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

- [Feature Flags](../reference/feature-flags.md) — enabling the `experiments` feature
- [Configuration](../reference/configuration.md) — full config reference
- [Adaptive Inference](../advanced/adaptive-inference.md) — runtime model routing that experiments can tune
