---
aliases:
  - Interop Protocol Gaps
  - Protocol Gap Analysis
tags:
  - sdd
  - spec
  - protocol
  - interop
  - research
created: 2026-04-17
status: approved
related:
  - "[[MOC-specs]]"
  - "[[008-mcp/spec]]"
  - "[[013-acp/spec]]"
  - "[[014-a2a/spec]]"
---

# Spec 045: Agent Interoperability Protocol Gap Analysis

## Purpose

This spec documents capability gaps between the four major agent interoperability protocols
surveyed in arXiv:2505.02279 — MCP, ACP, A2A, and ANP — and Zeph's current implementation.
It provides architectural guidance on when to use each protocol for new features and records
ANP's disposition as a P4 research item.

## Source

- Survey: arXiv:2505.02279 — "A Survey of Agent Interoperability Protocols"
- Zeph ACP baseline: `agent-client-protocol = "0.10"` (Cargo.toml workspace)
- Zeph MCP baseline: `rmcp` crate, `specs/008-mcp/spec.md`
- Zeph A2A baseline: `crates/zeph-a2a/`, `specs/014-a2a/spec.md`
- Zeph ACP baseline: `crates/zeph-acp/`, `specs/013-acp/spec.md`

## Capability Matrix

Columns:
- **MCP** — Model Context Protocol (Anthropic / community)
- **ACP** — Agent Client Protocol (IBM / BeeAI, `agent-client-protocol` crate)
- **A2A** — Agent-to-Agent Protocol (Google / A2A project)
- **ANP** — Agent Network Protocol (ANP project)
- **Zeph-current** — Zeph's implementation status as of 2026-04-17

| Capability | MCP | ACP | A2A | ANP | Zeph-current | Notes |
|---|---|---|---|---|---|---|
| **Discovery** | Partial — no standard well-known | Server registration only | `/.well-known/agent.json` | DID-based, decentralized | **MCP**: implemented via server config. **ACP**: not in spec. **A2A**: implemented (`AgentRegistry`, `crates/zeph-a2a/src/discovery.rs`). **ANP**: missing. | A2A discovery is the most complete in Zeph; MCP relies on static config |
| **Tool exposure** | Full — `tools/list`, `tools/call` | Full — tool forwarding via `mcp_passthrough` | Partial — via task artifacts | Full | **MCP**: fully implemented (`zeph-mcp`). **ACP**: implemented via `mcp_bridge.rs`. **A2A**: partial — tools are not first-class; results returned as artifacts. **ANP**: not implemented. | MCP is the primary tool mechanism in Zeph |
| **Streaming** | Partial — SSE via HTTP transport | Full — SSE + WebSocket | Full — SSE `TaskEventStream` | Full | **MCP**: SSE on HTTP transport only. **ACP**: SSE (`acp-http`) + WebSocket. **A2A**: SSE streaming implemented (`stream_message`). **ANP**: not implemented. | All three implemented protocols support streaming; ANP absent |
| **Auth / scope** | Bearer token (transport-level) | Bearer token + per-tool rules | Bearer token + IBCT (Zeph ext.) | DID-based, cryptographic | **MCP**: bearer auth in rmcp transport. **ACP**: `AcpPermissionGate` + bearer. **A2A**: IBCT (HMAC-SHA256, Zeph extension). **ANP**: not implemented. | Zeph's IBCT is a proprietary extension on A2A, not in upstream spec |
| **Multi-turn task** | No — request-response only | Yes — session fork/resume | Yes — `input-required` state, task re-submission | Yes | **MCP**: single-turn only; no session state. **ACP**: session fork/resume implemented (unstable features). **A2A**: `input-required` lifecycle implemented. **ANP**: not implemented. | ACP and A2A both support multi-turn; MCP is stateless by design |
| **Federation / peering** | No | No | Partial — via agent discovery + delegation | Yes — designed for decentralized mesh | **MCP**: not applicable. **ACP**: not in spec. **A2A**: partial federation via `AgentRegistry` + task delegation. **ANP**: not implemented. | A2A federation is the recommended path for Zeph's orchestration use cases |
| **IDE embedding** | No | Yes — primary use case (ACP) | No | No | **ACP**: fully implemented (`zeph-acp`, feature `ide`). Others not applicable. | ACP is the sole IDE protocol; MCP tools are passed through via `mcp_bridge.rs` |
| **Capability re-negotiation** | No — capabilities fixed at handshake | **Tested? Unverified** — `agent-client-protocol` 0.10 includes session-level capability advertisement; dynamic re-negotiation during an active session is not confirmed tested in Zeph | No — `AgentCard` is static, re-discovery required | Yes — renegotiation is a first-class DID operation | **MCP**: not supported. **ACP**: protocol field present in 0.10 but Zeph integration not verified end-to-end. **A2A**: not supported natively; would require re-discovery. **ANP**: not implemented. | ACP's re-negotiation field is the only viable path without ANP; needs integration test (P3) |

