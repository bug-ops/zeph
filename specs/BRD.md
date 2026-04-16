---
aliases:
  - Zeph BRD
  - Zeph Business Requirements
tags:
  - brd
  - ai-agent
  - rust
  - status/draft
created: 2026-04-13
project: "Zeph"
status: draft
related:
  - "[[SRS]]"
  - "[[NFR]]"
  - "[[constitution]]"
  - "[[MOC-specs]]"
---

# Zeph: Business Requirements Document

> [!abstract]
> Zeph is a lightweight, open-source Rust AI agent with hybrid multi-provider
> inference, a skills-first architecture, and semantic memory. This BRD defines
> what Zeph is, why it exists, and what success looks like for its primary personas.
> It serves as the business-level input to [[SRS]] and [[NFR]].

## Executive Summary

Zeph is an open-source, self-hostable AI agent written in Rust that integrates
multiple LLM backends (Ollama, Claude, OpenAI, HuggingFace/candle) behind a
unified interface. It gives developers, power users, and teams a programmable,
privacy-respecting agent that runs locally or as a lightweight service, with
no cloud lock-in. Skills-based extensibility lets users teach the agent domain
knowledge without retraining a model. Semantic memory with SQLite and Qdrant
gives the agent long-term recall across sessions. All I/O is handled through
interchangeable channels (CLI, TUI, Telegram), and secrets are managed
exclusively via an age-encrypted vault. Zeph targets pre-v1.0 active
development and is not yet production-hardened for multi-tenant deployments.

---

## Problem Statement

### What problem exists today?

Existing AI agents are predominantly:

1. **Cloud-only** — locked to a single provider's API, data leaves the machine.
2. **Monoglot** — written in Python or TypeScript, making them difficult to
   embed in Rust toolchains or operate with minimal runtime overhead.
3. **Opaque** — behaviour is not auditable; there is no structured way to
   extend the agent with domain knowledge beyond system-prompt hacking.
4. **Memory-poor** — most agents have no durable cross-session memory; every
   conversation starts cold.
5. **Single-channel** — built for one interface (web chat or CLI), not portable
   to Telegram, TUI dashboards, or programmatic APIs.

### Who experiences this problem?

- Rust developers who need an agent embedded in their workflow without a Python
  dependency.
- Privacy-aware power users who cannot send proprietary data to a cloud API.
- Small teams that want a lightweight, self-hosted agent without infrastructure
  complexity.

### What is the impact of not solving it?

Without Zeph, users must choose between heavyweight Python frameworks (LangChain,
AutoGPT), cloud-only products (ChatGPT Plugins, Claude Projects), or writing
agent glue code from scratch — all of which either leak data, require a Python
runtime, or provide no semantic memory or skill governance.

### Current workarounds

- Pasting context manually into each conversation.
- Shell scripts wrapping raw `curl` calls to OpenAI.
- Python wrappers (LangChain) with substantial runtime overhead.
- Local Ollama with no skill management or memory.

> [!warning] Assumptions
> - The primary deployment target is a developer's local machine or a small
>   VPS, not a multi-tenant SaaS platform.
> - Users are comfortable with TOML configuration and a terminal interface.
> - The age vault is the only accepted secret storage; `.env` files are out
>   of scope.

---

## Target Users

### Primary Users

| Persona | Description | Primary Goal | Key Pain Point |
|---------|-------------|-------------|----------------|
| **CLI Developer** | Rust/systems developer using Zeph as a daily coding assistant | Integrate an agent into shell workflows, pipe output, use slash commands | Existing agents require Python or cloud APIs |
| **Power User (TUI)** | Technical user running Zeph full-time in a terminal TUI | Monitor agent state, context, metrics, and memory in real time | No visibility into what the agent is doing |
| **Remote User (Telegram)** | Developer or team member accessing Zeph via a Telegram bot | Use the agent from mobile or when away from the terminal | CLI-only agents are inaccessible from mobile |

### Secondary Users

