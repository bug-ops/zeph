# zeph-a2a Guide

Protocol client/server work for A2A lives here.

- Start with crate-local checks: `cargo build -p zeph-a2a`, `cargo nextest run -p zeph-a2a`, `cargo clippy -p zeph-a2a --all-targets -- -D warnings`.
- Keep changes isolated to A2A transport, discovery, JSON-RPC, and server behavior unless a shared API truly requires cross-crate edits.
- Preserve protocol-facing behavior unless the task explicitly calls for a spec-aligned change.
- If external behavior changes, update `crates/zeph-a2a/README.md` and the relevant docs in `docs/src/advanced/a2a.md`.
