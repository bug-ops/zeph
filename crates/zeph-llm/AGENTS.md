# zeph-llm Guide

Provider implementations, orchestration, routing, and inference behavior live here.

- Start with crate-local checks: `cargo build -p zeph-llm`, `cargo nextest run -p zeph-llm`, `cargo clippy -p zeph-llm --all-targets -- -D warnings`.
- Changes here are high impact: preserve provider contracts, streaming behavior, retries, and schema extraction semantics unless explicitly changing them.
- `LlmProvider::name()` (the config/instance name from `[[llm.providers]]`) and `model_identifier()` / `effective_model_identifier()` (the actual model id pattern-matched by `is_reasoning_model()` and reasoning/routing checks) are distinct — conflating them has recurred 3+ times (#5879, #6182, #6190). Any new provider wrapper (masking, router, triage, candle) needs an explicit `effective_model_identifier()` override or the check silently becomes unreachable.
- Claude no-prefill gate is compile-time enforced: `RequestBody`/`ToolRequestBody`/`VisionRequestBody`/`TypedToolRequestBody.messages` only accept `GatedStructuredHistory`/`GatedPlainHistory`, constructible only via `ClaudeProvider::structured_history`/`plain_history`. Never call `split_messages`/`split_messages_structured` directly from a new request-construction path — that reintroduces the prefill bug fixed five times (#5903/#6145/#6146/#6154/#6158).
- `ImageData` (vision/MCP image passthrough, spec-072) has a hand-written `Debug` impl redacting raw bytes to `[image: <mime>, N bytes]` — never restore `#[derive(Debug)]` on it or any future media-carrying struct.
- LLM serialization gate: any change to `claude.rs`, `openai.rs`, `ollama.rs`, `compatible.rs`, or any `#[derive(Serialize, Deserialize)]` struct on the request/response path requires a live multi-turn + tool-call session test before merge.
- Multi-model: all provider backends resolve through the `[[llm.providers]]` registry by name; subsystems reference providers via `*_provider` fields — never inline model strings.
- Keep model/provider docs and config examples in sync with behavior.
- If external behavior changes, update `crates/zeph-llm/README.md` and the relevant provider docs.