| Persona | Description | Primary Goal |
|---------|-------------|-------------|
| **Team Operator** | DevOps / infrastructure engineer deploying Zeph as a shared service | Expose Zeph via HTTP gateway, schedule tasks, monitor Prometheus metrics |
| **Skill Author** | Developer writing SKILL.md files to extend agent behaviour | Teach the agent domain knowledge without touching Rust code |
| **Benchmark Researcher** | ML engineer running Zeph against standard agent benchmarks | Compare memory, skill, and reasoning quality across providers |

### Stakeholders

- **Open-source contributors** — pull quality, project momentum, community trust.
- **Anthropic / OpenAI / Ollama ecosystems** — compatibility with their APIs
  is a dependency, not a requirement to control.

---

## Functional Requirements

> [!tip] Priority Legend
> - **Must** — without this the system is pointless (MVP)
> - **Should** — important but can ship without it
> - **Could** — nice to have

### Agent Core

- **FR-001**: As a CLI developer, I need the agent to receive messages, call
  LLMs, execute tools, and return responses in a single coherent turn loop,
  so that I can have a productive conversation with the agent.
  - *Acceptance criteria*: a user message results in an LLM response within the
    same terminal session; tool calls are dispatched and results appended before
    the final reply.
  - *Priority*: Must

- **FR-002**: As any user, I need the agent to support slash commands
  (`/help`, `/clear`, `/compact`, `/plan`, `/exit`), so that I can control
  agent behaviour without leaving the current channel.
  - *Acceptance criteria*: typing `/help` prints available commands; `/clear`
    resets conversation; `/compact` triggers context compaction.
  - *Priority*: Must

- **FR-003**: As a power user, I need the agent to swap the active LLM provider
  at runtime without restarting, so that I can switch between local and cloud
  models mid-session.
  - *Acceptance criteria*: provider swap via config hot-reload completes without
    dropping the current conversation history.
  - *Priority*: Should

### Multi-Provider LLM Inference

- **FR-010**: As a developer, I need Zeph to support Ollama, Claude (Anthropic),
  OpenAI, OpenAI-compatible, and HuggingFace/candle providers behind a unified
  interface, so that I am not locked to a single vendor.
  - *Acceptance criteria*: each provider passes the standard chat, streaming,
    and tool-call test suite.
  - *Priority*: Must

- **FR-011**: As a team operator, I need a provider pool with routing strategies
  (cascade, cost-weighted, bandit, complexity-tiered), so that expensive models
  are used only for complex tasks.
  - *Acceptance criteria*: a configured `TriageRouter` sends simple queries to
    the fast provider and complex queries to the quality provider, measurable
    via debug logs.
  - *Priority*: Should

- **FR-012**: As any user, I need the agent to support prompt caching where
  the provider supports it, so that repeated system prompts do not incur
  unnecessary token costs.
  - *Priority*: Could

### Skills System

- **FR-020**: As a skill author, I need to define agent skills in plain
  `SKILL.md` files that are loaded and hot-reloaded without restarting the agent,
  so that domain knowledge can be added or updated at runtime.
  - *Acceptance criteria*: adding or editing a SKILL.md file is reflected in the
    active registry within 500ms debounce.
  - *Priority*: Must

- **FR-021**: As a developer, I need skills to be matched to user messages via
  hybrid BM25 + embedding scoring with a configurable disambiguation threshold,
  so that irrelevant skills are not injected.
  - *Acceptance criteria*: with a threshold set to 0.7, a message with < 0.7
    score against all skills results in no skill injection.
  - *Priority*: Must

- **FR-022**: As a skill author, I need a self-learning loop that upgrades skill
  trust scores based on positive/negative feedback signals, so that well-performing
  skills are preferred automatically.
  - *Priority*: Should

### Semantic Memory

- **FR-030**: As any user, I need the agent to persist conversation history in
  SQLite and retrieve semantically similar memories from Qdrant across sessions,
  so that the agent remembers relevant context from past interactions.
  - *Acceptance criteria*: querying the agent about a topic discussed in a
    previous session returns a contextually relevant recalled memory.
  - *Priority*: Must

