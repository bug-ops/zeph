# zeph-agent-tools

[![Crates.io](https://img.shields.io/crates/v/zeph-agent-tools)](https://crates.io/crates/zeph-agent-tools)
[![docs.rs](https://img.shields.io/docsrs/zeph-agent-tools)](https://docs.rs/zeph-agent-tools)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)

Agent tool dispatcher for Zeph: provides the `AgentChannel` sealed trait, borrowed event
carriers, and doom-loop detection utilities used by the tool dispatch loop in `zeph-core`.

## Key types

| Type / Function | Description |
|-----------------|-------------|
| `AgentChannel` | Sealed async sink trait the tool dispatcher uses to emit events to the user surface |
| `ToolEventStart<'a>` | Borrowed payload for a tool-start event (zero-copy through the seam) |
| `ToolEventOutput<'a>` | Borrowed payload for a tool-output event, including body and error flag |
| `ChannelSinkError` | Concrete error returned by every `AgentChannel` method |
| `ToolDispatchError` | Error enum covering LLM, tool, MCP, timeout, and channel failures |
| `doom_loop_hash` | Hash message content with volatile tool IDs normalized out |
| `Sealed` | Marker trait that prevents external `AgentChannel` implementations |

## Usage

### Doom-loop detection

```rust
use zeph_agent_tools::doom_loop_hash;

// Volatile tool IDs are normalized before hashing so repeated responses
// with different IDs still produce the same hash.
let h1 = doom_loop_hash("[tool_result: abc123] same output");
let h2 = doom_loop_hash("[tool_result: xyz789] same output");
assert_eq!(h1, h2);
```

### Implementing `AgentChannel` (zeph-core only)

```rust,no_run
// The trait is sealed — only zeph-core can implement it.
// zeph-core provides: impl<C: Channel> AgentChannel for AgentChannelView<'_, C>
//
// Dispatcher usage (generic, no Box/dyn):
async fn dispatch<Ch: zeph_agent_tools::AgentChannel>(ch: &mut Ch, text: &str) {
    ch.send(text).await.ok();
}
```

## Architecture

`zeph-agent-tools` does **not** depend on `zeph-core` or `zeph-channels`. It defines its own
minimal `AgentChannel` trait (sealed via `Sealed`) which `zeph-core` implements through a local
adapter type `AgentChannelView<'a, C>`. This avoids the circular dependency that would arise
from referencing `zeph_core::channel::Channel` directly from inside the dispatcher crate.

`zeph-core` depends on this crate; downstream channels (`zeph-channels`) do not.

> **Note:** This crate is Phase 2 scaffolding (issue #3516). The `AgentChannel` trait and
> borrowed event carriers are complete. Full `ToolDispatcher` extraction from `zeph-core` is
> tracked as a follow-up once the persistence extraction (#3515) lands and integration tests
> are stable.

## License

MIT
