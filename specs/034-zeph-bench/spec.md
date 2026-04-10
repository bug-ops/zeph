---
aliases:
  - Benchmark Harness
  - zeph-bench
tags:
  - sdd
  - spec
  - benchmarking
  - testing
created: 2026-04-08
status: draft
related:
  - "[[MOC-specs]]"
---

# Feature: zeph-bench — Benchmark Harness

> **Status**: Draft
> **Author**: Andrei G.
> **Date**: 2026-04-08
> **Branch**: feat/m{N}/{issue}-zeph-bench

---

## 1. Overview

### Problem Statement

Zeph's differentiating capabilities — persistent semantic memory across sessions,
multi-hop recall, and tool-augmented reasoning — have no external, reproducible
measurement. Internal metrics (admission scores, recall counts) are not comparable
to other agents. Without benchmark results on standard leaderboards, architectural
claims about Zeph's memory and tool-use superiority are unverifiable.

### Goal

Add a `bench` feature-gated crate (`zeph-bench`) that runs Zeph against standard
AI-agent benchmarks in a fully automated, reproducible, and deterministic manner,
producing structured output suitable for external leaderboard submission and
internal regression tracking.

### Out of Scope

- Benchmark dataset hosting or curation (datasets are downloaded from their
  official sources at runtime; none are bundled in the repo)
- Automated leaderboard submission (output format is compliant; submission is manual)
- New agent capabilities to improve benchmark scores (the harness evaluates
  existing capabilities without changing core logic)
- Real-time TUI rendering during benchmark runs (headless mode only for
  reproducibility)
- Parallel multi-scenario execution (single scenario at a time to avoid
  Qdrant/SQLite contention)

---

## 2. User Stories

### US-001: Run a named benchmark from CLI

AS A developer
I WANT to run `zeph bench run --dataset longmemeval --output results/`
SO THAT I get a structured JSON result file and a Markdown summary without manual
orchestration.

**Acceptance criteria:**
```
GIVEN a valid dataset name and an accessible agent config
WHEN the command is invoked
THEN the harness downloads (or uses cached) scenarios, feeds them to the agent
  one at a time, collects responses, evaluates them against ground truth, and
  writes a JSON result file and a Markdown summary to the output directory
```

### US-002: Memory isolation between scenarios

AS A developer
I WANT each benchmark scenario to start with a clean memory state
SO THAT earlier scenarios cannot contaminate later ones, giving reproducible
per-scenario scores.

**Acceptance criteria:**
```
GIVEN a benchmark with N scenarios
WHEN scenario K begins
THEN the Qdrant collection and SQLite conversation history used by the bench
  session are fully reset, with no traces of scenario K-1
```

### US-003: Deterministic mode

AS A developer
I WANT all LLM calls during a benchmark run to use temperature=0
SO THAT re-running the same benchmark on the same model produces the same scores.

**Acceptance criteria:**
```
GIVEN a benchmark run in deterministic mode (default)
WHEN the agent calls any LLM provider
THEN temperature is forced to 0 and any provider-side seed is set to a fixed value
  regardless of what the user's config specifies
```

### US-004: Baseline comparison (memory on vs off)

AS A developer
I WANT to run the same benchmark with memory enabled and disabled
SO THAT I can produce a delta score that quantifies the value of Zeph's memory.

**Acceptance criteria:**
```
GIVEN --baseline flag is provided
WHEN the harness runs
THEN it executes the benchmark twice: once with [memory] enabled=true and once with
  [memory] enabled=false; the output contains a comparison table with per-scenario
  delta scores
```

### US-005: Dataset listing

AS A developer
I WANT to run `zeph bench list`
SO THAT I can see which datasets are available, their descriptions, and whether a
  local cache exists.

**Acceptance criteria:**
```
GIVEN any valid config
WHEN `zeph bench list` is invoked
THEN a table of supported datasets is printed with name, description, scenario count
  (if cached), and cache status
```