- **FR-031**: As a developer, I need the agent to detect rising context pressure
  and compact conversation history (soft threshold at ~60%, hard at ~90%),
  so that long sessions do not exhaust the model's context window.
  - *Acceptance criteria*: at 90% context utilisation, compaction fires and
    context length drops below 60%.
  - *Priority*: Must

- **FR-032**: As any user, I need the agent to maintain an entity graph
  (MAGMA typed edges) for BFS-based graph recall, so that factual relationships
  extracted from conversations are reused in future turns.
  - *Priority*: Should

### Multi-Channel I/O

- **FR-040**: As a CLI developer, I need a text-based CLI channel that reads
  from stdin and writes to stdout, so that Zeph can be used in shell pipelines.
  - *Priority*: Must

- **FR-041**: As a power user, I need a ratatui-based TUI channel with real-time
  metrics, context pressure gauge, memory panel, and spinner indicators for all
  background operations, so that I have full situational awareness during a session.
  - *Priority*: Should

- **FR-042**: As a remote user, I need a Telegram channel with streaming output
  support, so that I can interact with Zeph from mobile without a terminal.
  - *Priority*: Should

### Tool Execution

- **FR-050**: As a developer, I need the agent to execute shell commands,
  web scraping, and file operations via a composable tool executor, with a
  blocklist check and optional user-approval gate, so that I control what the
  agent can do autonomously.
  - *Acceptance criteria*: a blocklisted command is rejected before the
    permission policy is consulted; a non-blocklisted command in the "ask first"
    set prompts the user for confirmation.
  - *Priority*: Must

- **FR-051**: As a team operator, I need tool execution audit logging with
  `claim_source` attribution, so that every tool call is traceable to its
  origin.
  - *Priority*: Should

### MCP Integration

- **FR-060**: As a developer, I need Zeph to act as an MCP client connecting
  to one or more MCP servers, discovering their tools semantically, and invoking
  them transparently alongside native tools, so that I can extend Zeph's
  capabilities via the MCP ecosystem.
  - *Priority*: Should

- **FR-061**: As a team operator, I need per-server tool quotas and structured
  error codes from MCP tool calls, so that runaway MCP servers cannot consume
  unlimited resources.
  - *Priority*: Could

### A2A and ACP Protocols

- **FR-070**: As a developer, I need Zeph to implement the A2A (Agent-to-Agent)
  JSON-RPC 2.0 protocol for agent discovery and invocation, so that Zeph can
  participate in multi-agent networks.
  - *Priority*: Could

- **FR-071**: As a developer, I need ACP (Agent Control Protocol) transport
  support with session management and capability advertisement, so that Zeph
  can be controlled or forked by another agent.
  - *Priority*: Could

### Vault and Secrets

- **FR-080**: As any user, I need all secrets (API keys, tokens) to be stored
  exclusively in an age-encrypted vault, never in environment variables or TOML
  config files, so that secrets are not leaked in configuration or logs.
  - *Acceptance criteria*: attempting to set `ZEPH_OPENAI_API_KEY` via env var
    is ignored; the key is resolved only from the age vault.
  - *Priority*: Must

### Gateway and Scheduler

- **FR-090**: As a team operator, I need an HTTP gateway with bearer-token
  authentication for webhook ingestion, so that external systems can send
  events to Zeph without direct terminal access.
  - *Priority*: Could

- **FR-091**: As a team operator, I need a cron-based task scheduler with
  SQLite persistence and CLI management (`schedule list/add/remove`), so that
  periodic agent tasks run unattended.
  - *Priority*: Could

### Code Indexing

- **FR-100**: As a CLI developer, I need AST-based code indexing with semantic
  retrieval and repo-map generation, so that the agent can answer questions
  about the current codebase without manual context injection.
  - *Priority*: Could

### Subagent Lifecycle

- **FR-110**: As a developer, I need to spawn named subagents with scoped tool
  permissions, TTL-based grants, and JSONL transcript persistence via `/agent spawn`,
  so that complex tasks can be delegated to isolated agent instances.
  - *Priority*: Could

