# zeph-mcp Guide

MCP client lifecycle, registry, policies, and tool execution bridging live here.

- Start with crate-local checks: `cargo build -p zeph-mcp`, `cargo nextest run -p zeph-mcp`, `cargo clippy -p zeph-mcp --all-targets -- -D warnings`.
- Treat policy enforcement, rate limits, transport setup, and tool exposure as security-sensitive behavior.
- Keep changes isolated to MCP behavior unless shared tool abstractions require coordinated edits.
- If external behavior changes, update `crates/zeph-mcp/README.md` and the relevant MCP docs.