---

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|-------------|----------|
| FR-001 | WHEN `zeph bench run --dataset <name>` is invoked THE SYSTEM SHALL download the dataset if not locally cached, run all scenarios sequentially, and write results to the output directory | must |
| FR-002 | WHEN a benchmark scenario begins THE SYSTEM SHALL reset the Qdrant collection and SQLite conversation history for the bench session before feeding the first message | must |
| FR-003 | WHEN deterministic mode is active (default) THE SYSTEM SHALL override temperature to 0.0 and set a fixed seed (0) for all LLM calls in the bench session | must |
| FR-004 | WHEN a scenario response is collected THE SYSTEM SHALL evaluate it against the dataset's ground-truth answer using the dataset's canonical metric (exact match, F1, or LLM judge) | must |
| FR-005 | WHEN a benchmark run completes THE SYSTEM SHALL write a `results.json` file in the leaderboard-compatible schema for the dataset and a human-readable `summary.md` | must |
| FR-006 | WHEN `--baseline` flag is set THE SYSTEM SHALL run the benchmark twice (memory enabled, memory disabled) and include per-scenario and aggregate delta in output | should |
| FR-007 | WHEN `--scenario <id>` flag is provided THE SYSTEM SHALL run only the specified scenario, skipping all others | should |
| FR-008 | WHEN `--resume` flag is provided and a partial `results.json` exists THE SYSTEM SHALL skip already-completed scenarios and continue from where it stopped | should |
| FR-009 | WHEN `zeph bench list` is invoked THE SYSTEM SHALL print a table of supported datasets, their canonical metric, scenario count (if cached), and cache path | must |
| FR-010 | WHEN a dataset is not yet cached THE SYSTEM SHALL print a download prompt and require explicit `--download` flag or `zeph bench download --dataset <name>` before running | should |
| FR-011 | WHEN an LLM call times out or errors during a scenario THE SYSTEM SHALL mark the scenario as `error` in results and continue to the next scenario without aborting the run | must |
| FR-012 | WHEN the harness is active THE SYSTEM SHALL never write to memory collections or SQLite databases used by non-bench agent sessions | must |
| FR-013 | WHEN `--provider <name>` flag is provided THE SYSTEM SHALL use only that named provider from `[[llm.providers]]` for all LLM calls in the run | should |

---

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Isolation | Bench sessions use a dedicated Qdrant collection prefix (`bench_<dataset>_<run_id>`) and a dedicated SQLite DB path (`bench-<run_id>.db`) — never the agent's production collections |
| NFR-002 | Reproducibility | Given identical dataset, model, and config, two runs on the same binary must produce identical per-scenario responses (temperature=0, fixed seed) |
| NFR-003 | Architecture | `BenchmarkChannel` implements `Channel` from `zeph-core` — no changes to agent core, context builder, memory pipeline, or tool executor |
| NFR-004 | Feature gate | All `zeph-bench` code is gated behind the `bench` feature flag; no bench code compiles into default or `full` builds |
| NFR-005 | Dependency minimalism | `zeph-bench` may only depend on crates already in `[workspace.dependencies]` plus one new download/cache utility if not already present |
| NFR-006 | Error handling | `thiserror` typed errors in `zeph-bench`; `anyhow` only at the CLI entry point |
| NFR-007 | Performance | Time-per-scenario overhead (excluding LLM inference) must be under 2 seconds for isolation reset |
| NFR-008 | Output format | `results.json` schema must be a superset of the dataset's official leaderboard schema so it can be directly submitted without transformation |

---

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| `DatasetSpec` | Static description of a supported benchmark | `name`, `description`, `metric`, `download_url`, `schema_version` |
| `Scenario` | One evaluation unit from a dataset | `id`, `turns: Vec<Turn>`, `ground_truth: String`, `metadata: serde_json::Value` |
| `Turn` | Single user/assistant exchange in a scenario | `role`, `content`, `session_id` (for multi-session datasets) |
| `ScenarioResult` | Output of running one scenario | `scenario_id`, `response: String`, `score: f64`, `error: Option<String>`, `token_usage: TokenUsage`, `elapsed_ms: u64` |
| `BenchRun` | Complete run output | `dataset`, `model`, `run_id`, `started_at`, `finished_at`, `results: Vec<ScenarioResult>`, `aggregate: AggregateScore` |
| `AggregateScore` | Summary statistics | `mean_score: f64`, `median_score: f64`, `stddev: f64`, `error_count: u32` |
| `BaselineComparison` | Memory-on vs memory-off delta | `memory_enabled: BenchRun`, `memory_disabled: BenchRun`, `delta_per_scenario: Vec<(String, f64)>`, `aggregate_delta: f64` |

---

## 6. Supported Datasets

| Dataset | Metric | Notes |
|---------|--------|-------|
| `longmemeval` | Exact-match + F1 | Long-term memory recall, multi-hop, temporal. Official eval script bundled. |
| `locomo` | F1 + LLM-judge | Meta 2024. Entity tracking over long conversations. |
| `frames` | Accuracy | Google 2024. Factual retrieval requiring synthesis. |
| `tau-bench` | Task completion rate | Realistic tool-use scenarios. Requires tool executor. |
| `gaia` | Accuracy | HuggingFace leaderboard. Tool use + reasoning. Levels 1/2/3. |

The minimum implementation must include `longmemeval`. Other datasets can be implemented
in follow-up issues against the same spec.

---

## 7. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Dataset not cached | Print descriptive error with `zeph bench download` instruction; exit 1 |
| Qdrant unavailable | Abort run with error; do not leave partial results file |
| LLM call times out in scenario | Mark scenario as `error`, continue to next |
| Ground truth absent for a scenario | Score as 0.0, flag in results with `missing_gt: true` |
| `--resume` but no partial results file | Run from the beginning as if `--resume` were absent |
| Unknown dataset name | Print list of supported datasets and exit 1 |
| Provider name not in `[[llm.providers]]` | Error at startup with clear message; do not start the run |
| Bench run interrupted (SIGINT) | Flush partial results to `results.json` with `status: interrupted`; do not corrupt existing file |
| Output directory does not exist | Create it automatically (single level); error if parent does not exist |

