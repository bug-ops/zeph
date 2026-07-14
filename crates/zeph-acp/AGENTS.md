# zeph-acp Guide

IDE embedding, ACP transport, permissions, and session handling live here.

- Start with crate-local checks: `cargo build -p zeph-acp`, `cargo nextest run -p zeph-acp`, `cargo clippy -p zeph-acp --all-targets -- -D warnings`.
- Read `specs/013-acp/spec.md` before changing session lifecycle, permission gates, or auth; honor its `## Key Invariants` section.
- Be careful with session lifecycle, permission gates, HTTP/WebSocket transport, and filesystem/terminal bridging.
- `session/delete` (and any future permanent-deletion path) must purge the persisted store row (`store.delete_acp_session_for_owner`) in addition to the in-memory entry — a deleted session must never resurrect via `session/load`/`session/resume` (#6271, #6284).
- An empty-string bearer/vault token must never be treated as "no auth configured" — `BearerAuthLayer::new` filters these out at construction as defense-in-depth even if an upstream caller fails to normalize one (#6282).
- Changes in ACP behavior should stay aligned with the CLI flags in the root binary and with ACP-related docs.
- If user-visible behavior changes, update `crates/zeph-acp/README.md` and the relevant docs in `docs/src/advanced/acp.md` or `docs/src/guides/ide-integration.md`.
