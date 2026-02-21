# zeph-a2a

[![Crates.io](https://img.shields.io/crates/v/zeph-a2a)](https://crates.io/crates/zeph-a2a)
[![docs.rs](https://img.shields.io/docsrs/zeph-a2a)](https://docs.rs/zeph-a2a)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)

A2A protocol client and server with agent discovery for Zeph.

## Overview

Implements the Agent-to-Agent (A2A) protocol over JSON-RPC 2.0, enabling Zeph to discover, communicate with, and delegate tasks to remote agents. Feature-gated behind `a2a`; the server component requires the `server` sub-feature.

## Key Modules

- **client** — `A2aClient` for sending tasks and messages to remote agents
- **server** — `A2aServer` exposing an A2A-compliant endpoint with `ProcessorEvent` streaming via `mpsc::Sender` (requires `server` feature)
- **card** — `AgentCardBuilder` for constructing agent capability cards
- **discovery** — `AgentRegistry` for agent lookup and registration
- **jsonrpc** — JSON-RPC 2.0 request/response types
- **types** — shared protocol types (Task, Message, Artifact, etc.)
- **error** — `A2aError` error types

## Installation

```bash
cargo add zeph-a2a
```

Enabled via the `a2a` feature flag on the root `zeph` crate.

## License

MIT
