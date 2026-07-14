# zeph-index Guide

Code indexing, repo map generation, and code retrieval live here.

- Start with crate-local checks: `cargo build -p zeph-index`, `cargo nextest run -p zeph-index`, `cargo clippy -p zeph-index --all-targets -- -D warnings`.
- Preserve deterministic indexing behavior where possible; retrieval regressions should get tests.
- Be careful with filesystem walking, tree-sitter parsing, and persistence interactions with memory/index stores.
- Extension-to-language mapping is centralized in `zeph_common::treesitter::lang_for_ext` — don't hand-roll a separate mapping in `languages.rs`; extend the shared table in `zeph-common` instead (#5971 consolidated a drifted duplicate between this crate and `zeph-tools`).
- If user-facing behavior changes, update `crates/zeph-index/README.md` and the relevant indexing docs.
