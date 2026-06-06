# zeph-memory Guide

Conversation persistence, embeddings, semantic recall, document ingestion, and graph memory live here.

- Start with crate-local checks: `cargo build -p zeph-memory`, `cargo nextest run -p zeph-memory`, `cargo clippy -p zeph-memory --all-targets -- -D warnings`.
- Be careful with persistence schema changes, token counting, vector-store behavior, and retention/eviction logic.
- Embedding dimension mismatches are a recurring source of bugs: whenever the embedding model or vector collection config changes, verify that stored and query vector dimensions match before running tests.
- Multi-model: summarization, compaction, and graph extraction each use an LLM — expose `*_provider` fields referencing `[[llm.providers]]` names; never hardcode a model.
- Memory-related bug fixes should get regression coverage near the changed code or in crate tests.
- If external behavior changes, update `crates/zeph-memory/README.md` and the relevant memory docs.
