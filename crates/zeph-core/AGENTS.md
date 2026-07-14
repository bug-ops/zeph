# zeph-core Guide

Core agent orchestration, config, context building, sanitization, and subagent plumbing live here.

- Start with crate-local checks: `cargo build -p zeph-core`, `cargo nextest run -p zeph-core`, `cargo clippy -p zeph-core --all-targets -- -D warnings`.
- Prefer minimal, well-contained changes; `zeph-core` is the highest-coupling crate in the workspace.
- Any change here may require follow-up updates in config, CLI wiring, docs, and integration tests.
- Be especially careful with context assembly, sanitization, config loading, and feature-gated surfaces.
- LLM serialization gate: changes to context assembly (`src/agent/context/`), `MessagePart`, `Message`, or any struct in LLM request/response paths require a live API session test before merge — verify no 400/422 errors and a well-formed `messages` array in the debug dump.
- Ephemeral media invariant (spec-072 §4 C1): `MessagePart::Image` is current-turn-only and MUST NEVER reach SQLite `parts_json`, the Qdrant embed path, or the durable JSONL session log. `Agent::persist_message` (`src/agent/persistence/store.rs`) strips all `Image` parts once, above both persistence writers, before calling `sink.record_message` and building `PersistMessageRequest`; the in-memory `Message` keeps its `Image` parts for the current turn's provider request. Any new persistence writer or call path into `persist_message` must go through this strip point, not bypass it.
- Multi-model: every subsystem calling an LLM must expose a `*_provider` config field referencing a named entry in `[[llm.providers]]`; never hardcode a model name.
- TUI: every background operation triggered from core must surface a visible spinner/status message in the TUI layer.
