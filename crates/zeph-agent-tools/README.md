# zeph-agent-tools

[![Crates.io](https://img.shields.io/crates/v/zeph-agent-tools)](https://crates.io/crates/zeph-agent-tools)
[![docs.rs](https://img.shields.io/docsrs/zeph-agent-tools)](https://docs.rs/zeph-agent-tools)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.97-blue)](https://www.rust-lang.org)

Doom-loop detection utilities for the Zeph tool dispatch loop.

## Key types

| Type / Function | Description |
|-----------------|-------------|
| `doom_loop_hash` | Hash message content with volatile tool IDs normalized out |

## Usage

```rust
use zeph_agent_tools::doom_loop_hash;

// Volatile tool IDs are normalized before hashing so repeated responses
// with different IDs still produce the same hash.
let h1 = doom_loop_hash("[tool_result: abc123] same output");
let h2 = doom_loop_hash("[tool_result: xyz789] same output");
assert_eq!(h1, h2);
```

## Architecture

`zeph-agent-tools` has no dependencies on other workspace crates. `zeph-core` calls
`doom_loop_hash` from the tool dispatch loop to detect repeated tool-call cycles.

> **Note:** This crate previously carried a sealed `AgentChannel` dispatcher-extraction trait
> (issue #3516) with no implementors anywhere in the workspace; it was removed as dead code
> (issue #6480). Reviving that dispatcher-extraction plan needs a fresh tracking issue.

## License

Licensed under either of [MIT](../../LICENSE) or [Apache License, Version 2.0](../../LICENSE-APACHE) at your option.
