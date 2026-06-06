# zeph-index Guide

Code indexing, repo map generation, and code retrieval live here.

- Start with crate-local checks: `cargo build -p zeph-index`, `cargo nextest run -p zeph-index`, `cargo clippy -p zeph-index --all-targets -- -D warnings`.
- Preserve deterministic indexing behavior where possible; retrieval regressions should get tests.
- Be careful with filesystem walking, tree-sitter parsing, and persistence interactions with memory/index stores.
- If user-facing behavior changes, update `crates/zeph-index/README.md` and the relevant indexing docs.
