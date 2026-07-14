---
aliases:
  - Safe Mode
  - /cd Command
  - Session Working-Directory Switch
  - Customization Isolation Flag
tags:
  - sdd
  - spec
  - cli
  - commands
  - troubleshooting
created: 2026-07-15
status: implemented
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[001-system-invariants/spec]]"
  - "[[047-cli-modes/spec]]"
  - "[[042-zeph-commands/spec]]"
  - "[[028-hooks/spec]]"
  - "[[043-zeph-common/spec]]"
  - "[[003-llm-providers/spec]]"
issues:
  - "#6031"
  - "#6032"
  - "#6207"
---

# Spec: `--safe-mode` Troubleshooting Flag and `/cd` Working-Directory Command

> [!info]
> Backfilled after-the-fact per this project's `/sdd` convention. Both features shipped together
> in commit `9b16183f` (#6207, closing research issues #6031 and #6032) with only ephemeral
> planning docs at `.local/specs/062-safe-mode-troubleshooting-flag/spec.md` and
> `.local/specs/063-mid-session-cd-command/spec.md` — neither is a permanent `/specs/` entry, and
> no line in [[047-cli-modes/spec]] (the closest existing spec, covering `--bare`/`--json`/`-y`/
> `/loop`/`/recap`) mentions either feature. This document is the missing permanent contract,
> derived from the two research specs and the shipped diff (39 files, `9b16183f`).

## Sources

### External
- Claude Code v2.1.205 (2026-07-08) — `--safe-mode` CLI flag / `CLAUDE_CODE_SAFE_MODE` env var:
  disables project context, plugins, skills, hooks, and MCP servers for one session, to isolate
  whether a misbehaving customization is the cause of a problem.
- Claude Code v2.1.206 (2026-07-09) — `/cd` slash command: moves the current session to a new
  working directory mid-conversation without breaking the prompt cache (complements the
  pre-existing `/add-dir`, which only adds a supplementary context directory).

### Internal
| File | Contents |
|---|---|
| `crates/zeph-commands/src/handlers/cd.rs` | `/cd <path>` handler (new) |
| `crates/zeph-commands/src/traits/agent.rs` | Trait surface extension for the `/cd` handler to invoke the shared cwd-change pipeline |
| `crates/zeph-common/src/security.rs` | New shared path-resolution/sandbox-validation module (`allowed_paths` check), used by both `/cd` and the pre-existing `set_working_directory` tool |
| `crates/zeph-tools/src/cwd.rs` | Pre-existing `set_working_directory` tool; extended to route through the new shared `zeph_common::security` validation |
| `crates/zeph-core/src/agent/hooks_dispatch.rs` | `check_cwd_changed`/`cwd_changed` hook pipeline (spec 028); both `/cd` and the LLM-invoked tool converge here |
| `crates/zeph-core/src/agent/agent_access_impl.rs` | `/cd` invalidates the repo-map memo and re-runs `CLAUDE.md`/`AGENTS.md` discovery for the new root |
| `crates/zeph-core/src/context.rs` | System-prompt volatile-block-only rebuild on cwd change (cache-preserving) |
| `crates/zeph-config/src/cli.rs`, `crates/zeph-config/src/env.rs` | `--safe-mode` CLI flag; `ZEPH_SAFE_MODE` env var |
| `crates/zeph-core/src/agent/skill_reload.rs` | Skill hot-reload gated off under `--safe-mode` |
| `src/execution_mode.rs` | `ExecMode` gains the safe-mode flag alongside the pre-existing `bare` flag |
| `src/runner.rs`, `src/daemon.rs`, `src/acp.rs`, `src/serve/*.rs` | All 6 session entry points (runner, daemon, standalone ACP, ACP-HTTP, serve, serve --acp) gated consistently |
| `crates/zeph-tools/src/diagnostics.rs`, `file.rs` | Sandbox path validation reused by the new shared security module |

---

## 1. Overview

### Problem Statement

Zeph had no single-flag way to isolate whether a customization source (`ZEPH.md`/project
context, an installed plugin, a skill, a configured hook, or a connected MCP server) is causing
unwanted behavior — a user had to manually disable each source one at a time. The pre-existing
`--bare` flag ([[047-cli-modes/spec]]) looks superficially similar but solves a different problem:
it is a test-session mode that skips memory/MCP-tool-registry/background-task overhead, and does
**not** gate project-context injection, plugin loading, skill loading, or hook execution.

Separately, Zeph had an *agent-invoked* `set_working_directory` tool
(`crates/zeph-tools/src/cwd.rs`) and a `cwd_changed` hook-dispatch pipeline
([[028-hooks/spec]]), but no **user-facing** command to trigger the same switch directly, and the
existing pipeline did not re-scope the `zeph-index` repo-map or re-run `CLAUDE.md`/`AGENTS.md`
discovery for the new directory, nor did it define an interaction with Claude prompt-cache
breakpoints — a naive full system-prompt rebuild on cwd change would defeat the project's existing
caching investment.

### Goal

1. A `--safe-mode` flag (and `ZEPH_SAFE_MODE` env var) that disables project-context loading,
   plugin loading, skill loading (including hot-reload), hook execution, and MCP server
   connections for a single session — distinct from and composable with `--bare`.
2. A user-facing `/cd <path>` slash command (CLI/TUI/ACP) that reuses the existing
   `set_working_directory`/`check_cwd_changed` pipeline, invalidates the repo-map memo, re-runs
   `CLAUDE.md`/`AGENTS.md` discovery, and preserves the cached system-prompt block across the
   directory change.

### Out of Scope

- Any change to `--bare`'s existing behavior or documented meaning — `--bare` remains the
  test-session-isolation primitive; `--safe-mode` is a distinct, orthogonal, composable flag
  (a user may pass both, e.g. `--bare --safe-mode`).
- Per-customization-source individual disable flags (e.g. `--no-hooks`, `--no-plugins`) as
  separately addressable flags — only the single all-in-one `--safe-mode` flag is in scope.
- Persisting `--safe-mode` as a `config.toml` setting — single-session, non-persistent only,
  mirroring Claude Code's CLI-flag/env-var-only design.
- `/add-dir`-equivalent supplementary-directory support (adding a directory alongside the primary
  one without switching) — noted as related prior art, not designed here.
- Multi-root / multi-workspace simultaneous indexing.

---

## 2. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN `--safe-mode` is passed (or `ZEPH_SAFE_MODE` is set) THE SYSTEM SHALL skip project-context (`ZEPH.md`/`.zeph/config.md`/`CLAUDE.md`/`AGENTS.md`) discovery and injection for the session | must |
| FR-002 | WHEN `--safe-mode` is active THE SYSTEM SHALL skip plugin loading for the session | must |
| FR-003 | WHEN `--safe-mode` is active THE SYSTEM SHALL skip skill loading and matching, including skill hot-reload | must |
| FR-004 | WHEN `--safe-mode` is active THE SYSTEM SHALL skip hook execution (all hook classes) for the session | must |
| FR-005 | WHEN `--safe-mode` is active THE SYSTEM SHALL skip MCP server connections for the session | must |
| FR-006 | WHEN `--safe-mode` is active THE SYSTEM SHALL still run a normal turn loop, LLM provider calls, and (unless `--bare` is also passed) memory and tool execution as usual | must |
| FR-007 | WHEN `--safe-mode` is active THE SYSTEM SHALL apply consistently across all 6 session entry points: runner, daemon, standalone ACP, ACP-HTTP, `serve`, `serve --acp` | must |
| FR-008 | WHEN a user runs `/cd <path>` THE SYSTEM SHALL route through the same `set_working_directory`/`check_cwd_changed` pipeline the LLM-invoked tool already uses — not a parallel implementation | must |
| FR-009 | WHEN `/cd <path>` succeeds THE SYSTEM SHALL invalidate the repo-map memo and re-run `CLAUDE.md`/`AGENTS.md` discovery for the new root | must |
| FR-010 | WHEN `/cd <path>` succeeds on a Claude-backed session THE SYSTEM SHALL rebuild only the volatile system-prompt block, preserving existing `cache_control` breakpoints on the stable/tools blocks | must |
| FR-011 | WHEN `/cd <path>` or `set_working_directory` resolves a target path THE SYSTEM SHALL validate it against the shell sandbox's `allowed_paths` via the shared `zeph_common::security` module | must |
| FR-012 | WHEN the target path for `/cd` does not exist or is not a directory THE SYSTEM SHALL fail safely with a clear error and leave the session's working directory unchanged | must |

---

## 3. Architecture

### 3.1 `--safe-mode`

A new orthogonal flag on `ExecMode` (`src/execution_mode.rs`), independent of the pre-existing
`bare` flag. Gated consistently at all 6 session entry points rather than in a single shared
runner path, since ACP/daemon/serve each construct their own agent-bootstrap sequence (the same
per-entry-point wiring pattern already required for other cross-cutting session flags — see the
recurring "wire X into ACP/serve/daemon" defect class tracked in this project's CI history).

### 3.2 `/cd <path>`

```
/cd <path>  (crates/zeph-commands/src/handlers/cd.rs)
        │
        ▼
zeph_common::security path resolution + allowed_paths validation (FR-011)
        │
        ▼
same set_working_directory / check_cwd_changed pipeline the LLM tool uses (FR-008)
        │
        ├── repo-map memo invalidated (FR-009)
        ├── CLAUDE.md/AGENTS.md discovery re-run for new root (FR-009)
        └── cwd_changed hooks fire (spec 028, unless --safe-mode)
        │
        ▼
next system-prompt rebuild: only the volatile block is regenerated (FR-010)
— CACHE_MARKER_STABLE / CACHE_MARKER_TOOLS breakpoints preserved
```

The command handler does not reimplement path resolution or cwd-mutation logic — it is a thin
user-facing entry point onto the pre-existing agent-invoked pipeline, satisfying the "reuse, don't
duplicate" requirement from the originating research spec.

---

## 4. Key Invariants

### Always (without asking)

- `--safe-mode` and `--bare` are independent and composable — passing one never implies or
  disables the other.
- `--safe-mode` never alters `--bare`'s documented behavior or its existing gate sites.
- `/cd` and `set_working_directory` converge on the same underlying cwd-change pipeline — no
  parallel/duplicate implementation exists.
- A `/cd` directory switch never triggers a full system-prompt rebuild — only the volatile block
  is regenerated, preserving Claude `cache_control` breakpoints on stable/tools blocks (FR-010).
- Every `/cd`/`set_working_directory` path resolution goes through the shared
  `zeph_common::security` sandbox validation — never a raw, unvalidated `set_current_dir`.
- `--safe-mode` is session-only — it is never written to `config.toml` or otherwise persisted.

### Ask First

- Adding individual per-source disable flags (`--no-hooks`, `--no-plugins`, etc.) as a
  complement to the all-in-one `--safe-mode` flag.
- Adding an `/add-dir`-equivalent supplementary-directory command.

### Never

- **NEVER** let `/cd` bypass the shell sandbox's `allowed_paths` validation — every resolution
  goes through `zeph_common::security` (FR-011).
- **NEVER** rebuild the full (stable + tools + volatile) system prompt on a `/cd` switch when the
  provider is Claude — this defeats the prompt-cache economics the feature exists to preserve
  (FR-010).
- **NEVER** gate `--safe-mode` at only a subset of the 6 session entry points — inconsistent
  gating reintroduces exactly the cross-mode divergence class this project's CI process treats as
  a first-class bug (see `.claude/rules/continuous-improvement.md`, Cross-Mode Consistency
  Testing).

---

## 5. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| `--safe-mode` combined with `--bare` | Both apply independently; project-context/plugins/skills/hooks/MCP are skipped (safe-mode) AND memory/MCP-tool-registry/background-tasks are skipped (bare) |
| `/cd` target path does not exist | Command fails with a clear error; session's working directory is unchanged (FR-012) |
| `/cd` target path resolves outside `allowed_paths` | Rejected by the shared sandbox validation before any cwd mutation (FR-011) |
| `/cd` invoked on a non-Claude provider without prompt-cache breakpoints | Directory switch proceeds normally; the cache-preservation behavior (FR-010) is a Claude-specific optimization, not a correctness requirement for other providers |
| LLM calls `set_working_directory` while `--safe-mode` is active | Hooks (`cwd_changed`) are skipped under safe-mode per FR-004; the cwd mutation itself and repo-map/instruction re-scoping still occur, since those are not hook-gated |
| `ZEPH_SAFE_MODE` env var set alongside a config.toml with plugins/hooks/skills configured | Safe-mode wins for the session; nothing in `config.toml` is mutated or migrated |

---

## 6. Success Criteria

- [x] `--safe-mode` gated consistently across runner, daemon, standalone ACP, ACP-HTTP, `serve`,
      `serve --acp` (FR-007)
- [x] `/cd` reuses `set_working_directory`/`check_cwd_changed` — no parallel implementation
      (FR-008)
- [x] Shared `zeph_common::security` sandbox validation used by both `/cd` and
      `set_working_directory` (FR-011)
- [x] `cargo +nightly fmt --check`, `cargo clippy --profile ci ... -D warnings`,
      `cargo nextest run ...` pass (landed in #6207)
- [ ] Live verification that a `/cd` switch on a Claude-backed session preserves the
      `cache_control` breakpoint on the stable/tools blocks (LLM Serialization Gate,
      `.claude/rules/continuous-improvement.md`) — not confirmed via a live session as part of
      this backfill; flagged for the next CI cycle's coverage sweep

---

## 7. Relationship to Existing Specs

| This spec | Existing spec | Relationship |
|-----------|---------------|---------------|
| `--safe-mode` flag, orthogonal to `--bare` | [[047-cli-modes/spec]] | Adds a fourth CLI execution mode alongside `--bare`/`--json`/`-y`; that spec should gain a cross-reference to this one |
| `/cd` slash command | [[042-zeph-commands/spec]] | New handler in the existing `CommandRegistry`/`CommandHandler<Ctx>` object-safe dispatch |
| `cwd_changed` hook reuse, hook suppression under `--safe-mode` | [[028-hooks/spec]] | `/cd` converges on the existing hook-dispatch pipeline; `--safe-mode` adds a new suppression condition to hook firing |
| Shared sandbox path validation | [[043-zeph-common/spec]] | New `zeph_common::security` module, following the crate's "no `zeph-*` peer dependency" boundary |
| Prompt-cache-preserving volatile-block-only rebuild | [[003-llm-providers/spec]] | Extends the existing `cache_control`/`CACHE_MARKER_*` breakpoint mechanism to the `/cd` cwd-change path |

---

## 8. See Also

- [[MOC-specs]] — Map of all specifications
- [[constitution]] — Project-wide principles
- [[047-cli-modes/spec]] — Sibling CLI execution modes (`--bare`, `--json`, `-y`, `/loop`, `/recap`)
- [[042-zeph-commands/spec]] — Slash command registry `/cd` registers into
- [[028-hooks/spec]] — `cwd_changed` hook pipeline reused by `/cd`
- [[043-zeph-common/spec]] — Shared primitives crate hosting the new `security` module
- [[003-llm-providers/spec]] — Prompt caching mechanism this feature's cache-preservation invariant extends
- GitHub issues #6031 (`--safe-mode` research), #6032 (`/cd` research) — both closed by #6207
- `.local/specs/062-safe-mode-troubleshooting-flag/spec.md`, `.local/specs/063-mid-session-cd-command/spec.md` — originating ephemeral research specs this document formalizes into the permanent index