---

## Non-Functional Requirements

Detailed targets are in [[NFR]]. High-level constraints for business context:

### Performance

- The CLI channel round-trip (user message → LLM → response displayed) must
  complete within the LLM provider's own latency; no significant overhead added
  by Zeph's routing and memory pipeline.
- The release binary must stay under 15 MiB (current constraint from constitution).

### Security & Privacy

- All secrets managed via age vault; no plaintext credentials anywhere in the
  system.
- Shell command execution protected by a blocklist that runs unconditionally
  before permission policy.
- SSRF protection: private IP ranges rejected in web tool.
- PII detection and redaction in the sanitizer pipeline.

### Availability

- Designed for single-user / small-team use; no high-availability or multi-tenant
  SLA targets in pre-v1.0.
- Graceful degradation: agent operates without memory (no Qdrant) or without
  MCP servers.

### Usability

- All background operations in TUI must have a visible spinner with a descriptive
  status message — no silent background work.
- CLI ergonomics: `/help` lists all available slash commands.

---

## Scope & Boundaries

### In Scope

- Single-binary Rust agent with CLI, TUI, and Telegram channels.
- Multi-provider LLM routing with cost and complexity awareness.
- SKILL.md-based skill system with hot-reload and self-learning.
- Dual-backend memory (SQLite + Qdrant) with graph recall.
- Tool execution with shell, web scraping, and file operations.
- MCP client for third-party tool servers.
- A2A and ACP protocol support (feature-gated).
- Age-encrypted vault for all secrets.
- Optional HTTP gateway, cron scheduler, code indexer, benchmark harness.
- Prometheus metrics export (feature-gated with gateway).
- PostgreSQL database backend (feature-gated alternative to SQLite).

### Out of Scope

> [!danger] Explicit Exclusions
>
> - **Multi-tenant SaaS platform**: Zeph is not a hosted service; no
>   user accounts, billing, or tenant isolation.
> - **Web UI**: there is no browser-based interface; CLI, TUI, and Telegram
>   are the only channels.
> - **Windows support**: the primary supported platforms are macOS and Linux.
> - **Model training or fine-tuning**: Zeph does not train models; it only
>   infers.
> - **Python or Node.js runtime**: Zeph is a single Rust binary; no polyglot
>   runtime dependencies.
> - **Backward compatibility shims before v1.0**: breaking changes are
>   documented in CHANGELOG.md without deprecation warnings.
> - **OpenSSL**: `openssl-sys` is banned; rustls is the only TLS stack.

---

## Integrations & Dependencies

| System | Direction | Data | Status |
|--------|-----------|------|--------|
| Ollama (local) | Outbound | Chat completions, embeddings | Exists |
| Anthropic Claude API | Outbound | Chat completions, tool calls | Exists |
| OpenAI API | Outbound | Chat, tools, embeddings | Exists |
| OpenAI-compatible APIs | Outbound | Chat, embeddings | Exists |
| HuggingFace / candle | Outbound | Local embeddings, inference | Exists |
| Qdrant (local/remote) | Both | Vector store: embeddings, recall | Exists |
| SQLite (embedded) | Both | Conversation history, scheduler, experiments | Exists |
| PostgreSQL (optional) | Both | Alternative to SQLite for teams | Feature-gated |
| Telegram Bot API | Both | Inbound messages, outbound replies | Exists |
| MCP servers (any) | Both | Tool discovery, tool calls | Exists |
| A2A peers | Both | JSON-RPC 2.0 agent invocation | Feature-gated |
| ACP clients | Both | ACP session management | Feature-gated |
| age (encryption) | Both | Vault secret encryption/decryption | Exists |
| Prometheus / OpenMetrics | Outbound | Metrics scraping | Feature-gated |
| OTLP / Jaeger | Outbound | Distributed traces | Feature-gated |
| Pyroscope | Outbound | Continuous profiling | Feature-gated |

---

## Constraints & Assumptions

