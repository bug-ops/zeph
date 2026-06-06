# zeph-bench Guide

Benchmark harness and `zeph bench` CLI subcommand for evaluating agent performance on standardized datasets (LOCOMO, FRAMES, GAIA, etc.) live here.

- Start with crate-local checks: `cargo build -p zeph-bench`, `cargo nextest run -p zeph-bench`, `cargo clippy -p zeph-bench --all-targets -- -D warnings`.
- Keep evaluation logic reproducible and deterministic; non-deterministic results make regressions invisible.
- Multi-model: LLM-as-judge evaluation exposes a `*_provider` config field referencing `[[llm.providers]]` by name — never hardcode a model.
- If user-facing behavior or dataset support changes, update `crates/zeph-bench/README.md` and the relevant benchmark docs.
