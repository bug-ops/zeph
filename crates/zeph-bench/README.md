# zeph-bench

[![Crates.io](https://img.shields.io/crates/v/zeph-bench)](https://crates.io/crates/zeph-bench)
[![docs.rs](https://img.shields.io/docsrs/zeph-bench)](https://docs.rs/zeph-bench)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.95-blue)](https://www.rust-lang.org)

Benchmark harness for evaluating Zeph agent performance on standardized datasets.

## Overview

Provides a CLI-driven benchmark runner that feeds standardized task datasets through the Zeph agent loop and records latency, token usage, and correctness metrics. Integrates with `zeph-core` and `zeph-llm` to exercise the full inference path under controlled conditions.

## Baseline Results

Results on sample datasets (baseline mode — no tools, no memory) with `gpt-5.4-mini`, 2026-04-25:

| Dataset | Scorer | Scenarios | Mean score | Exact match |
|---------|--------|-----------|------------|-------------|
| LOCOMO | Token F1 ≥ 0.5 | 11 | 1.0000 | 11/11 |
| GAIA | GAIA normalized exact | 8 | 1.0000 | 8/8 |

Baseline mode injects a concise-answer system prompt and applies response post-processing
(first-line extraction, markdown strip) before evaluation.

## Quick Start

```bash
# Run GAIA sample
zeph bench run --dataset gaia \
  --data-file path/to/gaia.jsonl \
  --provider my-provider \
  --output results/

# Run a single scenario
zeph bench run --dataset locomo \
  --data-file path/to/locomo.json \
  --scenario s1_0 \
  --output results/

# Resume an interrupted run
zeph bench run --dataset gaia \
  --data-file path/to/gaia.jsonl \
  --resume \
  --output results/
```

The `--provider` flag references a named entry from `[[llm.providers]]` in your config.
If omitted, the default provider is used.

## Installation

```toml
[dependencies]
zeph-bench = "0.20"
```

Or via `cargo add`:

```bash
cargo add zeph-bench
```

**Note:** Requires Rust 1.95 or later.

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

Part of the [Zeph](https://github.com/bug-ops/zeph) workspace.

## License

MIT
