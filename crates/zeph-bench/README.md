# zeph-bench

[![Crates.io](https://img.shields.io/crates/v/zeph-bench)](https://crates.io/crates/zeph-bench)
[![docs.rs](https://img.shields.io/docsrs/zeph-bench)](https://docs.rs/zeph-bench)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.95-blue)](https://www.rust-lang.org)

Benchmark harness for evaluating Zeph agent performance on standardized datasets.

## Overview

Provides a CLI-driven benchmark runner that feeds standardized task datasets through the Zeph agent loop and records latency, token usage, and correctness metrics. Integrates with `zeph-core` and `zeph-llm` to exercise the full inference path under controlled conditions.

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
