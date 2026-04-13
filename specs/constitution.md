---
aliases:
  - Project Principles
  - Zeph Constitution
tags:
  - sdd
  - constitution
  - project-wide
created: 2026-04-08
status: permanent
related:
  - "[[MOC-specs]]"
  - "[[001-system-invariants/spec]]"
---

# Project Constitution — Zeph

> Non-negotiable principles governing ALL development in this project.
> Every specification, plan, and task MUST comply with this document.
> Update this file only through explicit team decision.

## I. Architecture

- Cargo workspace (Edition 2024, resolver 3): 24 crates (including `zeph-common`, `zeph-commands`, `zeph-context`), root binary `zeph`
- `zeph-core` orchestrates all crates. Crate dependencies must follow the layered DAG:
  - **Layer 0** (foundation, no zeph-* deps): `zeph-llm`, `zeph-a2a`, `zeph-gateway`, `zeph-scheduler`, `zeph-common`, `zeph-config`, `zeph-vault`, `zeph-db`, `zeph-commands`
  - **Layer 1** (depends on Layer 0): `zeph-context` (→ llm, memory, config, common), `zeph-memory` (→ llm, config, vault, db), `zeph-tools` (→ common, config), `zeph-index` (→ llm, memory)
  - **Layer 2** (depends on Layers 0–1): `zeph-skills` (→ llm, memory, tools), `zeph-mcp` (→ llm, memory, tools, common), `zeph-sanitizer` (→ llm, tools, common)
  - **Layer 3** (orchestrator): `zeph-core` (→ all Layer 0–2 crates)
  - **Layer 4** (consumers): `zeph-channels`, `zeph-tui`, `zeph-acp`, `zeph-orchestration`, `zeph-subagent` (→ core + selective Layer 0–2)
  - **Special** (feature-gated, no mandatory deps): `zeph-bench` (benchmarking), `zeph-experiments` (internal research)
  - Same-layer imports are **prohibited** (e.g., a Layer 1 crate must NOT import another Layer 1 crate)
  - Cross-layer imports are permitted only downward (higher layer → lower layer)
- New functionality requires integration at all applicable points: config, CLI, TUI, `--init` wizard, `--migrate-config`
- Optional capabilities are feature-gated; new optional features require a dedicated feature flag
- All background operations in TUI must have a visible spinner with a descriptive status message
- No blocking I/O in async hot paths; avoid heap allocations in tight loops

## II. Technology Stack

- Language: Rust 1.88 (MSRV), Edition 2024
- Async: tokio + native async traits; no `async-trait` crate for new code in library crates
- HTTP: reqwest 0.13 (rustls, no openssl-sys)
- Database: SQLite (sqlx 0.8) for persistence + Qdrant for semantic search
- TUI: ratatui 0.30 + crossterm 0.29
- Serialization: serde + serde_json; YAML via `serde_norway` (NOT serde_yaml / serde_yml)
- Schema generation: `schemars` (`#[derive(JsonSchema)]`) — no manual schema construction
- Logging: `tracing` crate (not `log`)
- Error handling: `thiserror` in library crates, `anyhow` in application code
- Config: TOML with `ZEPH_*` env overrides; secrets via VaultProvider
- TLS: rustls everywhere; openssl-sys is banned
- Unsafe: `unsafe_code = "deny"` workspace-wide — no exceptions

## III. Testing (NON-NEGOTIABLE)

- All features must have tests that pass before merge
- Unit tests: inline `#[cfg(test)]` modules; use `MockProvider` / `MockChannel` patterns
- Integration tests (require Qdrant): `cargo nextest run -- --ignored`
- Property tests: `proptest` for complex invariants
- Pre-merge check command (MUST use `--features full` to match CI):
  ```
  cargo +nightly fmt --check
  cargo clippy --workspace --features full -- -D warnings
  cargo nextest run --config-file .github/nextest.toml --workspace --features full --lib --bins
  ```
- LLM serialization gate: any PR touching LLM request/response paths requires a live API session test
- Coverage: `cargo-llvm-cov` in CI; no hard minimum but coverage must not regress significantly

## IV. Code Style

- `cargo +nightly fmt` for formatting; clippy pedantic + all = warn, -D warnings
- No `unwrap()` / `expect()` in production code — use `?` and proper error types
- Builders: fluent API with `#[must_use]`, accept `impl Into<String>` for string params
- Public APIs must have doc comments; internal logic comments only where non-obvious
- No emoji in code, comments, commit messages, or PR descriptions
- No redundant comments — only explain cognitively complex blocks
- DRY: check existing implementations via Grep/Glob before creating new ones
- MVP / pre-1.0: implement minimum necessary; no premature abstraction or over-engineering

## V. Security

- `unsafe_code = "deny"` workspace-wide — no exceptions
- Vault secrets must be zeroized on drop (`zeroize` crate)
- All user / external input must be validated and sanitized at system boundaries
- Bearer token comparison via BLAKE3 + `ConstantTimeEq` (subtle crate)
- Shell commands: blocklist check runs unconditionally before permission policy
- No secrets, API keys, or credentials in source code or commits
- SSRF protection: validate redirect chains; reject private IP ranges
- Symlink boundary checks in file-loading paths

## VI. Performance

- No blocking I/O in async hot paths (tokio tasks)
- Avoid unnecessary heap allocations in hot loops
- Prefer `Arc`-based sharing over cloning large structures
- Serialize `ToolDefinition` structs once and cache the result
- Binary size target: keep release binary under 15 MiB

## VII. Simplicity

- Prefer standard library and workspace patterns over new dependencies
- New dependencies require version check via context7 MCP and explicit justification
- No backward-compatibility shims before v1.0.0 — write clean code, document breaking changes in CHANGELOG.md
- Feature flags for optional capabilities; do not pollute the default build
- No worktrees created manually (`git worktree add`) — always use `EnterWorktree` tool

## VIII. Git Workflow

- Branch naming:
  - Features: `feat/m{N}/{issue-number}-{feature-slug}`
  - Bug fixes: `fix/{issue-number}-{short-slug}`
  - Hotfixes: `hotfix/{issue-number}-{short-slug}`
- Commit messages: concise, professional; no emoji, no "co-authored by claude", no AI attribution
- One logical change per commit
- Sync with main before pushing: `git fetch origin main && git merge origin/main`
- Update `CHANGELOG.md` (`[Unreleased]`) at end of each implementation phase
- Update `docs/src/` for user-facing changes; update `README.md` / crate READMEs if project-level features change

## IX. Agent Boundaries

### Always (without asking)
- Run `cargo nextest` after changes
- Follow existing code style and patterns
- Add doc comments to new public APIs
- Update `CHANGELOG.md` when completing a phase

### Ask First
- Adding new external dependencies
- Changing database schema (SQLite or Qdrant index structure)
- Adding or removing feature flags
- Changing public API signatures in library crates
- Modifying CI/CD workflows
- Introducing new optional features or transports

### Never
- Commit secrets, API keys, or credentials
- Use `openssl-sys` or `serde_yaml` / `serde_yml`
- Use `unsafe` blocks
- Create git worktrees with `git worktree add` directly
- Add emoji to code, commits, or documentation
- Skip `--features full` in pre-merge checks
- Make code changes during a continuous improvement / testing session