### Matrix Legend

- **Implemented** — feature present in Zeph codebase and covered by tests
- **Partial** — feature partially present; gaps documented in Notes column
- **Missing** — protocol supports the capability but Zeph does not implement it
- **Not applicable** — protocol does not define this capability by design
- **Tested? Unverified** — Zeph has code touching the capability but no confirmed end-to-end test

## Protocol Positioning: When to Use Each

### MCP — Use for tool exposure to the LLM

MCP is Zeph's primary mechanism for exposing external tools to the language model. Use MCP when:
- Integrating new external services as tools callable by the model
- Exposing resources (files, databases, APIs) in a standard way
- Building tool registries for Qdrant-backed semantic discovery

Do **not** use MCP for agent-to-agent delegation or session state.

### ACP — Use for IDE integration

ACP is the IDE integration protocol. Use ACP when:
- Adding new IDE-facing capabilities (file access, terminal forwarding, model switching)
- Extending the permission model for new tool categories
- Supporting new IDE clients (e.g., JetBrains, Neovim)

Do **not** use ACP for agent orchestration — `[orchestration]` is explicitly ignored in ACP sessions per `specs/013-acp/spec.md`.

### A2A — Use for agent-to-agent delegation

A2A is the multi-agent delegation protocol. Use A2A when:
- The orchestrator needs to delegate a subtask to a remote agent
- Building agent federations or hierarchical agent networks
- Exposing Zeph as a callable agent to other orchestration frameworks

A2A is the **recommended protocol for orchestration interop** in Zeph's current architecture. Centralized A2A is sufficient for all identified use cases (see ANP disposition below).

### ANP — P4 research, do not implement

ANP (Agent Network Protocol) provides decentralized, DID-based agent discovery and capability re-negotiation. It is architecturally sound for open, permissionless agent meshes.

**Disposition: P4 research.** Reasons:
1. Centralized A2A via `AgentRegistry` covers all current Zeph orchestration use cases.
2. DID infrastructure adds operational complexity with no near-term user benefit.
3. ANP's decentralized model is only valuable when Zeph agents need to discover and trust arbitrary third-party agents — not a current requirement.
4. Revisit when: (a) Zeph is deployed in multi-tenant or marketplace environments, or (b) A2A centralized discovery becomes a bottleneck.

## ACP: Current State vs. Survey Definition

Zeph uses `agent-client-protocol = "0.10"` (pinned in workspace `Cargo.toml`).

The arXiv:2505.02279 survey describes ACP's capability advertisement and re-negotiation model.
Zeph's implementation baseline is **0.10.3** (the patch available at spec creation time).

**Known gaps vs. survey definition:**

| Gap | Severity | Notes |
|---|---|---|
| Capability re-negotiation | Unknown | `agent-client-protocol` 0.10 includes capability fields in the session handshake. Dynamic re-negotiation during an active session is not confirmed tested in Zeph's `AcpSessionManager`. Needs an integration test. |
| Elicitation (`unstable-elicitation`) | Partial | Feature flag enabled in `acp` feature bundle; not fully exercised in tests. |
| Session logout (`unstable-logout`) | Partial | Feature flag enabled; not exercised in integration tests. |

**Version note:** If `agent-client-protocol` 0.11.x becomes available on crates.io, evaluate for capability re-negotiation improvements before upgrading. Track as P3 follow-up (see below).

### P3 Follow-up: ACP capability re-negotiation integration test

**Do not file a GitHub issue** — tracked here as a spec-level note.

When `agent-client-protocol` 0.11.x is available:
1. Grep `Cargo.toml` for `agent-client-protocol` version.
2. Review 0.11 changelog for re-negotiation API changes.
3. Write an integration test: start ACP session, trigger capability re-negotiation mid-session, verify `AcpSessionManager` reflects the updated capabilities.
4. Update the `Capability re-negotiation` row in the matrix above from `Tested? Unverified` to `Implemented` or `Partial`.

## Cross-references

- ACP spec: `specs/013-acp/spec.md` — session model, permission gate, config coverage
- A2A spec: `specs/014-a2a/spec.md` — IBCT, discovery, task lifecycle
- MCP spec: `specs/008-mcp/spec.md` — tool registry, semantic discovery, server lifecycle
- Orchestration spec: `specs/009-orchestration/spec.md` — DAG planner, VeriMAP, cascade defense

## Key Invariants

- ANP is P4 research — do NOT implement without an explicit architectural decision recorded in this spec.
- ACP capability re-negotiation must be tested end-to-end before marking as `Implemented` in the matrix.
- Any new protocol integration must be added to this matrix before merging.
- Protocol selection for new features must follow the "When to Use Each" guidance above; deviations require a spec update.
