# zeph-context Guide

Context budget, lifecycle management, compaction strategy, and context assembler live here. This crate is stateless and data-only — it has no dependency on `zeph-core`.

- Start with crate-local checks: `cargo build -p zeph-context`, `cargo nextest run -p zeph-context`, `cargo clippy -p zeph-context --all-targets -- -D warnings`.
- Read `specs/004-memory/004-16-memory-type-aware-retrieval.md` (MemGuard type-aware retrieval composition, #6226/#6086) before changing `assembler.rs`'s `schedule_context_fetchers` type-gating logic. `enabled = false` (default) and empty `default_compose_types` must both stay byte-for-byte no-ops (every fetcher runs unfiltered); NEVER gate `fetch_corrections`/`BehavioralRule` behind the active `FunctionalType` set. In-code comments cite this as "spec 064" — a naming collision with the permanent `/specs/064-durable-execution/` slot; the real spec is at the path above.
- LLM serialization gate: changes to context assembly structs (`MessagePart`, `Message`, assembled `messages` array) require a live API session test before merge — verify no 400/422 errors and a well-formed payload in the debug dump.
- Multi-model: compaction uses an LLM — expose a `compaction_provider` config field referencing `[[llm.providers]]` by name; never hardcode a model.
- Keep `IndexAccess` trait contract stable; callers in `zeph-core` implement it and breakage is silent at the trait boundary.
- If external behavior changes, update `crates/zeph-context/README.md` and the relevant context docs.
