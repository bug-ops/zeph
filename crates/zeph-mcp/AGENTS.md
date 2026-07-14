# zeph-mcp Guide

MCP client lifecycle, registry, policies, and tool execution bridging live here.

- Start with crate-local checks: `cargo build -p zeph-mcp`, `cargo nextest run -p zeph-mcp`, `cargo clippy -p zeph-mcp --all-targets -- -D warnings`.
- Treat policy enforcement, rate limits, transport setup, and tool exposure as security-sensitive behavior.
- Keep changes isolated to MCP behavior unless shared tool abstractions require coordinated edits.
- Any cache keyed by server-supplied data (tool names, schemas) is attacker-influenced and must be bounded (`lru::LruCache`), never an unbounded process-lifetime `HashMap` — confirmed unbounded-growth bug in `name_referenced_in` (#6296).
- `tool_list_locked` must be released on every cleanup path (disconnect, connect failure, list_tools failure, pre-connect probe-block) — two prior omissions (#6138 OAuth-transport connect, #6143 removal/probe-block) each left a server permanently locked.
- If external behavior changes, update `crates/zeph-mcp/README.md` and the relevant MCP docs.