---

## 8. BenchmarkChannel Contract

`BenchmarkChannel` lives in `zeph-bench` and implements `zeph-core::Channel`:

- `recv()`: returns the next turn from the current scenario; returns `None` when
  the scenario is complete
- `send()` / `send_chunk()` / `flush_chunks()`: collect the agent's response text
  for evaluation; do not print to stdout
- `confirm()`: auto-confirms (headless)
- `elicit()`: auto-declines (headless)
- `supports_exit()`: returns `false` (the bench loop, not the agent, controls lifecycle)
- All other methods: no-op (status, typing indicators, diffs are irrelevant in headless mode)

The channel is re-created (not reset) for each scenario so state is completely fresh.

---

## 9. CLI Interface

```
zeph bench list
zeph bench download --dataset <name>
zeph bench run --dataset <name> --output <path>
             [--scenario <id>]
             [--provider <name>]
             [--baseline]
             [--resume]
             [--no-deterministic]
zeph bench show --results <path>
```

`bench` is a top-level subcommand of the main `zeph` CLI (clap, behind `bench` feature).

---

## 10. Output File Layout

```
<output>/
├── results.json        # machine-readable, leaderboard-compatible
├── summary.md          # human-readable per-scenario table + aggregate
└── baseline/           # only when --baseline is set
    ├── memory-on/
    │   ├── results.json
    │   └── summary.md
    └── memory-off/
        ├── results.json
        └── summary.md
```

---

## 11. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | LongMemEval baseline score (memory disabled) | Establishes baseline; no specific target |
| SC-002 | LongMemEval score (memory enabled) | Strictly higher than SC-001 |
| SC-003 | Scenario isolation | Zero cross-scenario memory contamination (verified by running identical scenarios back-to-back and checking score variance) |
| SC-004 | Determinism | Two runs on same model produce identical per-scenario responses |
| SC-005 | Per-scenario overhead (excluding LLM) | < 2 seconds |
| SC-006 | Error resilience | Run completes even if up to 10% of scenarios time out |

---

## 12. Agent Boundaries

### Always (without asking)
- Run `cargo nextest run --features bench` after changes
- Use `bench_` prefixed Qdrant collection names and dedicated SQLite path
- Force temperature=0 in deterministic mode regardless of config
- Mark errored scenarios in output instead of aborting the run

### Ask First
- Adding a new dataset loader (requires review of dataset license and schema)
- Changing `results.json` schema (may break downstream tooling)
- Adding `bench` to any bundle (`full`, `desktop`, etc.)
- Using a net-new dependency not already in `[workspace.dependencies]`

### Never
- Write to or read from the production Qdrant collection (`zeph_memory`, `zeph_skills`, etc.)
- Write to the production SQLite database
- Change any code in `zeph-core`, `zeph-memory`, `zeph-llm`, or other core crates
- Print benchmark responses to stdout (they must only go to the result file)
- Enable `bench` in the `full` feature bundle (bench is a development tool, not a runtime feature)
- Store API keys or dataset credentials inline

---

## 13. Layer Placement

`zeph-bench` is a **Layer 4 consumer** (same tier as `zeph-channels`, `zeph-tui`):
- Depends on: `zeph-core` (Channel trait, Agent), `zeph-memory` (isolation reset),
  `zeph-llm` (provider override), `zeph-config` (provider registry)
- Must NOT be depended on by any other crate

---

## 14. Open Questions

- Which LLM-judge model should be used for LOCOMO/GAIA? Should it be a separate
  `judge_provider` config field or reuse the main provider? [NEEDS CLARIFICATION: confirm
  whether an LLM judge is needed for LongMemEval or if exact-match/F1 suffice]
- LongMemEval session boundaries: does Zeph need to simulate multi-session resets
  mid-scenario, or is each scenario a single continuous session?
  [NEEDS CLARIFICATION: check LongMemEval dataset schema for session_id fields]
- Should `zeph bench run` stream progress to stderr (scenario N/M, current score)?
  This would require a secondary progress channel separate from the BenchmarkChannel.

---

## 15. References

- LongMemEval paper and dataset: https://arxiv.org/abs/2410.10813
- LOCOMO dataset (Meta): https://arxiv.org/abs/2402.12335
- FRAMES (Google): https://arxiv.org/abs/2409.12941
- tau-bench: https://arxiv.org/abs/2406.12045
- GAIA leaderboard: https://huggingface.co/spaces/gaia-benchmark/leaderboard
- Channel trait: `crates/zeph-core/src/channel.rs`
- System invariants: `.local/specs/001-system-invariants/spec.md`
- Feature flag rules: `.local/specs/029-feature-flags/spec.md`
- Multi-model design: `.local/specs/024-multi-model-design/spec.md`
