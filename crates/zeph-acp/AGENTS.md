# zeph-acp Guide

IDE embedding, ACP transport, permissions, and session handling live here.

- Start with crate-local checks: `cargo build -p zeph-acp`, `cargo nextest run -p zeph-acp`, `cargo clippy -p zeph-acp --all-targets -- -D warnings`.
- Be careful with session lifecycle, permission gates, HTTP/WebSocket transport, and filesystem/terminal bridging.
- Changes in ACP behavior should stay aligned with the CLI flags in the root binary and with ACP-related docs.
- If user-visible behavior changes, update `crates/zeph-acp/README.md` and the relevant docs in `docs/src/advanced/acp.md` or `docs/src/guides/ide-integration.md`.
