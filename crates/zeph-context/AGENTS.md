# zeph-context Guide

Context budget, lifecycle management, compaction strategy, and context assembler live here. This crate is stateless and data-only — it has no dependency on `zeph-core`.

- Start with crate-local checks: `cargo build -p zeph-context`, `cargo nextest run -p zeph-context`, `cargo clippy -p zeph-context --all-targets -- -D warnings`.
- LLM serialization gate: changes to context assembly structs (`MessagePart`, `Message`, assembled `messages` array) require a live API session test before merge — verify no 400/422 errors and a well-formed payload in the debug dump.
- Multi-model: compaction uses an LLM — expose a `compaction_provider` config field referencing `[[llm.providers]]` by name; never hardcode a model.
- Keep `IndexAccess` trait contract stable; callers in `zeph-core` implement it and breakage is silent at the trait boundary.
- If external behavior changes, update `crates/zeph-context/README.md` and the relevant context docs.
