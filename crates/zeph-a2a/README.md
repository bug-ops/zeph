# zeph-a2a

A2A protocol client and server for agent-to-agent communication.

## Overview

Implements the Agent-to-Agent (A2A) protocol over JSON-RPC 2.0, enabling Zeph to discover, communicate with, and delegate tasks to remote agents. Feature-gated behind `a2a`; the server component requires the `server` sub-feature.

## Key Modules

- **client** — `A2aClient` for sending tasks and messages to remote agents
- **server** — `A2aServer` exposing an A2A-compliant endpoint (requires `server` feature)
- **card** — `AgentCardBuilder` for constructing agent capability cards
- **discovery** — `AgentRegistry` for agent lookup and registration
- **jsonrpc** — JSON-RPC 2.0 request/response types
- **types** — shared protocol types (Task, Message, Artifact, etc.)
- **error** — `A2aError` error types

## Usage

```toml
# Cargo.toml (workspace root)
zeph-a2a = { path = "crates/zeph-a2a" }
```

Enabled via the `a2a` feature flag on the root `zeph` crate.

## License

MIT
