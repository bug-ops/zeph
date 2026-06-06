# zeph-experiments Guide

Experiment engine for A/B testing adaptive agent behavior and hyperparameter tuning (temperature, top-p, retrieval depth, etc.) via an LLM-as-judge evaluation loop lives here.

- Start with crate-local checks: `cargo build -p zeph-experiments`, `cargo nextest run -p zeph-experiments`, `cargo clippy -p zeph-experiments --all-targets -- -D warnings`.
- Multi-model: LLM-as-judge and hypothesis scoring expose `*_provider` config fields referencing `[[llm.providers]]` by name — never hardcode a model.
- Keep experiment runs reproducible: seed random state, fix dataset splits, and record all config parameters in experiment artifacts.
- If user-facing behavior changes, update `crates/zeph-experiments/README.md` and the relevant docs.
