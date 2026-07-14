# zeph-agent-persistence Guide

Persistence service (`PersistenceService`): loads conversation history from and writes messages to `SemanticMemory` (SQLite + Qdrant), plus tool-pair sanitization, embedding decisions, and graph-extraction configuration.

- Start with crate-local checks: `cargo build -p zeph-agent-persistence`, `cargo nextest run -p zeph-agent-persistence`, `cargo clippy -p zeph-agent-persistence --all-targets -- -D warnings`.
- Read `specs/057-agent-persistence/spec.md` before changing history loading, persistence, or extraction enqueueing.
- Read `specs/068-session-persistence/spec.md` (INV-SP-1..4) before changing session-open/hydration behavior — `hydrate_from_event_log` (`hydrate.rs`) is the single sanctioned pipeline for ACP resume/load/fork, CLI `sessions resume`, and `/conv resume`; its `messages` fold MUST go through `zeph_session::ReplayEngine::replay`'s bounded/chunked reader, never `fold()` on a cloned event `Vec` (regression fixed in #5861 — that call doubled peak memory on every resume path).
- Core invariant: this crate MUST NOT depend on `zeph-core`. Keep the borrow-lens views (`MemoryPersistenceView`, `SecurityView`, `MetricsView`) narrow; `zeph-core` builds them from `Agent` fields.
- Ephemeral media invariant (spec-072 §4 C1): callers into this crate's persistence path always receive `Image`-free `MessagePart` slices — `zeph-core`'s `Agent::persist_message` strips `MessagePart::Image` before invoking `PersistMessageRequest`/`svc.persist_message`. Do not add code here that assumes `Image` parts need filtering again downstream, and do not weaken the assumption that this crate never itself sees an unstripped slice.
- Features: `sqlite` (default) / `postgres` forwarded to `zeph-memory` — verify behavior is identical across both backends; silent divergence is a first-class bug.
- LLM serialization gate: tool-pair sanitization (`sanitize.rs`, `request.rs`) controls whether `tool_use`/`tool_result` blocks are well-formed. A malformed pairing causes hard LLM 400/422 errors that unit tests do not catch — changes here require a live multi-turn + tool-call session test before merge.
- Embedding dimension mismatches are a recurring source of bugs: whenever the embedding model or vector collection config changes, verify stored and query vector dimensions match before running tests.
- Multi-model: graph extraction and embedding each call an LLM/embedder — resolve via `*_provider` fields referencing named `[[llm.providers]]` entries; never hardcode a model.
