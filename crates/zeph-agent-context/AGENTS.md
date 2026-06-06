# zeph-agent-context Guide

Context-assembly service (`ContextService`): system prompt rebuild, memory injection, semantic recall, summarization, and compaction. A stateless façade extracted from `Agent<C>` so context-assembly edits do not recompile the tool dispatcher or persistence layer.

- Start with crate-local checks: `cargo build -p zeph-agent-context`, `cargo nextest run -p zeph-agent-context`, `cargo clippy -p zeph-agent-context --all-targets -- -D warnings`.
- Read `specs/021-zeph-context/spec.md` before changing assembly, budget, or compaction behavior; honor its `## Key Invariants` and `NEVER` sections.
- Core invariant: this crate MUST NOT depend on `zeph-core`. The decoupling is the whole point — never add a `zeph-core` dependency to satisfy a borrow.
- Keep the borrow-lens views (`MessageWindowView`, `ContextAssemblyView`, `ContextSummarizationView`) narrow; `zeph-core` constructs them from `Agent` field projections.
- Features: `sqlite` (default) / `postgres` forwarded to `zeph-memory`, plus `index` for `IndexAccess` integration. Run `cargo nextest run -p zeph-agent-context --features index` when touching index-backed assembly views.
- LLM serialization gate: summarization and compaction build LLM request payloads — changes to `summarization/` or `compaction.rs` require a live API session test (no 400/422, well-formed `messages` array in the debug dump) before merge.
- Multi-model: summarization and compaction call an LLM — resolve the provider via a `*_provider` field referencing a named `[[llm.providers]]` entry; never hardcode a model.