### Technical Constraints

- Language: Rust 1.94 (MSRV), Edition 2024, no `unsafe` blocks.
- Async: tokio; no `async-trait` crate in library crates.
- TLS: rustls only; `openssl-sys` banned.
- YAML: `serde_norway` only; `serde_yaml` / `serde_yml` banned.
- Database: SQLite (default) or PostgreSQL (opt-in); `sqlx::Any` banned.
- Feature flags: `default = []`; always-on capabilities compiled without flags.
- Binary size: release binary must stay under 15 MiB.
- Unsafe code: `unsafe_code = "deny"` workspace-wide.

### Business Constraints

- Open-source project; no commercial license or paid support tier.
- Pre-v1.0: no backward-compatibility obligation; breaking changes documented.
- No dedicated infrastructure budget; the age vault is the only secret store.
- No fixed team size or deadline; development is community-driven.

> [!warning] Assumptions
> - Users accept that pre-v1.0 releases may have breaking configuration changes.
> - Ollama or at least one cloud provider API key is available for LLM inference.
> - Qdrant is optional; the agent degrades gracefully to SQLite-only memory.
> - Skills are authored by users as SKILL.md files; there is no GUI skill editor.

---

## Success Criteria

- [ ] A developer can install a single binary and start a productive CLI
      session with a local Ollama model within 5 minutes.
- [ ] Skills added as SKILL.md files are active within 500ms without restarting
      the agent.
- [ ] A message discussed in session N is recalled in session N+1 via semantic
      memory with no user-side configuration beyond enabling Qdrant.
- [ ] The TUI shows a spinner for every background operation; no silent waits.
- [ ] All secrets are resolved from the age vault at startup; zero plaintext
      credentials appear in logs or TOML.
- [ ] The release binary stays under 15 MiB.
- [ ] `cargo nextest run --workspace --features full` passes with zero test
      failures on every commit.
- [ ] A team operator can expose Zeph's metrics to Prometheus and receive
      ~25 gauge/counter metrics without code changes.

---

## Open Questions

> [!question] Unresolved Items
>
> - [ ] What is the target v1.0 feature freeze milestone and date?
> - [ ] Is Windows support ever in scope, or permanently out of scope?
> - [ ] Should Zeph publish crates to crates.io, or remain a binary-only
>       distribution?
> - [ ] Will ACP and A2A remain feature-gated in v1.0, or become always-on?
> - [ ] Is there a plan for user documentation (mdBook) alongside the inline
>       rustdoc?

---

## Glossary

| Term | Definition |
|------|-----------|
| Agent | The Zeph runtime instance that handles a user session end-to-end |
| Channel | The I/O boundary abstraction (CLI, TUI, Telegram) |
| Skill | A SKILL.md file defining domain knowledge injected into the system prompt |
| Provider | An LLM backend (Ollama, Claude, OpenAI, candle) |
| Vault | The age-encrypted secret store managed by the `zeph-vault` crate |
| Compaction | The process of summarising old messages to reduce context size |
| MCP | Model Context Protocol — a standard for LLM tool servers |
| A2A | Agent-to-Agent — JSON-RPC 2.0 protocol for inter-agent calls |
| ACP | Agent Control Protocol — session-oriented agent control transport |
| SKILL.md | A Markdown file describing a skill's trigger patterns and instructions |
| BM25 | Sparse text ranking algorithm used in skill matching |
| Qdrant | Open-source vector database used for semantic memory recall |
| age | A modern file encryption tool used for the Zeph vault backend |
| EARS | Easy Approach to Requirements Syntax (WHEN…SHALL notation) |
| TUI | Terminal User Interface (ratatui-based dashboard) |
| DAG | Directed Acyclic Graph — used for multi-step orchestration |

---

## See Also

- [[SRS]] — functional requirements derived from this BRD
- [[NFR]] — non-functional / quality requirements
- [[constitution]] — project-wide non-negotiable principles
- [[MOC-specs]] — index of all technical specifications
- [[001-system-invariants/spec]] — architectural invariants
