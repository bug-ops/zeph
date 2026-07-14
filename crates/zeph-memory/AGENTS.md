# zeph-memory Guide

Conversation persistence, embeddings, semantic recall, document ingestion, and graph memory live here.

- Start with crate-local checks: `cargo build -p zeph-memory`, `cargo nextest run -p zeph-memory`, `cargo clippy -p zeph-memory --all-targets -- -D warnings`.
- Be careful with persistence schema changes, token counting, vector-store behavior, and retention/eviction logic.
- `[memory.type_aware_compose]` (MemGuard, spec `004-memory/004-16-memory-type-aware-retrieval.md`, #6226) gates five of the six `ContextAssembler` fetchers by functional memory type at retrieval time. Corrections are exempt and MUST stay unconditionally composed regardless of the active type set — that exemption is safety-critical, never gate it.
- Cross-backend correctness: never use a raw `LIMIT -1` SQLite unlimited-sentinel — it crashes on Postgres. Use `zeph_db::limit_clause()` (added after #6121 broke `SessionStore::list` and sibling `list_*` helpers) and decode `created_at`/`updated_at` via the shared timestamp helper, not raw `String`, since Postgres uses `TIMESTAMPTZ`.
- Control-char stripping and secret-prefix/Bearer/JWT redaction must go through `zeph_common::sanitize` / `zeph_common::secrets` — don't hand-roll a stripper or prefix list here; a divergent, weaker stripper in the graph community summarizer reintroduced a newline/tab prompt-injection vector after the #6091 consolidation (fixed by #6135).
- Embedding dimension mismatches are a recurring source of bugs: whenever the embedding model or vector collection config changes, verify that stored and query vector dimensions match before running tests.
- Multi-model: summarization, compaction, and graph extraction each use an LLM — expose `*_provider` fields referencing `[[llm.providers]]` names; never hardcode a model.
- Memory-related bug fixes should get regression coverage near the changed code or in crate tests.
- If external behavior changes, update `crates/zeph-memory/README.md` and the relevant memory docs.
