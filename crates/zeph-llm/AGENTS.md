# zeph-llm Guide

Provider implementations, orchestration, routing, and inference behavior live here.

- Start with crate-local checks: `cargo build -p zeph-llm`, `cargo nextest run -p zeph-llm`, `cargo clippy -p zeph-llm --all-targets -- -D warnings`.
- Changes here are high impact: preserve provider contracts, streaming behavior, retries, and schema extraction semantics unless explicitly changing them.
- LLM serialization gate: any change to `claude.rs`, `openai.rs`, `ollama.rs`, `compatible.rs`, or any `#[derive(Serialize, Deserialize)]` struct on the request/response path requires a live multi-turn + tool-call session test before merge.
- Multi-model: all provider backends resolve through the `[[llm.providers]]` registry by name; subsystems reference providers via `*_provider` fields — never inline model strings.
- Keep model/provider docs and config examples in sync with behavior.
- If external behavior changes, update `crates/zeph-llm/README.md` and the relevant provider docs.
