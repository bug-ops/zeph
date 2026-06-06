# zeph-orchestration Guide

Multi-model task orchestration: DAG decomposition, concurrent sub-agent execution, failure propagation, and result synthesis live here.

- Start with crate-local checks: `cargo build -p zeph-orchestration`, `cargo nextest run -p zeph-orchestration`, `cargo clippy -p zeph-orchestration --all-targets -- -D warnings`.
- Multi-model: planner and synthesizer use LLMs — expose `planner_provider` and `synthesizer_provider` config fields referencing `[[llm.providers]]` by name; use the most capable model for planning and reasoning tasks.
- DAG execution must handle partial failure gracefully; do not silently drop sub-task errors.
- LLM serialization gate: changes to task decomposition or result synthesis structs require a live multi-turn session test before merge.
- If external behavior changes, update `crates/zeph-orchestration/README.md` and the relevant orchestration docs.
