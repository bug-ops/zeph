---
aliases:
  - Worktree Subsystem BRD
  - BRD-063
tags:
  - sdd
  - brd
  - worktree
created: 2026-05-29
status: approved
related:
  - "[[specs/063-worktree-subsystem/srs]]"
  - "[[specs/063-worktree-subsystem/spec]]"
  - "[[044-subagent-lifecycle/spec]]"
---

# BRD-063: Worktree Subsystem + `worktree.base_ref`

## 1. Business Context

Zeph's subagents today share the parent agent's working directory. When a subagent performs file edits,
shell commands, or code generation, those changes immediately affect the repository that the parent is
also operating in. This creates two problems:

1. **Contamination risk** — a failing or incorrect subagent modifies the parent's working tree.
2. **Lack of isolation** — multiple subagents running concurrently may step on each other's filesystem
   changes, producing unpredictable results.

Claude Code (the primary parity target, issue #3918 — no `parity-claude-code-3918/spec.md` exists
under `/specs/` as of the 2026-07 audit; likely a `.local/`-scoped working doc) provides
`worktree.baseRef` semantics that give each agent an isolated git worktree. Zeph deferred this
capability in the parity spec (rows 40–41) because no worktree subsystem existed. Issue #4655 delivers
that subsystem as a prerequisite for the deferred parity items.

## 2. Stakeholders

| Stakeholder | Interest |
|---|---|
| Agent operators (developers using Zeph) | Agents that write code operate in isolation without contaminating the main branch |
| Zeph core contributors | Clean architecture: git I/O lives in a dedicated crate, not scattered across tools |
| Security-conscious users | Subagent blast radius is bounded to its worktree branch |

## 3. Business Goals

**BG-1.** Provide per-subagent git worktree isolation so that file-system operations performed by a
subagent do not affect the parent working tree or other subagents.

**BG-2.** Deliver a `worktree.base_ref` config knob (`fresh | head`) controlling whether a subagent
branches from the pristine remote default (`origin/<default_branch>`) or from the parent's current
local `HEAD`.

**BG-3.** Keep the feature entirely opt-in (`worktree.enabled = false` default) so existing deployments
are unaffected.

**BG-4.** Establish the infrastructure foundation for the deferred parity items: `worktree.bgIsolation`
(child-process isolation, issue #4656) and full concurrent worktree execution.

## 4. Scope

### In Scope

- New `zeph-worktree` crate: `WorktreeManager`, `WorktreeHandle`, `WorktreeError`, `GitRunner` trait.
- Config types in `zeph-config`: `WorktreeConfig`, `WorktreeBaseRef`.
- Integration with `zeph-subagent`: per-agent opt-in via `permissions.worktree`, serialised execution
  under `CwdGuard` mutex, `set_working_directory` disabled for worktree agents.
- Bootstrap integration in binary: capability probe, manager construction.
- CLI: `zeph worktree list`, `zeph worktree clean`.
- TUI: command palette entry.
- `--init` wizard option; `--migrate-config` step 53.
- Live-testing playbook and coverage-status rows.

### Out of Scope (deferred)

- `bgIsolation` / child-process isolation — deferred to issue #4656.
- True concurrent worktree agent execution — deferred to cwd-threading or bgIsolation PR.
- Merge/rebase orchestration between worktree and main branch.
- Conflict resolution tooling.
- Embedded `git2` / `gix` library — shell-out only.

## 5. Constraints

| Constraint | Rationale |
|---|---|
| `worktree.enabled = false` by default | Zero impact on existing users |
| Shell out to `git` (no `git2`/`gix`) | Matches codebase convention; no new heavy deps |
| MVP: concurrency-1 for worktree agents | In-process agents share process cwd; safe isolation requires serialisation |
| All secrets resolved from age vault | Vault-first policy (CLAUDE.md) |

## 6. Success Criteria

| Criterion | Measurable Target |
|---|---|
| Worktree created for opted-in subagent | `git worktree list` shows the worktree after spawn |
| `base_ref = fresh` produces a branch at `origin/<default>` | Resolved commit = `git rev-parse origin/<default>` |
| `base_ref = head` produces a branch at parent HEAD | Resolved commit = `git rev-parse HEAD` at spawn time |
| CwdGuard serialises concurrent worktree agent | Second worktree agent blocks until first releases guard |
| Worktree removed on completion | `git worktree list` no longer shows it after cleanup |
| Non-worktree agents unaffected | Existing test suite passes with `worktree.enabled = false` |
| Startup capability probe catches missing git | Clear error at bootstrap, not at first spawn |

## 7. Risks

| Risk | Likelihood | Mitigation |
|---|---|---|
| `git fetch` latency blocks spawn (fresh mode) | Medium | Single-attempt with await-discipline timeout; clear error |
| `origin/HEAD` symref unset in CI/shallow clones | Medium | `config.default_branch` escape hatch; error `BaseRefUnresolved` |
| Worktree leak on crash | Low | `worktree clean` CLI op; startup reconciler via `git worktree list --porcelain` |
| User confusion: uncommitted changes absent in worktree | Medium | Runtime dirty-tree warning; docs |
